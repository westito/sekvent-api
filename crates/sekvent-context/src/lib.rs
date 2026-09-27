//! Per-call context carried through every handler, client and queue message.
//!
//! In process the deadline is an absolute [`Instant`]; on the wire it travels
//! as a relative `grpc-timeout` (see [`headers`]). Work that is queued for
//! later never inherits the caller's deadline or cancellation: use
//! [`CallContext::detached`].

#![forbid(unsafe_code)]

mod clock;
pub mod headers;

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

pub use clock::{Clock, ManualClock, SystemClock};

/// Who is calling, as established by service-to-service authentication.
///
/// Only a caller authenticated as a trusted service link may propagate
/// end-user `subject` and `tenant` values; the inbound layer strips them
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceIdentity {
    /// Stable name of the calling service or link, e.g. `billing`.
    pub name: String,
    /// Whether this caller may assert `subject` / `tenant` on behalf of a user.
    pub trusted: bool,
}

impl ServiceIdentity {
    /// A caller that may not assert user identity.
    pub fn untrusted(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            trusted: false,
        }
    }
    /// A caller that may assert user identity.
    pub fn trusted(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            trusted: true,
        }
    }
}

/// The context of one logical call.
#[derive(Debug, Clone)]
pub struct CallContext {
    request_id: String,
    deadline: Option<Instant>,
    cancel: CancellationToken,
    caller: Option<ServiceIdentity>,
    subject: Option<String>,
    tenant: Option<String>,
    idempotency_key: Option<String>,
    traceparent: Option<String>,
}

impl Default for CallContext {
    fn default() -> Self {
        Self::new()
    }
}

impl CallContext {
    /// A fresh root context with a new request id, no deadline and its own
    /// cancellation token.
    pub fn new() -> Self {
        Self {
            request_id: uuid::Uuid::now_v7().to_string(),
            deadline: None,
            cancel: CancellationToken::new(),
            caller: None,
            subject: None,
            tenant: None,
            idempotency_key: None,
            traceparent: None,
        }
    }

    /// Replace the request id.
    #[must_use]
    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = id.into();
        self
    }
    /// Set an absolute deadline. A later deadline than the current one is
    /// ignored: a callee never gets more time than its caller.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(match self.deadline {
            Some(current) => current.min(deadline),
            None => deadline,
        });
        self
    }
    /// Set a deadline relative to now (same narrowing rule).
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.with_deadline(Instant::now() + timeout)
    }
    /// Use this cancellation token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }
    /// Set the authenticated caller.
    #[must_use]
    pub fn with_caller(mut self, caller: ServiceIdentity) -> Self {
        self.caller = Some(caller);
        self
    }
    /// Set the end-user subject.
    #[must_use]
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }
    /// Set the tenant.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }
    /// Set the idempotency key.
    #[must_use]
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
    /// Set the W3C `traceparent`.
    #[must_use]
    pub fn with_traceparent(mut self, traceparent: impl Into<String>) -> Self {
        self.traceparent = Some(traceparent.into());
        self
    }

    /// Request id.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    /// Absolute deadline, if any.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    /// Time left before the deadline; `None` without a deadline, zero once passed.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }
    /// Whether the deadline has passed.
    pub fn is_expired(&self) -> bool {
        self.remaining() == Some(Duration::ZERO)
    }
    /// The cancellation token.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }
    /// Resolves when the call is cancelled.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }
    /// Authenticated caller, if any.
    pub fn caller(&self) -> Option<&ServiceIdentity> {
        self.caller.as_ref()
    }
    /// End-user subject, if any.
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }
    /// Tenant, if any.
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }
    /// Idempotency key, if any.
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }
    /// W3C `traceparent`, if any.
    pub fn traceparent(&self) -> Option<&str> {
        self.traceparent.as_deref()
    }

    /// A child for an outbound call: same identity and deadline, a child
    /// cancellation token, and the caller cleared (the callee learns its
    /// caller from authentication, not from us).
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            request_id: self.request_id.clone(),
            deadline: self.deadline,
            cancel: self.cancel.child_token(),
            caller: None,
            subject: self.subject.clone(),
            tenant: self.tenant.clone(),
            idempotency_key: self.idempotency_key.clone(),
            traceparent: self.traceparent.clone(),
        }
    }

    /// A context for work queued beyond this call's lifetime: identity and
    /// trace are kept, deadline and cancellation are not.
    #[must_use]
    pub fn detached(&self) -> Self {
        Self {
            request_id: self.request_id.clone(),
            deadline: None,
            cancel: CancellationToken::new(),
            caller: None,
            subject: self.subject.clone(),
            tenant: self.tenant.clone(),
            idempotency_key: self.idempotency_key.clone(),
            traceparent: self.traceparent.clone(),
        }
    }

    /// Drop `subject` and `tenant` unless the caller is a trusted link.
    #[must_use]
    pub fn sanitize_for_caller(mut self) -> Self {
        if !self.caller.as_ref().is_some_and(|caller| caller.trusted) {
            self.subject = None;
            self.tenant = None;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated() -> CallContext {
        CallContext::new()
            .with_request_id("req-orders-1")
            .with_caller(ServiceIdentity::trusted("billing"))
            .with_subject("user-7")
            .with_tenant("tenant-a")
            .with_idempotency_key("order-42")
            .with_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
    }

    #[test]
    fn identities_record_whether_the_caller_is_trusted() {
        let untrusted = ServiceIdentity::untrusted("orders");
        assert_eq!(untrusted.name, "orders");
        assert!(!untrusted.trusted);
        let trusted = ServiceIdentity::trusted(String::from("billing"));
        assert_eq!(trusted.name, "billing");
        assert!(trusted.trusted);
        assert_ne!(untrusted, ServiceIdentity::trusted("orders"));
    }

    #[test]
    fn a_new_context_is_empty_with_a_fresh_v7_request_id() {
        let ctx = CallContext::default();
        let id = uuid::Uuid::parse_str(ctx.request_id()).unwrap();
        assert_eq!(id.get_version_num(), 7);
        assert_ne!(ctx.request_id(), CallContext::new().request_id());
        assert_eq!(ctx.deadline(), None);
        assert_eq!(ctx.remaining(), None);
        assert!(!ctx.is_expired());
        assert!(!ctx.cancel_token().is_cancelled());
        assert_eq!(ctx.caller(), None);
        assert_eq!(ctx.subject(), None);
        assert_eq!(ctx.tenant(), None);
        assert_eq!(ctx.idempotency_key(), None);
        assert_eq!(ctx.traceparent(), None);
    }

    #[test]
    fn builders_set_every_field() {
        let ctx = populated();
        assert_eq!(ctx.request_id(), "req-orders-1");
        assert_eq!(ctx.caller(), Some(&ServiceIdentity::trusted("billing")));
        assert_eq!(ctx.subject(), Some("user-7"));
        assert_eq!(ctx.tenant(), Some("tenant-a"));
        assert_eq!(ctx.idempotency_key(), Some("order-42"));
        assert_eq!(
            ctx.traceparent(),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        );
    }

    #[test]
    fn a_deadline_only_ever_narrows() {
        let base = Instant::now();
        let near = base + Duration::from_secs(5);
        let far = base + Duration::from_mins(1);

        let ctx = CallContext::new().with_deadline(far);
        assert_eq!(ctx.deadline(), Some(far));
        let ctx = ctx.with_deadline(near);
        assert_eq!(ctx.deadline(), Some(near));
        let ctx = ctx.with_deadline(far);
        assert_eq!(ctx.deadline(), Some(near), "a later deadline is ignored");
    }

    #[test]
    fn a_timeout_becomes_a_deadline_relative_to_now() {
        let before = Instant::now();
        let ctx = CallContext::new().with_timeout(Duration::from_hours(1));
        let deadline = ctx.deadline().unwrap();
        assert!(deadline >= before + Duration::from_hours(1));
        assert!(deadline <= Instant::now() + Duration::from_hours(1));

        let remaining = ctx.remaining().unwrap();
        assert!(remaining > Duration::ZERO);
        assert!(remaining <= Duration::from_hours(1));
        assert!(!ctx.is_expired());

        let narrowed = ctx.with_timeout(Duration::from_hours(2));
        assert_eq!(narrowed.deadline(), Some(deadline));
    }

    #[test]
    fn a_passed_deadline_leaves_zero_time_and_is_expired() {
        let ctx = CallContext::new().with_deadline(Instant::now());
        assert_eq!(ctx.remaining(), Some(Duration::ZERO));
        assert!(ctx.is_expired());
    }

    #[tokio::test]
    async fn cancelling_the_token_resolves_cancelled() {
        let token = CancellationToken::new();
        let ctx = CallContext::new().with_cancel(token.clone());
        assert!(!ctx.cancel_token().is_cancelled());

        token.cancel();

        ctx.cancelled().await;
        assert!(ctx.cancel_token().is_cancelled());
    }

    #[test]
    fn a_child_keeps_identity_and_deadline_but_not_the_caller() {
        let deadline = Instant::now() + Duration::from_secs(30);
        let parent = populated().with_deadline(deadline);

        let child = parent.child();

        assert_eq!(child.request_id(), parent.request_id());
        assert_eq!(child.deadline(), Some(deadline));
        assert_eq!(child.caller(), None);
        assert_eq!(child.subject(), parent.subject());
        assert_eq!(child.tenant(), parent.tenant());
        assert_eq!(child.idempotency_key(), parent.idempotency_key());
        assert_eq!(child.traceparent(), parent.traceparent());
    }

    #[tokio::test]
    async fn cancellation_flows_from_parent_to_child_only() {
        let parent = CallContext::new();
        let first = parent.child();
        first.cancel_token().cancel();
        assert!(!parent.cancel_token().is_cancelled());

        let second = parent.child();
        parent.cancel_token().cancel();
        second.cancelled().await;
        assert!(second.cancel_token().is_cancelled());
    }

    #[test]
    fn a_detached_context_drops_deadline_cancellation_and_caller() {
        let parent = populated().with_timeout(Duration::from_secs(1));

        let detached = parent.detached();
        parent.cancel_token().cancel();

        assert_eq!(detached.request_id(), parent.request_id());
        assert_eq!(detached.deadline(), None);
        assert!(!detached.cancel_token().is_cancelled());
        assert_eq!(detached.caller(), None);
        assert_eq!(detached.subject(), Some("user-7"));
        assert_eq!(detached.tenant(), Some("tenant-a"));
        assert_eq!(detached.idempotency_key(), Some("order-42"));
        assert_eq!(detached.traceparent(), parent.traceparent());
    }

    #[test]
    fn only_a_trusted_caller_keeps_subject_and_tenant() {
        let trusted = populated().sanitize_for_caller();
        assert_eq!(trusted.subject(), Some("user-7"));
        assert_eq!(trusted.tenant(), Some("tenant-a"));

        let untrusted = populated()
            .with_caller(ServiceIdentity::untrusted("orders"))
            .sanitize_for_caller();
        assert_eq!(untrusted.subject(), None);
        assert_eq!(untrusted.tenant(), None);
        assert_eq!(untrusted.idempotency_key(), Some("order-42"));

        let anonymous = CallContext::new()
            .with_subject("user-7")
            .with_tenant("tenant-a")
            .sanitize_for_caller();
        assert_eq!(anonymous.subject(), None);
        assert_eq!(anonymous.tenant(), None);
    }
}
