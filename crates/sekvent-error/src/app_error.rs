use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use crate::ErrorCode;

type Source = Box<dyn StdError + Send + Sync + 'static>;

/// One invalid request field, reported back to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FieldViolation {
    /// Path of the offending field, e.g. `items[2].quantity`.
    pub field: String,
    /// Caller-safe description of what is wrong.
    pub description: String,
}

/// The error type every sekvent service returns.
///
/// Everything except [`AppError::source`] is caller-visible and must be safe
/// to show: never put credentials, key material, SQL or upstream response
/// bodies into the message, reason or metadata. The source chain is for the
/// server's own logs and is never serialized.
pub struct AppError {
    // Boxed so `Result<T, AppError>` stays one pointer wide on the error path.
    inner: Box<Inner>,
}

struct Inner {
    code: ErrorCode,
    message: String,
    reason: Option<String>,
    domain: Option<String>,
    metadata: BTreeMap<String, String>,
    retry_after: Option<Duration>,
    field_violations: Vec<FieldViolation>,
    source: Option<Source>,
}

impl AppError {
    /// A new error with a caller-visible message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::from_inner(Inner {
            code,
            message: message.into(),
            reason: None,
            domain: None,
            metadata: BTreeMap::new(),
            retry_after: None,
            field_violations: Vec::new(),
            source: None,
        })
    }

    fn from_inner(inner: Inner) -> Self {
        Self {
            inner: Box::new(inner),
        }
    }

    /// `INVALID_ARGUMENT`.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }
    /// `NOT_FOUND`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }
    /// `ALREADY_EXISTS`.
    pub fn already_exists(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::AlreadyExists, message)
    }
    /// `PERMISSION_DENIED`.
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PermissionDenied, message)
    }
    /// `UNAUTHENTICATED`.
    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthenticated, message)
    }
    /// `FAILED_PRECONDITION`.
    pub fn failed_precondition(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::FailedPrecondition, message)
    }
    /// `UNAVAILABLE`.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }
    /// `DEADLINE_EXCEEDED`.
    pub fn deadline_exceeded(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::DeadlineExceeded, message)
    }
    /// `RESOURCE_EXHAUSTED`.
    pub fn resource_exhausted(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ResourceExhausted, message)
    }
    /// `UNIMPLEMENTED`.
    pub fn unimplemented(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unimplemented, message)
    }
    /// `CANCELLED`.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Cancelled, message)
    }

    /// `INTERNAL` with a fixed, caller-safe message; the cause goes to the
    /// source chain only.
    pub fn internal(source: impl Into<Source>) -> Self {
        Self::new(ErrorCode::Internal, "internal error").with_source(source)
    }

    /// Machine-readable reason, `UPPER_SNAKE_CASE` by convention
    /// (`google.rpc.ErrorInfo.reason`).
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.inner.reason = Some(reason.into());
        self
    }
    /// Domain the reason belongs to, e.g. the service name.
    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.inner.domain = Some(domain.into());
        self
    }
    /// Caller-visible key/value context.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner.metadata.insert(key.into(), value.into());
        self
    }
    /// Hint for when a retry may succeed.
    #[must_use]
    pub fn with_retry_after(mut self, after: Duration) -> Self {
        self.inner.retry_after = Some(after);
        self
    }
    /// Add a field violation.
    #[must_use]
    pub fn with_field_violation(
        mut self,
        field: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        self.inner.field_violations.push(FieldViolation {
            field: field.into(),
            description: description.into(),
        });
        self
    }
    /// Attach the internal cause. Logged by the server, never sent.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<Source>) -> Self {
        self.inner.source = Some(source.into());
        self
    }

    /// The status code.
    pub fn code(&self) -> ErrorCode {
        self.inner.code
    }
    /// The caller-visible message.
    pub fn message(&self) -> &str {
        &self.inner.message
    }
    /// The machine-readable reason, if any.
    pub fn reason(&self) -> Option<&str> {
        self.inner.reason.as_deref()
    }
    /// The reason's domain, if any.
    pub fn domain(&self) -> Option<&str> {
        self.inner.domain.as_deref()
    }
    /// Caller-visible metadata.
    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.inner.metadata
    }
    /// Retry hint, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        self.inner.retry_after
    }
    /// Field violations, if any.
    pub fn field_violations(&self) -> &[FieldViolation] {
        &self.inner.field_violations
    }
    /// See [`ErrorCode::is_transient`].
    pub fn is_transient(&self) -> bool {
        self.inner.code.is_transient()
    }

    /// The caller-visible part, detached from the internal source.
    pub fn to_wire(&self) -> WireError {
        WireError {
            code: self.inner.code.as_str().to_owned(),
            message: self.inner.message.clone(),
            reason: self.inner.reason.clone(),
            domain: self.inner.domain.clone(),
            metadata: self.inner.metadata.clone(),
            retry_after_ms: self
                .inner
                .retry_after
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            field_violations: self.inner.field_violations.clone(),
        }
    }

    /// Rebuild from the wire form. Unknown code names become `UNKNOWN`.
    pub fn from_wire(wire: WireError) -> Self {
        Self::from_inner(Inner {
            code: ErrorCode::parse(&wire.code).unwrap_or(ErrorCode::Unknown),
            message: wire.message,
            reason: wire.reason,
            domain: wire.domain,
            metadata: wire.metadata,
            retry_after: wire.retry_after_ms.map(Duration::from_millis),
            field_violations: wire.field_violations,
            source: None,
        })
    }
}

impl fmt::Debug for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppError")
            .field("code", &self.inner.code)
            .field("message", &self.inner.message)
            .field("reason", &self.inner.reason)
            .field("domain", &self.inner.domain)
            .field("metadata", &self.inner.metadata)
            .field("retry_after", &self.inner.retry_after)
            .field("field_violations", &self.inner.field_violations)
            .field("source", &self.inner.source)
            .finish()
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.inner.code, self.inner.message)
    }
}

impl StdError for AppError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner
            .source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

/// The serializable, caller-visible form of an [`AppError`].
///
/// Shared by the HTTP JSON body and by persisted dead-letter rows, so both
/// read back into the same `AppError`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WireError {
    /// [`ErrorCode::as_str`] name.
    pub code: String,
    /// Caller-visible message.
    pub message: String,
    /// Machine-readable reason.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub reason: Option<String>,
    /// Reason domain.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub domain: Option<String>,
    /// Caller-visible metadata.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "BTreeMap::is_empty")
    )]
    pub metadata: BTreeMap<String, String>,
    /// Retry hint in milliseconds.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub retry_after_ms: Option<u64>,
    /// Field violations.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub field_violations: Vec<FieldViolation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_one_pointer_wide() {
        assert_eq!(size_of::<AppError>(), size_of::<usize>());
    }
}
