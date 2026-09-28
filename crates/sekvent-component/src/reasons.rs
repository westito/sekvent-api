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
/// The remote component's circuit breaker is open (`UNAVAILABLE`); set by
/// `sekvent-resilience`.
pub const CIRCUIT_OPEN: &str = "CIRCUIT_OPEN";
/// The remote component could not be reached (`UNAVAILABLE`).
pub const UNREACHABLE: &str = "COMPONENT_UNREACHABLE";
/// The call would exceed `SEKVENT_COMPONENT_MAX_HOPS` (`FAILED_PRECONDITION`).
pub const CALL_DEPTH_EXCEEDED: &str = "CALL_DEPTH_EXCEEDED";
/// The gRPC path names no method of the component (`UNIMPLEMENTED`).
pub const UNKNOWN_METHOD: &str = "UNKNOWN_METHOD";
/// A component is exposed over gRPC but its routes were never mounted
/// (`FAILED_PRECONDITION`).
pub const GRPC_NOT_MOUNTED: &str = "GRPC_NOT_MOUNTED";
/// The method's own `TIMEOUT` expired before the callee answered, while the
/// caller still had time left (`DEADLINE_EXCEEDED`); counted as a failure of
/// the callee by its circuit breaker.
pub const METHOD_TIMEOUT: &str = "METHOD_TIMEOUT";
/// A transient failure of a component further down the call chain, turned
/// into `INTERNAL` by the component that saw it, so callers above neither
/// retry it nor count it against the healthy component in between.
pub const DOWNSTREAM_FAILURE: &str = "DOWNSTREAM_FAILURE";
/// A component method panicked (`INTERNAL`).
pub const HANDLER_PANICKED: &str = "HANDLER_PANICKED";
