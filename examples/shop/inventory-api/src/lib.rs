//! Contract of the example shop's inventory component.
//!
//! Callers depend on this crate only: the [`Inventory`] trait, the
//! [`InventoryHandle`] they call it through, its messages and its typed
//! [`InventoryError`]. Where the implementation runs is decided when the App
//! is built, never here.

#![forbid(unsafe_code)]

/// Messages of the `shop.inventory.v1` package.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
}

pub use proto::shop::inventory::v1::{
    ReleaseReply, ReleaseRequest, ReserveReply, ReserveRequest, StockReply, StockRequest,
};
use sekvent::prelude::*;

/// What can go wrong in the inventory component.
///
/// Each variant travels as an `AppError` with its reason, the domain
/// `shop.inventory.v1` and its fields as metadata, and decodes back into the
/// same variant on the caller's side.
#[derive(Debug, ComponentError)]
#[component_error(domain = "shop.inventory.v1")]
pub enum InventoryError {
    /// Fewer units are available than were asked for.
    #[reason("OUT_OF_STOCK", code = FailedPrecondition, message = "only {available} of {sku} left")]
    OutOfStock {
        /// The SKU that ran short.
        sku: String,
        /// Units available when the reservation was attempted.
        available: u32,
    },
    /// The SKU is not stocked at all.
    #[reason("UNKNOWN_SKU", code = NotFound)]
    UnknownSku {
        /// The SKU that was asked for.
        sku: String,
    },
    /// No reservation has this id.
    #[reason("RESERVATION_NOT_FOUND", code = NotFound)]
    ReservationNotFound {
        /// The id that was asked for.
        reservation_id: String,
    },
    /// Any other error, including framework errors such as a missed deadline.
    #[other]
    Other(AppError),
}

/// Stock and reservations.
#[sekvent::component(name = "inventory", package = "shop.inventory.v1")]
pub trait Inventory: Send + Sync + 'static {
    /// Reserve stock for an order. Reserving again for the same order returns
    /// the existing reservation.
    #[call(idempotent, timeout = "2s", bulkhead = 16)]
    async fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError>;

    /// Release a reservation, giving its units back.
    #[call(timeout = "500ms")]
    async fn release(
        &self,
        cx: &CallContext,
        req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError>;

    /// How many units of a SKU are available.
    #[call(idempotent, timeout = "500ms")]
    async fn stock(
        &self,
        cx: &CallContext,
        req: StockRequest,
    ) -> Result<StockReply, InventoryError>;
}
