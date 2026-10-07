//! Transport-agnostic error model.
//!
//! [`ErrorCode`] is the canonical gRPC code set and stays exhaustive: adding a
//! variant would break every downstream `match`. [`AppError`] separates what a
//! caller may see (code, public message, reason, metadata, field violations)
//! from what only the server may log (the internal source chain), and the
//! internal part is never serialized.
//!
//! Wire mappings live behind features:
//! - `http`: axum `IntoResponse` with a JSON body ([`http`] module).
//! - `grpc`: `tonic::Status` with `google.rpc.Status` details in the standard
//!   `grpc-status-details-bin` trailer, plus an optional project-specific
//!   binary trailer ([`grpc`] module).
//! - `serde`: the [`WireError`] form shared by HTTP bodies and dead-letter rows.
//!
//! The serving-boundary conversions (`From<AppError> for tonic::Status` and
//! `IntoResponse for AppError`) log a server-side failure (`UNKNOWN`,
//! `INTERNAL`, `DATA_LOSS`) once before answering: one `error` event, target
//! `sekvent::error`, message `request failed`, with the `code`, the `reason`
//! and the `source` chain joined with `": "` and capped at 2 KiB. The caller
//! still sees only the wire form; the message and metadata are not logged.
//! Other codes are caller errors and are not logged. The plain encoders
//! (`grpc::to_status`, [`AppError::to_wire`]) never log.

#![forbid(unsafe_code)]

mod app_error;
mod code;

pub use app_error::{AppError, FieldViolation, WireError};
pub use code::ErrorCode;

#[cfg(any(feature = "grpc", feature = "http"))]
mod boundary;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "http")]
pub mod http;
#[cfg(all(test, any(feature = "grpc", feature = "http")))]
mod test_support;

#[cfg(any(feature = "grpc", feature = "http"))]
pub use boundary::log_server_side;

/// Result alias used across sekvent crates.
pub type Result<T, E = AppError> = std::result::Result<T, E>;
