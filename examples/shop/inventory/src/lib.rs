//! The example shop's inventory component, kept in memory.
//!
//! [`InventoryService`] implements [`inventory_api::Inventory`]; install it
//! with `InventoryHandle::install`. It is cheap to clone: clones share the
//! stock, so a test can keep one to watch [`InventoryService::watchers`].

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use inventory_api::{
    Inventory, InventoryError, ReleaseReply, ReleaseRequest, ReserveReply, ReserveRequest,
    StockReply, StockRequest,
};
use sekvent::prelude::*;
use tokio::sync::watch;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::WatchStream;

/// In-memory stock and reservations.
///
/// Reservation ids come from a per-instance counter (`res-1`, `res-2`, …),
/// so tests can predict them. Every SKU has a `watch` channel carrying its
/// level, which `watch_stock` subscribes to.
#[derive(Debug, Clone)]
pub struct InventoryService {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    /// The level of every SKU, fixed at construction. Written only while
    /// the state is locked, so watchers see levels in the order they were
    /// reached.
    levels: HashMap<String, watch::Sender<u32>>,
}

#[derive(Debug, Default)]
struct State {
    stock: HashMap<String, u32>,
    reservations: HashMap<String, Reservation>,
    /// Reservation id by order id, so reserving twice for one order is a no-op.
    by_order: HashMap<String, String>,
    issued: u64,
}

#[derive(Debug)]
struct Reservation {
    order_id: String,
    sku: String,
    quantity: u32,
}

impl InventoryService {
    /// An inventory holding `stock` as `(sku, units)` pairs.
    pub fn new(stock: impl IntoIterator<Item = (String, u32)>) -> Self {
        let stock: HashMap<String, u32> = stock.into_iter().collect();
        let levels = stock
            .iter()
            .map(|(sku, &units)| (sku.clone(), watch::Sender::new(units)))
            .collect();
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    stock,
                    ..State::default()
                }),
                levels,
            }),
        }
    }

    /// How many `watch_stock` streams of `sku` are open right now.
    pub fn watchers(&self, sku: &str) -> usize {
        self.inner
            .levels
            .get(sku)
            .map_or(0, watch::Sender::receiver_count)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Tell the watchers of `sku` its new level; call with the state locked.
    fn publish(&self, sku: &str, units: u32) {
        if let Some(level) = self.inner.levels.get(sku) {
            level.send_if_modified(|current| {
                let changed = *current != units;
                *current = units;
                changed
            });
        }
    }
}

impl Inventory for InventoryService {
    async fn reserve(
        &self,
        _cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        let mut state = self.state();
        let Some(&available) = state.stock.get(&req.sku) else {
            return Err(InventoryError::UnknownSku { sku: req.sku });
        };
        if let Some(existing) = state.by_order.get(&req.order_id) {
            return Ok(ReserveReply {
                reservation_id: existing.clone(),
                remaining: available,
            });
        }
        let Some(remaining) = available.checked_sub(req.quantity) else {
            return Err(InventoryError::OutOfStock {
                sku: req.sku,
                available,
            });
        };
        state.issued += 1;
        let reservation_id = format!("res-{}", state.issued);
        state.stock.insert(req.sku.clone(), remaining);
        self.publish(&req.sku, remaining);
        state
            .by_order
            .insert(req.order_id.clone(), reservation_id.clone());
        state.reservations.insert(
            reservation_id.clone(),
            Reservation {
                order_id: req.order_id,
                sku: req.sku,
                quantity: req.quantity,
            },
        );
        Ok(ReserveReply {
            reservation_id,
            remaining,
        })
    }

    async fn release(
        &self,
        _cx: &CallContext,
        req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        let mut state = self.state();
        let Some(reservation) = state.reservations.remove(&req.reservation_id) else {
            return Err(InventoryError::ReservationNotFound {
                reservation_id: req.reservation_id,
            });
        };
        state.by_order.remove(&reservation.order_id);
        let units = state.stock.entry(reservation.sku.clone()).or_default();
        *units = units.saturating_add(reservation.quantity);
        let units = *units;
        self.publish(&reservation.sku, units);
        Ok(ReleaseReply { released: true })
    }

    async fn stock(
        &self,
        _cx: &CallContext,
        req: StockRequest,
    ) -> Result<StockReply, InventoryError> {
        let available = self.state().stock.get(&req.sku).copied();
        match available {
            Some(available) => Ok(StockReply {
                sku: req.sku,
                available,
            }),
            None => Err(InventoryError::UnknownSku { sku: req.sku }),
        }
    }

    async fn watch_stock(
        &self,
        _cx: &CallContext,
        req: StockRequest,
    ) -> Result<CallStream<StockReply, InventoryError>, InventoryError> {
        let Some(level) = self.inner.levels.get(&req.sku) else {
            return Err(InventoryError::UnknownSku { sku: req.sku });
        };
        let sku = req.sku;
        // The current level first, then the latest level after each change:
        // a slow watcher skips levels in between, never the last one.
        let levels = WatchStream::new(level.subscribe()).map(move |available| {
            Ok::<_, InventoryError>(StockReply {
                sku: sku.clone(),
                available,
            })
        });
        Ok(CallStream::new(levels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> InventoryService {
        InventoryService::new([("sku-apple".to_owned(), 5)])
    }

    fn reserve(order_id: &str, quantity: u32) -> ReserveRequest {
        ReserveRequest {
            order_id: order_id.to_owned(),
            sku: "sku-apple".to_owned(),
            quantity,
        }
    }

    async fn available(inventory: &InventoryService) -> u32 {
        let request = StockRequest {
            sku: "sku-apple".to_owned(),
        };
        inventory
            .stock(&CallContext::new(), request)
            .await
            .expect("sku-apple is stocked")
            .available
    }

    #[tokio::test]
    async fn reserving_twice_for_one_order_reserves_once() {
        let inventory = inventory();
        let cx = CallContext::new();
        let first = inventory.reserve(&cx, reserve("ord-1", 2)).await.unwrap();
        let again = inventory.reserve(&cx, reserve("ord-1", 2)).await.unwrap();
        assert_eq!(first.reservation_id, "res-1");
        assert_eq!(again.reservation_id, "res-1");
        assert_eq!(first.remaining, 3);
        assert_eq!(available(&inventory).await, 3);
    }

    #[tokio::test]
    async fn release_restores_stock_once() {
        let inventory = inventory();
        let cx = CallContext::new();
        let reserved = inventory.reserve(&cx, reserve("ord-1", 5)).await.unwrap();
        assert_eq!(reserved.remaining, 0);
        let release = || ReleaseRequest {
            reservation_id: reserved.reservation_id.clone(),
        };
        assert!(inventory.release(&cx, release()).await.unwrap().released);
        assert_eq!(available(&inventory).await, 5);
        let again = inventory.release(&cx, release()).await.unwrap_err();
        assert!(matches!(
            again,
            InventoryError::ReservationNotFound { reservation_id } if reservation_id == "res-1"
        ));
    }

    #[tokio::test]
    async fn shortage_and_unknown_sku_are_typed() {
        let inventory = inventory();
        let cx = CallContext::new();
        let short = inventory
            .reserve(&cx, reserve("ord-1", 6))
            .await
            .unwrap_err();
        assert!(matches!(
            short,
            InventoryError::OutOfStock { ref sku, available: 5 } if sku == "sku-apple"
        ));
        let unknown = StockRequest {
            sku: "sku-none".to_owned(),
        };
        let missing = inventory.stock(&cx, unknown).await.unwrap_err();
        assert!(matches!(missing, InventoryError::UnknownSku { sku } if sku == "sku-none"));
    }

    fn apples() -> StockRequest {
        StockRequest {
            sku: "sku-apple".to_owned(),
        }
    }

    async fn next_level(levels: &mut CallStream<StockReply, InventoryError>) -> u32 {
        let level = levels
            .next()
            .await
            .expect("the stream of levels never ends on its own")
            .expect("levels are never errors");
        assert_eq!(level.sku, "sku-apple");
        level.available
    }

    #[tokio::test]
    async fn watching_yields_the_level_now_then_after_every_change() {
        let inventory = inventory();
        let cx = CallContext::new();
        let mut levels = inventory.watch_stock(&cx, apples()).await.unwrap();
        assert_eq!(next_level(&mut levels).await, 5);

        let reserved = inventory.reserve(&cx, reserve("ord-1", 2)).await.unwrap();
        assert_eq!(next_level(&mut levels).await, 3);

        let release = ReleaseRequest {
            reservation_id: reserved.reservation_id,
        };
        inventory.release(&cx, release).await.unwrap();
        assert_eq!(next_level(&mut levels).await, 5);
    }

    #[tokio::test]
    async fn a_slow_watcher_skips_to_the_latest_level() {
        let inventory = inventory();
        let cx = CallContext::new();
        let mut levels = inventory.watch_stock(&cx, apples()).await.unwrap();
        assert_eq!(next_level(&mut levels).await, 5);

        inventory.reserve(&cx, reserve("ord-1", 1)).await.unwrap();
        inventory.reserve(&cx, reserve("ord-2", 1)).await.unwrap();
        assert_eq!(next_level(&mut levels).await, 3);
    }

    #[tokio::test]
    async fn an_unknown_sku_fails_the_open() {
        let inventory = inventory();
        let unknown = StockRequest {
            sku: "sku-none".to_owned(),
        };
        let Err(error) = inventory.watch_stock(&CallContext::new(), unknown).await else {
            panic!("the open of an unknown SKU succeeded");
        };
        assert!(matches!(error, InventoryError::UnknownSku { sku } if sku == "sku-none"));
        assert_eq!(inventory.watchers("sku-none"), 0);
    }

    #[tokio::test]
    async fn dropping_a_stream_releases_its_subscription() {
        let inventory = inventory();
        let watcher = inventory.clone();
        let cx = CallContext::new();
        let first = inventory.watch_stock(&cx, apples()).await.unwrap();
        let second = inventory.watch_stock(&cx, apples()).await.unwrap();
        assert_eq!(watcher.watchers("sku-apple"), 2);
        drop(first);
        assert_eq!(watcher.watchers("sku-apple"), 1);
        drop(second);
        assert_eq!(watcher.watchers("sku-apple"), 0);
    }
}
