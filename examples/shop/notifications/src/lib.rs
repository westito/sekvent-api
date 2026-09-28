//! The example shop's notifications component, kept in memory.
//!
//! [`NotificationsService`] implements [`notifications_api::Notifications`]
//! and [`Lifecycle`]: it accepts notifications only between `on_start` and
//! `on_stop`. Install it with `NotificationsHandle::install_with_lifecycle`
//! so the App runs those hooks.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use notifications_api::{
    Notification, Notifications, NotificationsError, NotifyReply, NotifyRequest, SentReply,
    SentRequest,
};
use sekvent::prelude::*;

/// An in-memory outbox with a blocklist of customers who opted out.
///
/// Notification ids come from a per-instance counter (`ntf-1`, `ntf-2`, …).
/// Each stored notification records the tenant of the call that sent it.
#[derive(Debug)]
pub struct NotificationsService {
    blocked: HashSet<String>,
    open: AtomicBool,
    outbox: Mutex<Outbox>,
}

#[derive(Debug, Default)]
struct Outbox {
    sent: Vec<Notification>,
    issued: u64,
}

impl NotificationsService {
    /// An outbox that refuses to notify the `blocked` customers. It stays
    /// closed until [`Lifecycle::on_start`] runs.
    pub fn new(blocked: impl IntoIterator<Item = String>) -> Self {
        Self {
            blocked: blocked.into_iter().collect(),
            open: AtomicBool::new(false),
            outbox: Mutex::new(Outbox::default()),
        }
    }

    /// Whether the outbox accepts notifications.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    fn outbox(&self) -> MutexGuard<'_, Outbox> {
        self.outbox.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Lifecycle for NotificationsService {
    async fn on_start(&self) -> Result<(), AppError> {
        self.open.store(true, Ordering::Release);
        Ok(())
    }

    async fn on_stop(&self) -> Result<(), AppError> {
        self.open.store(false, Ordering::Release);
        Ok(())
    }
}

impl Notifications for NotificationsService {
    async fn notify(
        &self,
        cx: &CallContext,
        req: NotifyRequest,
    ) -> Result<NotifyReply, NotificationsError> {
        if !self.is_open() {
            return Err(NotificationsError::Other(AppError::unavailable(
                "notifications are closed",
            )));
        }
        if self.blocked.contains(&req.customer_id) {
            return Err(NotificationsError::RecipientBlocked {
                customer_id: req.customer_id,
            });
        }
        let mut outbox = self.outbox();
        outbox.issued += 1;
        let notification_id = format!("ntf-{}", outbox.issued);
        outbox.sent.push(Notification {
            notification_id: notification_id.clone(),
            customer_id: req.customer_id,
            order_id: req.order_id,
            template: req.template,
            tenant: cx.tenant().unwrap_or_default().to_owned(),
        });
        Ok(NotifyReply { notification_id })
    }

    async fn sent(
        &self,
        _cx: &CallContext,
        req: SentRequest,
    ) -> Result<SentReply, NotificationsError> {
        let notifications = self
            .outbox()
            .sent
            .iter()
            .filter(|notification| notification.customer_id == req.customer_id)
            .cloned()
            .collect();
        Ok(SentReply { notifications })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify(customer_id: &str) -> NotifyRequest {
        NotifyRequest {
            customer_id: customer_id.to_owned(),
            order_id: "ord-1".to_owned(),
            template: "order_placed".to_owned(),
        }
    }

    #[tokio::test]
    async fn closed_until_started_and_after_stopped() {
        let service = NotificationsService::new(Vec::new());
        let cx = CallContext::new();
        let closed = service.notify(&cx, notify("cust-1")).await.unwrap_err();
        assert!(
            matches!(closed, NotificationsError::Other(ref error) if error.code() == ErrorCode::Unavailable)
        );

        service.on_start().await.unwrap();
        let reply = service.notify(&cx, notify("cust-1")).await.unwrap();
        assert_eq!(reply.notification_id, "ntf-1");

        service.on_stop().await.unwrap();
        assert!(!service.is_open());
    }

    #[tokio::test]
    async fn blocked_customers_are_refused_and_tenants_recorded() {
        let service = NotificationsService::new(["cust-blocked".to_owned()]);
        service.on_start().await.unwrap();
        let cx = CallContext::new().with_tenant("tenant-a");

        let blocked = service
            .notify(&cx, notify("cust-blocked"))
            .await
            .unwrap_err();
        assert!(matches!(
            blocked,
            NotificationsError::RecipientBlocked { customer_id } if customer_id == "cust-blocked"
        ));

        service.notify(&cx, notify("cust-1")).await.unwrap();
        let request = SentRequest {
            customer_id: "cust-1".to_owned(),
        };
        let sent = service.sent(&cx, request).await.unwrap().notifications;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].tenant, "tenant-a");
    }
}
