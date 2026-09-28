//! Fake inventories for fault injection. Each is installed through
//! `InventoryHandle::install`, so calls to it cross the same binding as the
//! real one.

use std::sync::{Arc, Mutex, PoisonError};

use inventory_api::{
    Inventory, InventoryError, ReleaseReply, ReleaseRequest, ReserveReply, ReserveRequest,
    StockReply, StockRequest,
};
use sekvent::prelude::*;
use tokio::sync::{Semaphore, mpsc, oneshot};

fn unsupported(method: &str) -> InventoryError {
    InventoryError::Other(AppError::unimplemented(format!(
        "the fake inventory does not implement {method}"
    )))
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

/// `reserve` reports its entry, holds a guard that signals when it is
/// dropped, and never completes: only a deadline, a cancellation or
/// dropping the call's future ends it.
pub(crate) struct PendingInventory {
    entered: mpsc::UnboundedSender<()>,
    dropped: Mutex<Option<oneshot::Sender<()>>>,
}

/// The test's side of a [`PendingInventory`].
pub(crate) struct Pending {
    /// One message per `reserve` call that entered the implementation.
    pub(crate) entered: mpsc::UnboundedReceiver<()>,
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
        _cx: &CallContext,
        _req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        let sender = {
            let mut slot = self.dropped.lock().unwrap_or_else(PoisonError::into_inner);
            slot.take()
        };
        let _guard = DropSignal(sender);
        let _ = self.entered.send(());
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
