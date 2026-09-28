//! The example shop's orders component, kept in memory.
//!
//! [`OrdersService`] implements [`orders_api::Orders`] on top of two other
//! components, reached through their handles. It neither knows nor cares
//! whether those calls stay in-process or cross a serialization boundary.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use inventory_api::{InventoryError, InventoryHandle, ReleaseRequest, ReserveRequest};
use notifications_api::{NotificationsError, NotificationsHandle, NotifyRequest};
use orders_api::{
    GetOrderRequest, Order, OrderStatus, Orders, OrdersError, PlaceOrderReply, PlaceOrderRequest,
};
use sekvent::prelude::*;

/// Template of the notification sent for a placed order.
pub const ORDER_PLACED_TEMPLATE: &str = "order_placed";

/// In-memory orders.
///
/// Order ids come from a per-instance counter (`ord-1`, `ord-2`, …); an id
/// is taken when an order is attempted, so a rejected order still uses one.
#[derive(Debug)]
pub struct OrdersService {
    inventory: InventoryHandle,
    notifications: NotificationsHandle,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    orders: HashMap<String, Order>,
    issued: u64,
}

impl OrdersService {
    /// Orders that reserve stock through `inventory` and notify customers
    /// through `notifications`. Build it in the factory from
    /// `deps.handle::<InventoryHandle>()?` and
    /// `deps.handle::<NotificationsHandle>()?`.
    pub fn new(inventory: InventoryHandle, notifications: NotificationsHandle) -> Self {
        Self {
            inventory,
            notifications,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_order_id(&self) -> String {
        let mut state = self.state();
        state.issued += 1;
        format!("ord-{}", state.issued)
    }

    /// Undo a reservation after a failed order. The order already failed for
    /// another reason, so a failed release is logged and not reported.
    async fn release_quietly(&self, cx: &CallContext, reservation_id: &str) {
        let request = ReleaseRequest {
            reservation_id: reservation_id.to_owned(),
        };
        if let Err(error) = self.inventory.release(cx, request).await {
            tracing::warn!(%reservation_id, ?error, "could not release the reservation of a failed order");
        }
    }
}

impl Orders for OrdersService {
    async fn place_order(
        &self,
        cx: &CallContext,
        req: PlaceOrderRequest,
    ) -> Result<PlaceOrderReply, OrdersError> {
        if req.quantity == 0 {
            return Err(OrdersError::InvalidQuantity {
                quantity: req.quantity,
            });
        }
        let order_id = self.next_order_id();

        let reserve = ReserveRequest {
            order_id: order_id.clone(),
            sku: req.sku.clone(),
            quantity: req.quantity,
        };
        let reservation =
            self.inventory
                .reserve(cx, reserve)
                .await
                .map_err(|error| match error {
                    InventoryError::OutOfStock { sku, available } => {
                        OrdersError::OutOfStock { sku, available }
                    }
                    InventoryError::UnknownSku { sku } => OrdersError::UnknownSku { sku },
                    error @ (InventoryError::ReservationNotFound { .. }
                    | InventoryError::Other(_)) => OrdersError::Other(error.into()),
                })?;

        let notify = NotifyRequest {
            customer_id: req.customer_id.clone(),
            order_id: order_id.clone(),
            template: ORDER_PLACED_TEMPLATE.to_owned(),
        };
        if let Err(error) = self.notifications.notify(cx, notify).await {
            self.release_quietly(cx, &reservation.reservation_id).await;
            return Err(match error {
                NotificationsError::RecipientBlocked { customer_id } => {
                    OrdersError::CustomerBlocked { customer_id }
                }
                NotificationsError::Other(error) => OrdersError::Other(error),
            });
        }

        let order = Order {
            order_id: order_id.clone(),
            customer_id: req.customer_id,
            sku: req.sku,
            quantity: req.quantity,
            status: i32::from(OrderStatus::Placed),
        };
        self.state().orders.insert(order_id.clone(), order);
        Ok(PlaceOrderReply {
            order_id,
            reservation_id: reservation.reservation_id,
        })
    }

    async fn get_order(
        &self,
        _cx: &CallContext,
        req: GetOrderRequest,
    ) -> Result<Order, OrdersError> {
        let order = self.state().orders.get(&req.order_id).cloned();
        order.ok_or(OrdersError::OrderNotFound {
            order_id: req.order_id,
        })
    }
}
