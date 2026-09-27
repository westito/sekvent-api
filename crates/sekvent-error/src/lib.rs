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

#![forbid(unsafe_code)]

mod app_error;
mod code;

pub use app_error::{AppError, FieldViolation, WireError};
pub use code::ErrorCode;

#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "http")]
pub mod http;

/// Result alias used across sekvent crates.
pub type Result<T, E = AppError> = std::result::Result<T, E>;
