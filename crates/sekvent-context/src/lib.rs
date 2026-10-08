//! Per-call context carried through every handler, client and queue message.
//!
//! In process the deadline is an absolute [`Instant`]; on the wire it travels
//! as a relative `grpc-timeout` (see [`headers`]). Work that is queued for
//! later never inherits the caller's deadline or cancellation: use
//! [`CallContext::detached`].

#![forbid(unsafe_code)]

mod clock;
mod end_user;
pub mod headers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

pub use clock::{Clock, ManualClock, SystemClock};
pub use end_user::EndUser;

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
///
/// The direct caller is either a service ([`caller`](Self::caller)) or an
/// end user ([`end_user`](Self::end_user)), never both.
#[derive(Debug, Clone)]
pub struct CallContext {
    request_id: String,
    deadline: Option<Instant>,
    cancel: CancellationToken,
    caller: Option<ServiceIdentity>,
    end_user: Option<Arc<EndUser>>,
    subject: Option<String>,
    tenant: Option<String>,
    idempotency_key: Option<String>,
    /// Whether the idempotency key came in with this call rather than being
    /// set for it: such a key names the caller's operation and is never
    /// forwarded to another one.
    key_received: bool,
    traceparent: Option<String>,
    hops: u32,
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
            end_user: None,
            subject: None,
            tenant: None,
            idempotency_key: None,
            key_received: false,
            traceparent: None,
            hops: 0,
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
    /// Set a deadline relative to now (same narrowing rule). A timeout too
    /// large to represent as an instant sets no tighter deadline.
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => self.with_deadline(deadline),
            None => self,
        }
    }
    /// Use this cancellation token.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }
    /// Set the authenticated caller. Clears the end user: a call has one
    /// direct caller.
    #[must_use]
    pub fn with_caller(mut self, caller: ServiceIdentity) -> Self {
        self.caller = Some(caller);
        self.end_user = None;
        self
    }
    /// Make an authenticated end user the direct caller: subject and tenant
    /// become theirs (a tenant the user has none of is cleared) and the
    /// service caller is cleared.
    #[must_use]
    pub fn with_end_user(mut self, user: EndUser) -> Self {
        self.subject = Some(user.subject().to_owned());
        self.tenant = user.tenant().map(str::to_owned);
        self.caller = None;
        self.end_user = Some(Arc::new(user));
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
    /// Set the idempotency key of this call: it is forwarded to the callee
    /// of the next outbound call made with this context (or a
    /// [`child`](Self::child) of it).
    #[must_use]
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self.key_received = false;
        self
    }
    /// The context as the callee of a call sees it: an idempotency key it
    /// carries was received with the call, so it stays readable through
    /// [`idempotency_key`](Self::idempotency_key) but is no longer forwarded
    /// by [`child`](Self::child) or [`headers::inject`]. A key identifies one
    /// operation; handing it to a different one would make that callee treat
    /// distinct requests as duplicates.
    /// [`headers::from_headers`] returns contexts in this state.
    #[must_use]
    pub fn into_inbound(mut self) -> Self {
        self.key_received = self.idempotency_key.is_some();
        self
    }
    /// Set the W3C `traceparent`.
    #[must_use]
    pub fn with_traceparent(mut self, traceparent: impl Into<String>) -> Self {
        self.traceparent = Some(traceparent.into());
        self
    }

    /// Set how many component calls led to this one.
    #[must_use]
    pub fn with_hops(mut self, hops: u32) -> Self {
        self.hops = hops;
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
    /// The authenticated end user, when an end user made this call
    /// directly; `None` for service callers and for calls further down.
    pub fn end_user(&self) -> Option<&EndUser> {
        self.end_user.as_deref()
    }
    /// End-user subject, if any.
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }
    /// Tenant, if any.
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }
    /// Idempotency key, if any: set for this call or received with it.
    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }
    /// The idempotency key an outbound call made with this context carries:
    /// one set for this call, never one received with it (see
    /// [`into_inbound`](Self::into_inbound)).
    pub fn outbound_idempotency_key(&self) -> Option<&str> {
        if self.key_received {
            None
        } else {
            self.idempotency_key()
        }
    }
    /// W3C `traceparent`, if any.
    pub fn traceparent(&self) -> Option<&str> {
        self.traceparent.as_deref()
    }
    /// How many component calls led to this one (0 at the edge).
    pub fn hops(&self) -> u32 {
        self.hops
    }

    /// A child for an outbound call: same identity, deadline and hop count, a
    /// child cancellation token, and the caller and end user cleared (the
    /// callee learns its caller from authentication, not from us; subject
    /// and tenant stay). The idempotency key is kept
    /// only when it was set for this call, not received with it.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            request_id: self.request_id.clone(),
            deadline: self.deadline,
            cancel: self.cancel.child_token(),
            caller: None,
            end_user: None,
            subject: self.subject.clone(),
            tenant: self.tenant.clone(),
            idempotency_key: self.outbound_idempotency_key().map(str::to_owned),
            key_received: false,
            traceparent: self.traceparent.clone(),
            hops: self.hops,
        }
    }

    /// A context for work queued beyond this call's lifetime: subject,
    /// tenant, trace and hop count are kept; deadline, cancellation, caller
    /// and end user are not.
    #[must_use]
    pub fn detached(&self) -> Self {
        Self {
            request_id: self.request_id.clone(),
            deadline: None,
            cancel: CancellationToken::new(),
            caller: None,
            end_user: None,
            subject: self.subject.clone(),
            tenant: self.tenant.clone(),
            idempotency_key: self.idempotency_key.clone(),
            key_received: self.key_received,
            traceparent: self.traceparent.clone(),
            hops: self.hops,
        }
    }

    /// Drop `subject` and `tenant` unless the caller is a trusted link or an
    /// authenticated end user.
    #[must_use]
    pub fn sanitize_for_caller(mut self) -> Self {
        if self.end_user.is_none() && !self.caller.as_ref().is_some_and(|caller| caller.trusted) {
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
            .with_hops(3)
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
        assert_eq!(ctx.hops(), 0);
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
        assert_eq!(ctx.hops(), 3);
        assert_eq!(ctx.with_hops(0).hops(), 0, "the hop count is replaced");
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
    fn an_unrepresentable_timeout_adds_no_deadline() {
        let ctx = CallContext::new().with_timeout(Duration::MAX);
        assert_eq!(ctx.deadline(), None);
        let bounded = CallContext::new()
            .with_timeout(Duration::from_secs(5))
            .with_timeout(Duration::MAX);
        assert!(bounded.remaining().unwrap() <= Duration::from_secs(5));
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
        assert_eq!(child.hops(), 3);
    }

    #[test]
    fn a_received_idempotency_key_is_readable_but_not_forwarded() {
        let inbound = populated().into_inbound();
        assert_eq!(inbound.idempotency_key(), Some("order-42"));
        assert_eq!(inbound.outbound_idempotency_key(), None);
        let child = inbound.child();
        assert_eq!(child.idempotency_key(), None);
        assert_eq!(child.outbound_idempotency_key(), None);
        assert_eq!(inbound.detached().idempotency_key(), Some("order-42"));
        assert_eq!(inbound.detached().outbound_idempotency_key(), None);

        let own = inbound.with_idempotency_key("order-43");
        assert_eq!(own.outbound_idempotency_key(), Some("order-43"));
        assert_eq!(own.child().idempotency_key(), Some("order-43"));

        let set = populated();
        assert_eq!(set.outbound_idempotency_key(), Some("order-42"));
        assert_eq!(set.child().outbound_idempotency_key(), Some("order-42"));

        let keyless = CallContext::new().into_inbound();
        assert_eq!(keyless.outbound_idempotency_key(), None);
        assert_eq!(
            keyless.with_idempotency_key("k").outbound_idempotency_key(),
            Some("k"),
            "a key set after the call came in is this call's own"
        );
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
        assert_eq!(detached.hops(), 3);
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

    #[test]
    fn an_end_user_is_the_direct_caller() {
        let user = EndUser::new("user-9")
            .with_tenant("tenant-b")
            .with_roles(["admin"]);
        let ctx = populated().with_end_user(user.clone());
        assert_eq!(ctx.end_user(), Some(&user));
        assert_eq!(ctx.caller(), None);
        assert_eq!(ctx.subject(), Some("user-9"));
        assert_eq!(ctx.tenant(), Some("tenant-b"));
        let kept = ctx.clone().sanitize_for_caller();
        assert_eq!(kept.subject(), Some("user-9"));
        assert_eq!(kept.tenant(), Some("tenant-b"));

        let tenantless = populated().with_end_user(EndUser::new("user-9"));
        assert_eq!(
            tenantless.tenant(),
            None,
            "the user's tenant, not the header's"
        );

        let child = ctx.child();
        assert_eq!(child.end_user(), None);
        assert_eq!(child.subject(), Some("user-9"));
        assert_eq!(child.tenant(), Some("tenant-b"));
        let detached = ctx.detached();
        assert_eq!(detached.end_user(), None);
        assert_eq!(detached.subject(), Some("user-9"));

        let service = ctx.with_caller(ServiceIdentity::trusted("billing"));
        assert_eq!(service.end_user(), None);
        assert_eq!(service.caller(), Some(&ServiceIdentity::trusted("billing")));
        assert_eq!(CallContext::new().end_user(), None);
    }
}
