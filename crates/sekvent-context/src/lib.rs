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
