//! Test implementations of the reference component, and small single-method
//! components expanded by hand the way `#[component]` expands a `local_only`
//! component (section 3.5 of the C1 spec).
#![allow(dead_code, missing_docs)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use sekvent_component::__private::{BoxFuture, Bytes, Dispatch};
use sekvent_component::{App, AppBuilder, AppError, Binding, BuildError, CallContext, Lifecycle};
use sekvent_config::MapSource;
use tokio::sync::{Semaphore, mpsc};

use super::inventory::{
    Inventory, InventoryError, InventoryHandle, ReleaseReply, ReleaseRequest, ReserveReply,
    ReserveRequest,
};

/// What the implementation saw of one call's context.
#[derive(Debug, Clone)]
pub struct Seen {
    pub request_id: String,
    pub subject: Option<String>,
    pub tenant: Option<String>,
    pub idempotency_key: Option<String>,
    pub caller: Option<(String, bool)>,
    pub deadline: Option<Instant>,
    pub traceparent: Option<String>,
    pub hops: u32,
}

/// Every context an implementation saw, in call order.
#[derive(Debug, Clone, Default)]
pub struct Probe(Arc<Mutex<Vec<Seen>>>);

impl Probe {
    fn record(&self, cx: &CallContext) {
        let seen = Seen {
            request_id: cx.request_id().to_owned(),
            subject: cx.subject().map(str::to_owned),
            tenant: cx.tenant().map(str::to_owned),
            idempotency_key: cx.idempotency_key().map(str::to_owned),
            caller: cx
                .caller()
                .map(|caller| (caller.name.clone(), caller.trusted)),
            deadline: cx.deadline(),
            traceparent: cx.traceparent().map(str::to_owned),
            hops: cx.hops(),
        };
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(seen);
    }

    pub fn calls(&self) -> Vec<Seen> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Sends on a channel when dropped: tells a test that a call's future was
/// dropped (or its task aborted).
pub struct DropSignal(pub mpsc::UnboundedSender<()>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// How [`FakeInventory::reserve`] behaves.
pub enum Behaviour {
    /// Real stock: reserve decrements it or fails `OutOfStock`.
    Stock(Mutex<u32>),
    /// Report entry, then wait for a permit.
    Gated {
        entered: mpsc::UnboundedSender<()>,
        permits: Arc<Semaphore>,
    },
    /// Report entry, hold a [`DropSignal`], never complete.
    Pending {
        entered: mpsc::UnboundedSender<()>,
        dropped: mpsc::UnboundedSender<()>,
    },
    /// Hand the call's cancellation token to a task of its own, which
    /// reports on `cancelled` once the token fires; then report entry and
    /// never complete.
    WatchCancel {
        entered: mpsc::UnboundedSender<()>,
        cancelled: mpsc::UnboundedSender<()>,
    },
    /// Panic.
    Panic,
    /// Fail with this error.
    Fail(fn() -> InventoryError),
}

/// A configurable [`Inventory`].
pub struct FakeInventory {
    pub probe: Probe,
    pub behaviour: Behaviour,
}

impl FakeInventory {
    pub fn new(behaviour: Behaviour) -> Self {
        Self {
            probe: Probe::default(),
            behaviour,
        }
    }

    pub fn stock(available: u32) -> Self {
        Self::new(Behaviour::Stock(Mutex::new(available)))
    }
}

impl Inventory for FakeInventory {
    async fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        self.probe.record(cx);
        match &self.behaviour {
            Behaviour::Stock(stock) => {
                let mut stock = stock.lock().unwrap_or_else(PoisonError::into_inner);
                if req.quantity > *stock {
                    return Err(InventoryError::OutOfStock {
                        sku: req.sku,
                        available: *stock,
                    });
                }
                *stock -= req.quantity;
                Ok(ReserveReply {
                    reservation_id: format!("res-{}", req.order_id),
                    remaining: *stock,
                })
            }
            Behaviour::Gated { entered, permits } => {
                entered.send(()).unwrap();
                permits.acquire().await.unwrap().forget();
                Ok(ReserveReply {
                    reservation_id: format!("res-{}", req.order_id),
                    remaining: 0,
                })
            }
            Behaviour::Pending { entered, dropped } => {
                let _signal = DropSignal(dropped.clone());
                entered.send(()).unwrap();
                std::future::pending::<Result<ReserveReply, InventoryError>>().await
            }
            Behaviour::WatchCancel { entered, cancelled } => {
                let token = cx.cancel_token().clone();
                let cancelled = cancelled.clone();
                tokio::spawn(async move {
                    token.cancelled().await;
                    let _ = cancelled.send(());
                });
                entered.send(()).unwrap();
                std::future::pending::<Result<ReserveReply, InventoryError>>().await
            }
            Behaviour::Panic => panic!("the fake inventory panicked on purpose"),
            Behaviour::Fail(error) => Err(error()),
        }
    }

    async fn release(
        &self,
        cx: &CallContext,
        req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        self.probe.record(cx);
        match req.reservation_id.as_str() {
            "" => Err(InventoryError::ReservationNotFound {
                reservation_id: String::new(),
                hint: None,
            }),
            id if id.starts_with("res-") => Ok(ReleaseReply { released: true }),
            id => Err(InventoryError::ReservationNotFound {
                reservation_id: id.to_owned(),
                hint: Some("ids start with res-".to_owned()),
            }),
        }
    }
}

/// Fails the first `failures` calls of `method` with `error()`, then serves
/// from real stock. Records every call that reaches it.
pub struct FlakyInventory {
    pub probe: Probe,
    pub method: &'static str,
    pub failures: Mutex<u32>,
    pub error: fn() -> AppError,
    pub stock: FakeInventory,
}

impl FlakyInventory {
    pub fn new(method: &'static str, failures: u32, error: fn() -> AppError) -> Self {
        Self {
            probe: Probe::default(),
            method,
            failures: Mutex::new(failures),
            error,
            stock: FakeInventory::stock(100),
        }
    }

    fn fail(&self, cx: &CallContext, method: &str) -> Result<(), InventoryError> {
        self.probe.record(cx);
        let mut left = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if method == self.method && *left > 0 {
            *left -= 1;
            return Err(InventoryError::Other((self.error)()));
        }
        Ok(())
    }
}

impl Inventory for FlakyInventory {
    async fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        self.fail(cx, "reserve")?;
        self.stock.reserve(cx, req).await
    }

    async fn release(
        &self,
        cx: &CallContext,
        req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        self.fail(cx, "release")?;
        self.stock.release(cx, req).await
    }
}

/// Serves the inventory by calling another inventory through `next` (a
/// handle of a different App), recording every call that reaches it.
pub struct RelayInventory {
    pub probe: Probe,
    pub next: InventoryHandle,
}

impl Inventory for RelayInventory {
    async fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        self.probe.record(cx);
        self.next.reserve(cx, req).await
    }

    async fn release(
        &self,
        cx: &CallContext,
        req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        self.probe.record(cx);
        self.next.release(cx, req).await
    }
}

/// Records lifecycle events of several components in one list.
#[derive(Debug, Clone, Default)]
pub struct Recorder(Arc<Mutex<Vec<String>>>);

impl Recorder {
    pub fn push(&self, event: impl Into<String>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event.into());
    }

    pub fn events(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// An [`Inventory`] (stock 10) with lifecycle hooks.
pub struct HookedInventory {
    pub inner: FakeInventory,
    pub hooks: Hooks,
}

/// Lifecycle hooks that record into a [`Recorder`] and can be made to fail.
pub struct Hooks {
    pub name: &'static str,
    pub recorder: Recorder,
    pub fail_start: bool,
    pub fail_stop: bool,
}

impl Hooks {
    pub fn new(name: &'static str, recorder: &Recorder) -> Self {
        Self {
            name,
            recorder: recorder.clone(),
            fail_start: false,
            fail_stop: false,
        }
    }

    fn start(&self) -> Result<(), AppError> {
        self.recorder.push(format!("start {}", self.name));
        if self.fail_start {
            return Err(AppError::unavailable(format!("{} cannot start", self.name)));
        }
        Ok(())
    }

    fn stop(&self) -> Result<(), AppError> {
        self.recorder.push(format!("stop {}", self.name));
        if self.fail_stop {
            return Err(AppError::internal(std::io::Error::other("stop failed")));
        }
        Ok(())
    }
}

impl Inventory for HookedInventory {
    async fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        self.inner.reserve(cx, req).await
    }

    async fn release(
        &self,
        cx: &CallContext,
        req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        self.inner.release(cx, req).await
    }
}

impl Lifecycle for HookedInventory {
    async fn on_start(&self) -> Result<(), AppError> {
        self.hooks.start()
    }

    async fn on_stop(&self) -> Result<(), AppError> {
        self.hooks.stop()
    }
}

/// A dispatcher whose replies are not valid messages.
pub struct GarbageDispatch;

impl Dispatch for GarbageDispatch {
    fn dispatch(
        &self,
        _method: usize,
        _cx: CallContext,
        _body: Bytes,
    ) -> BoxFuture<'static, Result<Bytes, AppError>> {
        Box::pin(async { Ok(Bytes::from_static(&[0x0a, 0xff, 0xff])) })
    }
}

/// Hand-expands a component with one method, `add(&self, cx, String) ->
/// Result<usize, AppError>`, in the given mode; the install functions attach
/// the given dispatcher (if any).
macro_rules! single_method_component {
    ($trait:ident, $dyn:ident, $handle:ident, $name:literal, $mode:ident, $dispatch:expr) => {
        pub trait $trait: Send + Sync + 'static {
            fn add(
                &self,
                cx: &CallContext,
                req: String,
            ) -> impl ::core::future::Future<Output = Result<usize, AppError>> + ::core::marker::Send;
        }

        #[doc(hidden)]
        pub trait $dyn: ::core::marker::Send + ::core::marker::Sync + 'static {
            fn __add<'a>(
                &'a self,
                cx: &'a CallContext,
                req: String,
            ) -> ::sekvent_component::__private::BoxFuture<'a, Result<usize, AppError>>;
        }

        impl<T: $trait> $dyn for T {
            fn __add<'a>(
                &'a self,
                cx: &'a CallContext,
                req: String,
            ) -> ::sekvent_component::__private::BoxFuture<'a, Result<usize, AppError>> {
                ::std::boxed::Box::pin(<T as $trait>::add(self, cx, req))
            }
        }

        #[derive(Clone, Debug)]
        pub struct $handle(::sekvent_component::__private::Endpoint<dyn $dyn>);

        impl $handle {
            pub const __METHODS: &'static [::sekvent_component::MethodDescriptor] =
                &[::sekvent_component::MethodDescriptor::call("add", "Add")];

            pub fn add<'a>(
                &'a self,
                cx: &'a CallContext,
                req: String,
            ) -> impl ::core::future::Future<Output = Result<usize, AppError>>
            + ::core::marker::Send
            + 'a {
                self.0.call_local(
                    0usize,
                    cx,
                    req,
                    |imp: ::std::sync::Arc<dyn $dyn>,
                     cx: ::sekvent_component::CallContext,
                     req: String| async move { $dyn::__add(&*imp, &cx, req).await },
                )
            }

            pub fn binding(&self) -> ::sekvent_component::Binding {
                self.0.binding()
            }

            fn local(imp: ::std::sync::Arc<dyn $dyn>) -> ::sekvent_component::__private::Local<dyn $dyn> {
                let local = ::sekvent_component::__private::Local::new(imp);
                let dispatch: Option<::std::sync::Arc<dyn Dispatch>> = $dispatch;
                match dispatch {
                    Some(dispatch) => local.with_dispatch(dispatch),
                    None => local,
                }
            }

            pub fn install<T, F>(
                app: &mut ::sekvent_component::AppBuilder<'_>,
                factory: F,
            ) -> ::core::result::Result<(), ::sekvent_component::BuildError>
            where
                T: $trait,
                F: ::core::ops::FnOnce(
                        &mut ::sekvent_component::Deps<'_>,
                    ) -> ::core::result::Result<T, ::sekvent_component::AppError>
                    + ::core::marker::Send
                    + 'static,
            {
                ::sekvent_component::__private::install_local(app, $handle, move |deps| {
                    let imp: ::std::sync::Arc<dyn $dyn> = ::std::sync::Arc::new(factory(deps)?);
                    ::core::result::Result::Ok(Self::local(imp))
                })
            }

            pub fn install_with_lifecycle<T, F>(
                app: &mut ::sekvent_component::AppBuilder<'_>,
                factory: F,
            ) -> ::core::result::Result<(), ::sekvent_component::BuildError>
            where
                T: $trait + ::sekvent_component::Lifecycle,
                F: ::core::ops::FnOnce(
                        &mut ::sekvent_component::Deps<'_>,
                    ) -> ::core::result::Result<T, ::sekvent_component::AppError>
                    + ::core::marker::Send
                    + 'static,
            {
                ::sekvent_component::__private::install_local(app, $handle, move |deps| {
                    let concrete = ::std::sync::Arc::new(factory(deps)?);
                    let imp: ::std::sync::Arc<dyn $dyn> = ::std::sync::Arc::<T>::clone(&concrete);
                    ::core::result::Result::Ok(Self::local(imp).with_lifecycle(concrete))
                })
            }

            pub fn install_remote(
                app: &mut ::sekvent_component::AppBuilder<'_>,
            ) -> ::core::result::Result<(), ::sekvent_component::BuildError> {
                ::sekvent_component::__private::install_remote(app, $handle)
            }
        }

        impl ::sekvent_component::ComponentHandle for $handle {
            const DESCRIPTOR: &'static ::sekvent_component::ComponentDescriptor =
                &::sekvent_component::ComponentDescriptor::new(
                    $name,
                    stringify!($trait),
                    $handle::__METHODS,
                )
                .with_mode(::sekvent_component::ComponentMode::$mode);
        }

        const _: () = {
            ::sekvent_component::__private::assert_local::<String>();
            ::sekvent_component::__private::assert_local::<usize>();
            ::sekvent_component::__private::assert_error::<AppError>();
        };
    };
}

single_method_component!(Notes, __NotesDyn, NotesHandle, "notes", LocalOnly, None);
single_method_component!(Audit, __AuditDyn, AuditHandle, "audit", LocalOnly, None);
single_method_component!(Jotter, __JotterDyn, JotterHandle, "notes", LocalOnly, None);
single_method_component!(
    Ledger,
    __LedgerDyn,
    LedgerHandle,
    "ledger",
    RemoteOnly,
    None
);
single_method_component!(Bare, __BareDyn, BareHandle, "bare", Standard, None);
single_method_component!(
    Odd,
    __OddDyn,
    OddHandle,
    "odd",
    Standard,
    Some(::std::sync::Arc::new(GarbageDispatch))
);

/// Counts the characters it is given; optionally has lifecycle hooks.
pub struct Counter {
    pub hooks: Option<Hooks>,
    pub probe: Probe,
}

impl Counter {
    pub fn plain() -> Self {
        Self {
            hooks: None,
            probe: Probe::default(),
        }
    }

    pub fn hooked(hooks: Hooks) -> Self {
        Self {
            hooks: Some(hooks),
            probe: Probe::default(),
        }
    }

    fn count(&self, cx: &CallContext, req: &str) -> Result<usize, AppError> {
        self.probe.record(cx);
        if req.is_empty() {
            return Err(AppError::invalid_argument("nothing to count").with_reason("EMPTY_TEXT"));
        }
        Ok(req.chars().count())
    }
}

impl Lifecycle for Counter {
    async fn on_start(&self) -> Result<(), AppError> {
        match &self.hooks {
            Some(hooks) => hooks.start(),
            None => Ok(()),
        }
    }

    async fn on_stop(&self) -> Result<(), AppError> {
        match &self.hooks {
            Some(hooks) => hooks.stop(),
            None => Ok(()),
        }
    }
}

impl Notes for Counter {
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError> {
        self.count(cx, &req)
    }
}

impl Audit for Counter {
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError> {
        self.count(cx, &req)
    }
}

impl Jotter for Counter {
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError> {
        self.count(cx, &req)
    }
}

impl Ledger for Counter {
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError> {
        self.count(cx, &req)
    }
}

impl Bare for Counter {
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError> {
        self.count(cx, &req)
    }
}

impl Odd for Counter {
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError> {
        self.count(cx, &req)
    }
}

/// A source selecting `binding` for every standard component, plus `pairs`.
pub fn source(binding: Binding, pairs: &[(&str, &str)]) -> MapSource {
    let mut source: MapSource = pairs.iter().copied().collect();
    source.set(sekvent_component::DEFAULT_BINDING_KEY, binding.as_str());
    source
}

/// Build (not start) an App with `inventory` installed.
pub fn build_inventory(
    binding: Binding,
    pairs: &[(&str, &str)],
    inventory: FakeInventory,
) -> Result<App, BuildError> {
    let config = source(binding, pairs);
    let mut builder = App::builder(&config);
    InventoryHandle::install(&mut builder, move |_| Ok(inventory))?;
    builder.build()
}

/// Build and start an App with `inventory` installed; its handle.
pub async fn started_inventory(
    binding: Binding,
    pairs: &[(&str, &str)],
    inventory: FakeInventory,
) -> (App, InventoryHandle) {
    let app = build_inventory(binding, pairs, inventory).unwrap();
    app.start().await.unwrap();
    let handle = app.handle::<InventoryHandle>().unwrap();
    assert_eq!(handle.binding(), binding);
    (app, handle)
}

/// Both in-process bindings.
pub const LOCAL_BINDINGS: [Binding; 2] = [Binding::Local, Binding::LocalSerialized];

/// Install helper so tests can pass a closure that installs components.
pub fn build_with(
    source: &MapSource,
    install: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> Result<App, BuildError> {
    let mut builder = App::builder(source);
    install(&mut builder)?;
    builder.build()
}

/// A deadline `after` from now on tokio's clock.
pub fn deadline_in(after: Duration) -> Instant {
    (tokio::time::Instant::now() + after).into_std()
}

/// A `notes` implementation whose `on_start` reports entry and then waits
/// for a permit, so a test can stop the App while it is starting.
pub struct SlowStart {
    pub entered: mpsc::UnboundedSender<()>,
    pub permits: Arc<Semaphore>,
    pub recorder: Recorder,
}

impl Notes for SlowStart {
    async fn add(&self, _cx: &CallContext, req: String) -> Result<usize, AppError> {
        Ok(req.len())
    }
}

impl Lifecycle for SlowStart {
    async fn on_start(&self) -> Result<(), AppError> {
        self.recorder.push("start notes");
        self.entered.send(()).unwrap();
        self.permits.acquire().await.unwrap().forget();
        Ok(())
    }

    async fn on_stop(&self) -> Result<(), AppError> {
        self.recorder.push("stop notes");
        Ok(())
    }
}
