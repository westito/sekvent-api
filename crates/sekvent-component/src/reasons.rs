//! Machine-readable reasons ([`AppError::reason`](sekvent_error::AppError::reason))
//! of the errors the component framework itself produces.

/// The component has not been started yet (`UNAVAILABLE`).
pub const NOT_STARTED: &str = "COMPONENT_NOT_STARTED";
/// The component is stopping and accepts no new calls (`UNAVAILABLE`).
pub const DRAINING: &str = "COMPONENT_DRAINING";
/// The component has stopped (`UNAVAILABLE`).
pub const STOPPED: &str = "COMPONENT_STOPPED";
/// The method's bulkhead is full (`RESOURCE_EXHAUSTED`); set by
/// `sekvent-resilience`.
pub const BULKHEAD_FULL: &str = "BULKHEAD_FULL";
/// The request bytes did not decode (`INVALID_ARGUMENT`).
pub const MALFORMED_REQUEST: &str = "MALFORMED_REQUEST";
/// The reply or error bytes did not decode (`INTERNAL`).
pub const MALFORMED_REPLY: &str = "MALFORMED_REPLY";
