//! Contract of the example shop's orders component.
//!
//! Callers depend on this crate only: the [`Orders`] trait, the
//! [`OrdersHandle`] they call it through, its messages and its typed
//! [`OrdersError`]. The orders component calls inventory and notifications
//! itself, but its contract does not expose their errors: it maps the ones a
//! caller can act on into its own variants.

#![forbid(unsafe_code)]

/// Messages of the `shop.orders.v1` package and the contract of its `Orders`
/// service.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
}

pub use proto::shop::orders::v1::{
    GetOrderRequest, Order, OrderStatus, PlaceOrderReply, PlaceOrderRequest,
};
use sekvent::prelude::*;

/// What can go wrong in the orders component.
#[derive(Debug, ComponentError)]
#[component_error(domain = "shop.orders.v1")]
pub enum OrdersError {
    /// An order needs at least one unit.
    #[reason("INVALID_QUANTITY", code = InvalidArgument)]
    InvalidQuantity {
        /// The quantity that was asked for.
        quantity: u32,
    },
    /// No order has this id.
    #[reason("ORDER_NOT_FOUND", code = NotFound)]
    OrderNotFound {
        /// The id that was asked for.
        order_id: String,
    },
    /// Fewer units are available than were ordered.
    #[reason("OUT_OF_STOCK", code = FailedPrecondition, message = "only {available} of {sku} left")]
    OutOfStock {
        /// The SKU that ran short.
        sku: String,
        /// Units available when the order was placed.
        available: u32,
    },
    /// The SKU is not sold.
    #[reason("UNKNOWN_SKU", code = NotFound)]
    UnknownSku {
        /// The SKU that was ordered.
        sku: String,
    },
    /// The customer cannot be notified, so the order is not placed.
    #[reason("CUSTOMER_BLOCKED", code = FailedPrecondition)]
    CustomerBlocked {
        /// The customer that placed the order.
        customer_id: String,
    },
    /// Any other error, including framework errors such as a missed deadline.
    #[other]
    Other(AppError),
}

/// Placing and reading orders.
#[sekvent::component(
    name = "orders",
    package = "shop.orders.v1",
    proto = "crate::proto::shop::orders::v1"
)]
pub trait Orders: Send + Sync + 'static {
    /// Place an order: reserve its stock, notify the customer and store it.
    #[call(timeout = "5s")]
    async fn place_order(
        &self,
        cx: &CallContext,
        req: PlaceOrderRequest,
    ) -> Result<PlaceOrderReply, OrdersError>;

    /// Read one order.
    #[call(idempotent, timeout = "1s")]
    async fn get_order(&self, cx: &CallContext, req: GetOrderRequest)
    -> Result<Order, OrdersError>;
}
