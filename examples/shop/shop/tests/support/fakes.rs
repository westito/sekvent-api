//! Fake inventories for fault injection. Each is installed through
//! `InventoryHandle::install`, so calls to it cross the same binding as the
//! real one; under the split profile it runs in the inventory service's App
//! and still reports to the test, which shares its process.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use inventory_api::{
    Inventory, InventoryError, ReleaseReply, ReleaseRequest, ReserveReply, ReserveRequest,
    StockReply, StockRequest,
};
use sekvent::context::ServiceIdentity;
use sekvent::prelude::*;
use tokio::sync::{Semaphore, mpsc, oneshot};

fn unsupported(method: &str) -> InventoryError {
    InventoryError::Other(AppError::unimplemented(format!(
        "the fake inventory does not implement {method}"
    )))
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `reserve` reports its order id on a channel, then waits for a permit.
/// Each permit lets exactly one call through.
pub(crate) struct GatedInventory {
    entered: mpsc::UnboundedSender<String>,
    permits: Arc<Semaphore>,
}

/// The test's side of a [`GatedInventory`].
pub(crate) struct Gate {
    /// Order ids of the `reserve` calls that entered the implementation.
    pub(crate) entered: mpsc::UnboundedReceiver<String>,
    /// Add permits to let waiting calls finish.
    pub(crate) permits: Arc<Semaphore>,
}

impl GatedInventory {
    pub(crate) fn new() -> (Self, Gate) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let permits = Arc::new(Semaphore::new(0));
        let fake = Self {
            entered: sender,
            permits: Arc::clone(&permits),
        };
        (
            fake,
            Gate {
                entered: receiver,
                permits,
            },
        )
    }
}

impl Inventory for GatedInventory {
    async fn reserve(
        &self,
        _cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        // The receiver may be gone once a test has seen what it needed.
        let _ = self.entered.send(req.order_id.clone());
        let permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| InventoryError::Other(AppError::unavailable("the gate was closed")))?;
        permit.forget();
        Ok(ReserveReply {
            reservation_id: format!("res-{}", req.order_id),
            remaining: 0,
        })
    }

    async fn release(
        &self,
        _cx: &CallContext,
        _req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        Err(unsupported("release"))
    }

    async fn stock(
        &self,
        _cx: &CallContext,
        _req: StockRequest,
    ) -> Result<StockReply, InventoryError> {
        Err(unsupported("stock"))
    }
}

/// Fires its `oneshot` when dropped.
struct DropSignal(Option<oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

/// `reserve` records the time its context has left, reports its entry,
/// holds a guard that signals when it is dropped, and never completes: only
/// a deadline, a cancellation or dropping the call's future ends it.
pub(crate) struct PendingInventory {
    entered: mpsc::UnboundedSender<Option<Duration>>,
    dropped: Mutex<Option<oneshot::Sender<()>>>,
}

/// The test's side of a [`PendingInventory`].
pub(crate) struct Pending {
    /// One message per `reserve` call that entered the implementation: the
    /// time its context had left on entry (`None` without a deadline),
    /// measured on tokio's clock.
    pub(crate) entered: mpsc::UnboundedReceiver<Option<Duration>>,
    /// Resolves when the first call's future is dropped.
    pub(crate) dropped: oneshot::Receiver<()>,
}

impl PendingInventory {
    pub(crate) fn new() -> (Self, Pending) {
        let (entered_sender, entered) = mpsc::unbounded_channel();
        let (dropped_sender, dropped) = oneshot::channel();
        let fake = Self {
            entered: entered_sender,
            dropped: Mutex::new(Some(dropped_sender)),
        };
        (fake, Pending { entered, dropped })
    }
}

impl Inventory for PendingInventory {
    async fn reserve(
        &self,
        cx: &CallContext,
        _req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        let sender = locked(&self.dropped).take();
        let _guard = DropSignal(sender);
        let _ = self.entered.send(sekvent::resilience::remaining(cx));
        std::future::pending().await
    }

    async fn release(
        &self,
        _cx: &CallContext,
        _req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        Err(unsupported("release"))
    }

    async fn stock(
        &self,
        _cx: &CallContext,
        _req: StockRequest,
    ) -> Result<StockReply, InventoryError> {
        Err(unsupported("stock"))
    }
}

/// Reason the [`FutureReasonInventory`] returns; no variant of
/// `InventoryError` knows it.
pub(crate) const FUTURE_REASON: &str = "FROM_THE_FUTURE";

/// `reserve` fails with a reason this build of `InventoryError` does not
/// know, as a newer implementation might.
pub(crate) struct FutureReasonInventory;

impl Inventory for FutureReasonInventory {
    async fn reserve(
        &self,
        _cx: &CallContext,
        _req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        Err(InventoryError::Other(
            AppError::failed_precondition("the reservation needs a newer caller")
                .with_reason(FUTURE_REASON),
        ))
    }

    async fn release(
        &self,
        _cx: &CallContext,
        _req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        Err(unsupported("release"))
    }

    async fn stock(
        &self,
        _cx: &CallContext,
        _req: StockRequest,
    ) -> Result<StockReply, InventoryError> {
        Err(unsupported("stock"))
    }
}

/// A method of the inventory component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    /// `reserve`.
    Reserve,
    /// `release`.
    Release,
    /// `stock`.
    Stock,
}

/// One call that entered a [`FlakyInventory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The method called.
    pub(crate) method: Method,
    /// The call's request id.
    pub(crate) request_id: String,
    /// The call's idempotency key, if any.
    pub(crate) idempotency_key: Option<String>,
}

/// Every call a fake saw, shared with the test.
pub(crate) type Log<T> = Arc<Mutex<Vec<T>>>;

/// A snapshot of `log`.
pub(crate) fn entries<T: Clone>(log: &Log<T>) -> Vec<T> {
    locked(log).clone()
}

/// Fails the first `failures` calls of one method with the error `error`
/// makes, then succeeds; every call of every method is recorded.
pub(crate) struct FlakyInventory {
    method: Method,
    failures: usize,
    error: Box<dyn Fn() -> AppError + Send + Sync>,
    log: Log<Entry>,
}

impl FlakyInventory {
    /// A fake failing the first `failures` calls of `method` with `error()`;
    /// `usize::MAX` fails every call.
    pub(crate) fn new(
        method: Method,
        failures: usize,
        error: impl Fn() -> AppError + Send + Sync + 'static,
    ) -> (Self, Log<Entry>) {
        let log = Log::default();
        let fake = Self {
            method,
            failures,
            error: Box::new(error),
            log: Arc::clone(&log),
        };
        (fake, log)
    }

    /// Record the call; fail it while the method's failures last.
    fn enter(&self, method: Method, cx: &CallContext) -> Result<(), InventoryError> {
        let mut log = locked(&self.log);
        let seen = log.iter().filter(|entry| entry.method == method).count();
        log.push(Entry {
            method,
            request_id: cx.request_id().to_owned(),
            idempotency_key: cx.idempotency_key().map(str::to_owned),
        });
        if method == self.method && seen < self.failures {
            Err(InventoryError::Other((self.error)()))
        } else {
            Ok(())
        }
    }
}

impl Inventory for FlakyInventory {
    async fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        self.enter(Method::Reserve, cx)?;
        Ok(ReserveReply {
            reservation_id: format!("res-{}", req.order_id),
            remaining: 0,
        })
    }

    async fn release(
        &self,
        cx: &CallContext,
        _req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        self.enter(Method::Release, cx)?;
        Ok(ReleaseReply { released: true })
    }

    async fn stock(
        &self,
        cx: &CallContext,
        req: StockRequest,
    ) -> Result<StockReply, InventoryError> {
        self.enter(Method::Stock, cx)?;
        Ok(StockReply {
            sku: req.sku,
            available: 1,
        })
    }
}

/// What a [`RecordingInventory`] saw of one call's context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    /// The method called.
    pub(crate) method: Method,
    /// The authenticated caller, if any.
    pub(crate) caller: Option<ServiceIdentity>,
    /// The end user's tenant, if it survived.
    pub(crate) tenant: Option<String>,
    /// Time left before the deadline on entry, on tokio's clock.
    pub(crate) remaining: Option<Duration>,
}

/// Succeeds on every call and records what each call's context carried.
pub(crate) struct RecordingInventory {
    log: Log<Seen>,
}

impl RecordingInventory {
    pub(crate) fn new() -> (Self, Log<Seen>) {
        let log = Log::default();
        (
            Self {
                log: Arc::clone(&log),
            },
            log,
        )
    }

    fn record(&self, method: Method, cx: &CallContext) {
        locked(&self.log).push(Seen {
            method,
            caller: cx.caller().cloned(),
            tenant: cx.tenant().map(str::to_owned),
            remaining: sekvent::resilience::remaining(cx),
        });
    }
}

impl Inventory for RecordingInventory {
    async fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        self.record(Method::Reserve, cx);
        Ok(ReserveReply {
            reservation_id: format!("res-{}", req.order_id),
            remaining: 0,
        })
    }

    async fn release(
        &self,
        cx: &CallContext,
        _req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        self.record(Method::Release, cx);
        Ok(ReleaseReply { released: true })
    }

    async fn stock(
        &self,
        cx: &CallContext,
        req: StockRequest,
    ) -> Result<StockReply, InventoryError> {
        self.record(Method::Stock, cx);
        Ok(StockReply {
            sku: req.sku,
            available: 1,
        })
    }
}
