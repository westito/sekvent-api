//! The example shop's inventory component, kept in memory.
//!
//! [`InventoryService`] implements [`inventory_api::Inventory`]; install it
//! with `InventoryHandle::install`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use inventory_api::{
    Inventory, InventoryError, ReleaseReply, ReleaseRequest, ReserveReply, ReserveRequest,
    StockReply, StockRequest,
};
use sekvent::prelude::*;

/// In-memory stock and reservations.
///
/// Reservation ids come from a per-instance counter (`res-1`, `res-2`, …),
/// so tests can predict them.
#[derive(Debug)]
pub struct InventoryService {
    state: Mutex<State>,
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
        Self {
            state: Mutex::new(State {
                stock: stock.into_iter().collect(),
                ..State::default()
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
        let units = state.stock.entry(reservation.sku).or_default();
        *units = units.saturating_add(reservation.quantity);
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
}
