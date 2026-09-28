use std::fmt;

/// The canonical status codes, identical in meaning and number to gRPC's.
///
/// Exhaustive on purpose: downstream code matches on it, and the set is fixed
/// by the gRPC specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Not an error; present so the numbering matches gRPC.
    Ok = 0,
    /// The operation was cancelled, typically by the caller.
    Cancelled = 1,
    /// Unknown error.
    Unknown = 2,
    /// The client specified an invalid argument.
    InvalidArgument = 3,
    /// The deadline expired before the operation could complete.
    DeadlineExceeded = 4,
    /// A requested entity was not found.
    NotFound = 5,
    /// The entity a client attempted to create already exists.
    AlreadyExists = 6,
    /// The caller is authenticated but lacks permission.
    PermissionDenied = 7,
    /// A resource has been exhausted (quota, rate limit, capacity).
    ResourceExhausted = 8,
    /// The system is not in a state required for the operation.
    FailedPrecondition = 9,
    /// The operation was aborted, typically due to a concurrency conflict.
    Aborted = 10,
    /// The operation was attempted past the valid range.
    OutOfRange = 11,
    /// The operation is not implemented or supported.
    Unimplemented = 12,
    /// An invariant the system relies on has been broken.
    Internal = 13,
    /// The service is currently unavailable; retrying may succeed.
    Unavailable = 14,
    /// Unrecoverable data loss or corruption.
    DataLoss = 15,
    /// The request lacks valid authentication credentials.
    Unauthenticated = 16,
}

impl ErrorCode {
    /// Every code, in numeric order.
    pub const ALL: [ErrorCode; 17] = [
        Self::Ok,
        Self::Cancelled,
        Self::Unknown,
        Self::InvalidArgument,
        Self::DeadlineExceeded,
        Self::NotFound,
        Self::AlreadyExists,
        Self::PermissionDenied,
        Self::ResourceExhausted,
        Self::FailedPrecondition,
        Self::Aborted,
        Self::OutOfRange,
        Self::Unimplemented,
        Self::Internal,
        Self::Unavailable,
        Self::DataLoss,
        Self::Unauthenticated,
    ];

    /// The gRPC numeric value.
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Code for a gRPC numeric value; unknown numbers map to [`ErrorCode::Unknown`].
    pub fn from_i32(value: i32) -> Self {
        usize::try_from(value)
            .ok()
            .and_then(|index| Self::ALL.get(index).copied())
            .unwrap_or(Self::Unknown)
    }

    /// The `SCREAMING_SNAKE_CASE` name used by gRPC and by the JSON body.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Cancelled => "CANCELLED",
            Self::Unknown => "UNKNOWN",
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::DeadlineExceeded => "DEADLINE_EXCEEDED",
            Self::NotFound => "NOT_FOUND",
            Self::AlreadyExists => "ALREADY_EXISTS",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::ResourceExhausted => "RESOURCE_EXHAUSTED",
            Self::FailedPrecondition => "FAILED_PRECONDITION",
            Self::Aborted => "ABORTED",
            Self::OutOfRange => "OUT_OF_RANGE",
            Self::Unimplemented => "UNIMPLEMENTED",
            Self::Internal => "INTERNAL",
            Self::Unavailable => "UNAVAILABLE",
            Self::DataLoss => "DATA_LOSS",
            Self::Unauthenticated => "UNAUTHENTICATED",
        }
    }

    /// Parse the `SCREAMING_SNAKE_CASE` name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|code| code.as_str() == name)
    }

    /// The HTTP status conventionally paired with this code
    /// (the mapping in `google/rpc/code.proto`).
    pub fn http_status(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::Cancelled => 499,
            Self::InvalidArgument | Self::FailedPrecondition | Self::OutOfRange => 400,
            Self::Unauthenticated => 401,
            Self::PermissionDenied => 403,
            Self::NotFound => 404,
            Self::AlreadyExists | Self::Aborted => 409,
            Self::ResourceExhausted => 429,
            Self::Unimplemented => 501,
            Self::Unavailable => 503,
            Self::DeadlineExceeded => 504,
            Self::Unknown | Self::Internal | Self::DataLoss => 500,
        }
    }

    /// Whether a failure with this code is worth retrying at all.
    ///
    /// Only the transport-level conditions qualify. Whether a *particular*
    /// call may be retried also depends on the method being idempotent, which
    /// is the caller's decision, not the error's.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::DeadlineExceeded | Self::ResourceExhausted | Self::Aborted
        )
    }

    /// Whether this code should count as a failure for a circuit breaker:
    /// the callee (not the request) is unhealthy.
    ///
    /// The code alone cannot tell a slow dependency from a caller whose own
    /// deadline ran out; whoever reports outcomes to a breaker must not
    /// report the caller's expired deadline or cancellation at all.
    pub fn trips_breaker(self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::DeadlineExceeded | Self::ResourceExhausted
        )
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
