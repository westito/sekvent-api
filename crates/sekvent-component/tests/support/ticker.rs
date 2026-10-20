//! The streaming reference component of the streaming note (section 11.3),
//! expanded by hand exactly as `#[component]` expands it, with hand-written
//! prost messages, a hand-written service contract (the 5-tuple constant
//! and its aliases) and a hand-expanded `#[derive(ComponentError)]`, so the
//! framework's streams are tested without the macro.
#![allow(dead_code, missing_docs)]

use sekvent_component::{AppError, CallContext, CallStream};

// ---------------------------------------------------------------------------
// Messages (what `sekvent-proto-build` would generate for shop.ticker.v1).
// ---------------------------------------------------------------------------

/// Ticks of a series, from `from` on.
#[derive(Clone, PartialEq, prost::Message)]
pub struct TicksRequest {
    #[prost(string, tag = "1")]
    pub series: String,
    #[prost(uint64, tag = "2")]
    pub from: u64,
}

/// The latest tick of a series.
#[derive(Clone, PartialEq, prost::Message)]
pub struct LatestRequest {
    #[prost(string, tag = "1")]
    pub series: String,
}

/// One tick.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Tick {
    #[prost(string, tag = "1")]
    pub series: String,
    #[prost(uint64, tag = "2")]
    pub seq: u64,
}

impl prost::Name for TicksRequest {
    const NAME: &'static str = "TicksRequest";
    const PACKAGE: &'static str = "shop.ticker.v1";
}

impl prost::Name for LatestRequest {
    const NAME: &'static str = "LatestRequest";
    const PACKAGE: &'static str = "shop.ticker.v1";
}

impl prost::Name for Tick {
    const NAME: &'static str = "Tick";
    const PACKAGE: &'static str = "shop.ticker.v1";
}

/// What `sekvent-proto-build` would generate for the package's service.
pub mod proto {
    /// Contract of `shop.ticker.v1.Ticker`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_upper_case_globals, dead_code, clippy::type_complexity)]
    pub const __sekvent_service_Ticker: (&str, &[(&str, &str, &str, bool, bool)]) = (
        "shop.ticker.v1.Ticker",
        &[
            (
                "Ticks",
                "shop.ticker.v1.TicksRequest",
                "shop.ticker.v1.Tick",
                false,
                true,
            ),
            (
                "Latest",
                "shop.ticker.v1.LatestRequest",
                "shop.ticker.v1.Tick",
                false,
                false,
            ),
        ],
    );
    /// Request and reply of `shop.ticker.v1.Ticker.Ticks`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Ticker__Ticks = (super::TicksRequest, super::Tick);
    /// Request and reply of `shop.ticker.v1.Ticker.Latest`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Ticker__Latest = (super::LatestRequest, super::Tick);
}

// ---------------------------------------------------------------------------
// #[derive(Debug, ComponentError)]
// #[component_error(domain = "shop.ticker.v1")]
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum TickerError {
    UnknownSeries { series: String },
    Gap { missing: u64 },
    Other(AppError),
}

const _: () = {
    ::sekvent_component::__private::assert_metadata::<String>();
    ::sekvent_component::__private::assert_metadata::<u64>();
};

#[automatically_derived]
#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::ComponentError for TickerError {
    fn into_app_error(self) -> ::sekvent_component::AppError {
        match self {
            Self::UnknownSeries { series } => ::sekvent_component::AppError::new(
                ::sekvent_component::ErrorCode::NotFound,
                ::std::format!("no series {series}"),
            )
            .with_reason("UNKNOWN_SERIES")
            .with_domain("shop.ticker.v1")
            .with_metadata("series", ::std::string::ToString::to_string(&series)),
            Self::Gap { missing } => ::sekvent_component::AppError::new(
                ::sekvent_component::ErrorCode::FailedPrecondition,
                ::std::format!("{missing} ticks were lost"),
            )
            .with_reason("SERIES_GAP")
            .with_domain("shop.ticker.v1")
            .with_metadata("missing", ::std::string::ToString::to_string(&missing)),
            Self::Other(error) => error,
        }
    }

    fn from_app_error(error: ::sekvent_component::AppError) -> Self {
        if ::sekvent_component::__private::matches(
            &error,
            "UNKNOWN_SERIES",
            ::core::option::Option::Some("shop.ticker.v1"),
        ) {
            match (::sekvent_component::__private::field::<String>(
                &error, "series",
            ),)
            {
                (::core::option::Option::Some(series),) => {
                    return Self::UnknownSeries { series };
                }
                _ => {}
            }
        }
        if ::sekvent_component::__private::matches(
            &error,
            "SERIES_GAP",
            ::core::option::Option::Some("shop.ticker.v1"),
        ) {
            match (::sekvent_component::__private::field::<u64>(
                &error, "missing",
            ),)
            {
                (::core::option::Option::Some(missing),) => {
                    return Self::Gap { missing };
                }
                _ => {}
            }
        }
        Self::Other(error)
    }
}

#[automatically_derived]
impl ::core::convert::From<::sekvent_component::AppError> for TickerError {
    fn from(error: ::sekvent_component::AppError) -> Self {
        <Self as ::sekvent_component::ComponentError>::from_app_error(error)
    }
}

#[automatically_derived]
impl ::core::convert::From<TickerError> for ::sekvent_component::AppError {
    fn from(error: TickerError) -> Self {
        <TickerError as ::sekvent_component::ComponentError>::into_app_error(error)
    }
}

// ---------------------------------------------------------------------------
// #[sekvent::component(
//     name = "ticker",
//     package = "shop.ticker.v1",
//     proto = "crate::support::ticker::proto"
// )]
// pub trait Ticker: Send + Sync + 'static {
//     #[call(stream, idempotent, timeout = "1s", bulkhead = 8, max_lifetime = "10m")]
//     async fn ticks(&self, cx: &CallContext, req: TicksRequest)
//         -> Result<CallStream<Tick, TickerError>, TickerError>;
//     #[call(idempotent, timeout = "500ms")]
//     async fn latest(&self, cx: &CallContext, req: LatestRequest) -> Result<Tick, TickerError>;
// }
// ---------------------------------------------------------------------------

pub trait Ticker: Send + Sync + 'static {
    /// Ticks of a series, from `from` on, as they happen.
    fn ticks(
        &self,
        cx: &CallContext,
        req: TicksRequest,
    ) -> impl ::core::future::Future<Output = Result<CallStream<Tick, TickerError>, TickerError>>
    + ::core::marker::Send;
    /// The latest tick of a series.
    fn latest(
        &self,
        cx: &CallContext,
        req: LatestRequest,
    ) -> impl ::core::future::Future<Output = Result<Tick, TickerError>> + ::core::marker::Send;
}

#[doc(hidden)]
pub trait __TickerDyn: ::core::marker::Send + ::core::marker::Sync + 'static {
    fn __ticks<'a>(
        &'a self,
        cx: &'a CallContext,
        req: TicksRequest,
    ) -> ::sekvent_component::__private::BoxFuture<
        'a,
        Result<CallStream<Tick, TickerError>, TickerError>,
    >;
    fn __latest<'a>(
        &'a self,
        cx: &'a CallContext,
        req: LatestRequest,
    ) -> ::sekvent_component::__private::BoxFuture<'a, Result<Tick, TickerError>>;
}

#[allow(clippy::all, clippy::pedantic)]
impl<__SekventImpl: Ticker> __TickerDyn for __SekventImpl {
    fn __ticks<'a>(
        &'a self,
        cx: &'a CallContext,
        req: TicksRequest,
    ) -> ::sekvent_component::__private::BoxFuture<
        'a,
        Result<CallStream<Tick, TickerError>, TickerError>,
    > {
        ::std::boxed::Box::pin(<__SekventImpl as Ticker>::ticks(self, cx, req))
    }
    fn __latest<'a>(
        &'a self,
        cx: &'a CallContext,
        req: LatestRequest,
    ) -> ::sekvent_component::__private::BoxFuture<'a, Result<Tick, TickerError>> {
        ::std::boxed::Box::pin(<__SekventImpl as Ticker>::latest(self, cx, req))
    }
}

/// Cloneable handle to the [`Ticker`] component (`ticker`).
///
/// Get it from `Deps::handle` in a factory or from `App::handle`; the App
/// builder decides whether its calls run in-process or across a
/// serialization boundary.
#[derive(Clone, Debug)]
pub struct TickerHandle(::sekvent_component::__private::Endpoint<dyn __TickerDyn>);

#[allow(clippy::all, clippy::pedantic)]
impl TickerHandle {
    #[doc(hidden)]
    pub const __METHODS: &'static [::sekvent_component::MethodDescriptor] = &[
        ::sekvent_component::MethodDescriptor::call("ticks", "Ticks")
            .with_stream()
            .with_idempotent()
            .with_timeout(::core::time::Duration::new(1u64, 0u32))
            .with_bulkhead(8u32)
            .with_max_lifetime(::core::time::Duration::new(600u64, 0u32)),
        ::sekvent_component::MethodDescriptor::call("latest", "Latest")
            .with_idempotent()
            .with_timeout(::core::time::Duration::new(0u64, 500000000u32)),
    ];

    /// Ticks of a series, from `from` on, as they happen.
    pub fn ticks<'a>(
        &'a self,
        cx: &'a CallContext,
        req: TicksRequest,
    ) -> impl ::core::future::Future<Output = Result<CallStream<Tick, TickerError>, TickerError>>
    + ::core::marker::Send
    + 'a {
        self.0.call_stream(
            0usize,
            cx,
            req,
            |imp: ::std::sync::Arc<dyn __TickerDyn>,
             cx: ::sekvent_component::CallContext,
             req: TicksRequest| async move { __TickerDyn::__ticks(&*imp, &cx, req).await },
        )
    }

    /// The latest tick of a series.
    pub fn latest<'a>(
        &'a self,
        cx: &'a CallContext,
        req: LatestRequest,
    ) -> impl ::core::future::Future<Output = Result<Tick, TickerError>> + ::core::marker::Send + 'a
    {
        self.0.call(
            1usize,
            cx,
            req,
            |imp: ::std::sync::Arc<dyn __TickerDyn>,
             cx: ::sekvent_component::CallContext,
             req: LatestRequest| async move { __TickerDyn::__latest(&*imp, &cx, req).await },
        )
    }

    /// The binding this handle's calls use.
    pub fn binding(&self) -> ::sekvent_component::Binding {
        self.0.binding()
    }

    /// Install the implementation `factory` builds. The factory runs during
    /// `AppBuilder::build`, in install order, and only when the component is
    /// bound `local` or `local-serialized`.
    pub fn install<__SekventImpl, __SekventFactory>(
        app: &mut ::sekvent_component::AppBuilder<'_>,
        factory: __SekventFactory,
    ) -> ::core::result::Result<(), ::sekvent_component::BuildError>
    where
        __SekventImpl: Ticker,
        __SekventFactory: ::core::ops::FnOnce(
                &mut ::sekvent_component::Deps<'_>,
            ) -> ::core::result::Result<
                __SekventImpl,
                ::sekvent_component::AppError,
            > + ::core::marker::Send
            + 'static,
    {
        ::sekvent_component::__private::install_local(app, TickerHandle, move |deps| {
            let imp: ::std::sync::Arc<dyn __TickerDyn> = ::std::sync::Arc::new(factory(deps)?);
            let dispatch = ::std::sync::Arc::new(__TickerDispatcher(::std::sync::Arc::clone(&imp)));
            ::core::result::Result::Ok(
                ::sekvent_component::__private::Local::new(imp).with_dispatch(dispatch),
            )
        })
    }

    /// Like [`TickerHandle::install`], and also run the implementation's
    /// `Lifecycle` hooks when the App starts and stops.
    pub fn install_with_lifecycle<__SekventImpl, __SekventFactory>(
        app: &mut ::sekvent_component::AppBuilder<'_>,
        factory: __SekventFactory,
    ) -> ::core::result::Result<(), ::sekvent_component::BuildError>
    where
        __SekventImpl: Ticker + ::sekvent_component::Lifecycle,
        __SekventFactory: ::core::ops::FnOnce(
                &mut ::sekvent_component::Deps<'_>,
            ) -> ::core::result::Result<
                __SekventImpl,
                ::sekvent_component::AppError,
            > + ::core::marker::Send
            + 'static,
    {
        ::sekvent_component::__private::install_local(app, TickerHandle, move |deps| {
            let concrete = ::std::sync::Arc::new(factory(deps)?);
            let imp: ::std::sync::Arc<dyn __TickerDyn> =
                ::std::sync::Arc::<__SekventImpl>::clone(&concrete);
            let dispatch = ::std::sync::Arc::new(__TickerDispatcher(::std::sync::Arc::clone(&imp)));
            ::core::result::Result::Ok(
                ::sekvent_component::__private::Local::new(imp)
                    .with_dispatch(dispatch)
                    .with_lifecycle(concrete),
            )
        })
    }
}

#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::ComponentHandle for TickerHandle {
    const DESCRIPTOR: &'static ::sekvent_component::ComponentDescriptor =
        &::sekvent_component::ComponentDescriptor::new("ticker", "Ticker", TickerHandle::__METHODS)
            .with_package("shop.ticker.v1");
}

#[doc(hidden)]
pub struct __TickerDispatcher(::std::sync::Arc<dyn __TickerDyn>);

#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::__private::Dispatch for __TickerDispatcher {
    fn dispatch(
        &self,
        method: usize,
        cx: ::sekvent_component::CallContext,
        body: ::sekvent_component::__private::Bytes,
    ) -> ::sekvent_component::__private::BoxFuture<
        'static,
        ::core::result::Result<
            ::sekvent_component::__private::Bytes,
            ::sekvent_component::AppError,
        >,
    > {
        let imp = ::std::sync::Arc::clone(&self.0);
        match method {
            1usize => ::sekvent_component::__private::serve(
                cx,
                body,
                move |cx: ::sekvent_component::CallContext, req: LatestRequest| async move {
                    __TickerDyn::__latest(&*imp, &cx, req).await
                },
            ),
            _ => ::sekvent_component::__private::unknown_method(
                <TickerHandle as ::sekvent_component::ComponentHandle>::DESCRIPTOR,
                method,
            ),
        }
    }

    fn dispatch_stream(
        &self,
        method: usize,
        cx: ::sekvent_component::CallContext,
        body: ::sekvent_component::__private::Bytes,
    ) -> ::sekvent_component::__private::BoxFuture<
        'static,
        ::core::result::Result<
            ::sekvent_component::CallStream<
                ::sekvent_component::__private::Bytes,
                ::sekvent_component::AppError,
            >,
            ::sekvent_component::AppError,
        >,
    > {
        let imp = ::std::sync::Arc::clone(&self.0);
        match method {
            0usize => ::sekvent_component::__private::serve_stream(
                cx,
                body,
                move |cx: ::sekvent_component::CallContext, req: TicksRequest| async move {
                    __TickerDyn::__ticks(&*imp, &cx, req).await
                },
            ),
            _ => ::sekvent_component::__private::unknown_stream_method(
                <TickerHandle as ::sekvent_component::ComponentHandle>::DESCRIPTOR,
                method,
            ),
        }
    }
}

const _: () = {
    ::sekvent_component::__private::assert_wire::<TicksRequest>();
    ::sekvent_component::__private::assert_wire::<Tick>();
    ::sekvent_component::__private::assert_error::<TickerError>();
    ::sekvent_component::__private::assert_error::<TickerError>();
    ::sekvent_component::__private::assert_wire::<LatestRequest>();
    ::sekvent_component::__private::assert_wire::<Tick>();
    ::sekvent_component::__private::assert_error::<TickerError>();
};

const _: () = ::sekvent_component::__private::assert_service(
    crate::support::ticker::proto::__sekvent_service_Ticker,
    "shop.ticker.v1.Ticker",
    &["Ticks", "Latest"],
);
const _: () = ::sekvent_component::__private::assert_stream_rpc::<TicksRequest, Tick>(
    crate::support::ticker::proto::__sekvent_service_Ticker,
    "Ticks",
);
const _: () = ::sekvent_component::__private::assert_rpc_types::<
    TicksRequest,
    Tick,
    crate::support::ticker::proto::__sekvent_rpc_Ticker__Ticks,
>();
const _: () = ::sekvent_component::__private::assert_rpc::<LatestRequest, Tick>(
    crate::support::ticker::proto::__sekvent_service_Ticker,
    "Latest",
);
const _: () = ::sekvent_component::__private::assert_rpc_types::<
    LatestRequest,
    Tick,
    crate::support::ticker::proto::__sekvent_rpc_Ticker__Latest,
>();

// ---------------------------------------------------------------------------
// Not part of the expansion: lets tests pair the handle with another
// dispatcher (the handle's constructor is private to this module, as it is
// in generated code).
// ---------------------------------------------------------------------------

pub fn install_with_dispatch<T: Ticker>(
    app: &mut ::sekvent_component::AppBuilder<'_>,
    imp: T,
    dispatch: ::std::sync::Arc<dyn ::sekvent_component::__private::Dispatch>,
) -> ::core::result::Result<(), ::sekvent_component::BuildError> {
    ::sekvent_component::__private::install_local(app, TickerHandle, move |_deps| {
        let imp: ::std::sync::Arc<dyn __TickerDyn> = ::std::sync::Arc::new(imp);
        ::core::result::Result::Ok(
            ::sekvent_component::__private::Local::new(imp).with_dispatch(dispatch),
        )
    })
}

/// The generated dispatcher of an implementation, for byte-level tests.
pub fn dispatcher<T: Ticker>(imp: T) -> __TickerDispatcher {
    __TickerDispatcher(::std::sync::Arc::new(imp))
}

/// Declare the component as `install_remote` does for a `remote_only`
/// component: no factory, bound to a remote transport.
pub fn install_remote(
    app: &mut ::sekvent_component::AppBuilder<'_>,
) -> ::core::result::Result<(), ::sekvent_component::BuildError> {
    ::sekvent_component::__private::install_remote(app, TickerHandle)
}
