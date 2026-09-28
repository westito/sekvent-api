//! Contract of the example shop's notifications component.
//!
//! Callers depend on this crate only: the [`Notifications`] trait, the
//! [`NotificationsHandle`] they call it through, its messages and its typed
//! [`NotificationsError`].

#![forbid(unsafe_code)]

/// Messages of the `shop.notifications.v1` package and the contract of its
/// `Notifications` service.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
}

pub use proto::shop::notifications::v1::{
    Notification, NotifyReply, NotifyRequest, SentReply, SentRequest,
};
use sekvent::prelude::*;

/// What can go wrong in the notifications component.
#[derive(Debug, ComponentError)]
#[component_error(domain = "shop.notifications.v1")]
pub enum NotificationsError {
    /// The customer has opted out of notifications.
    #[reason("RECIPIENT_BLOCKED", code = FailedPrecondition)]
    RecipientBlocked {
        /// The customer that cannot be notified.
        customer_id: String,
    },
    /// Any other error, including framework errors such as a missed deadline.
    #[other]
    Other(AppError),
}

/// Customer notifications about orders.
#[sekvent::component(
    name = "notifications",
    package = "shop.notifications.v1",
    proto = "crate::proto::shop::notifications::v1"
)]
pub trait Notifications: Send + Sync + 'static {
    /// Notify a customer about an order.
    #[call(timeout = "1s")]
    async fn notify(
        &self,
        cx: &CallContext,
        req: NotifyRequest,
    ) -> Result<NotifyReply, NotificationsError>;

    /// The notifications sent to a customer, oldest first.
    #[call(idempotent)]
    async fn sent(
        &self,
        cx: &CallContext,
        req: SentRequest,
    ) -> Result<SentReply, NotificationsError>;
}
