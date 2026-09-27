//! Tracing initialisation, in-memory log buffer and request-id propagation.
//!
//! - [`init`] installs the process-wide subscriber once: a filter from
//!   `SEKVENT_LOG` / `RUST_LOG`, a compact, pretty or JSON formatter on
//!   stderr that stamps every event with the service name, the bridge for the
//!   `log` crate and, optionally, a [`LogBuffer`].
//! - [`LogBuffer`] keeps the most recent records in memory, e.g. for an
//!   admin endpoint.
//! - [`request_id`] makes sure every HTTP request carries an `x-request-id`.
//! - [`truncate_for_log`] shortens untrusted text (upstream bodies) before it
//!   reaches a log line.

#![forbid(unsafe_code)]

mod buffer;
mod fields;
mod format;
mod init;
pub mod request_id;
mod truncate;

pub use buffer::{LogBuffer, LogBufferLayer, LogRecord};
pub use format::LogFormat;
pub use init::{
    InitGuard, LOG_FILTER_ENV, LOG_FORMAT_ENV, RUST_LOG_ENV, TelemetryError, TelemetryOptions,
    init, try_init_for_tests,
};
pub use truncate::truncate_for_log;
