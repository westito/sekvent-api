//! Tracing initialisation, in-memory log buffer, request ids and the
//! per-request access log.
//!
//! - [`init`] installs the process-wide subscriber once: a filter from
//!   `SEKVENT_LOG` / `RUST_LOG`, a compact, pretty or JSON formatter on
//!   stderr that stamps every event with the service name, the bridge for the
//!   `log` crate and, optionally, a [`LogBuffer`].
//! - [`LogBuffer`] keeps the most recent records in memory, e.g. for an
//!   admin endpoint.
//! - [`request_id`] makes sure every HTTP request carries an `x-request-id`.
//! - [`access_log`] does the same and also logs one event per request
//!   (target `sekvent::access`); a stack uses one of the two layers.
//! - [`truncate_for_log`] shortens untrusted text (upstream bodies) before it
//!   reaches a log line.

#![forbid(unsafe_code)]

pub mod access_log;
mod buffer;
mod fields;
mod format;
mod init;
pub mod request_id;
#[cfg(test)]
mod test_support;
mod truncate;

pub use buffer::{LogBuffer, LogBufferLayer, LogRecord};
pub use format::LogFormat;
pub use init::{
    InitGuard, LOG_FILTER_ENV, LOG_FORMAT_ENV, RUST_LOG_ENV, TelemetryError, TelemetryOptions,
    init, try_init_for_tests,
};
pub use truncate::truncate_for_log;
