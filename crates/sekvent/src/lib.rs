//! The sekvent backend framework: one dependency for the `sekvent-*` crates.
//!
//! Each library crate is re-exported as a module behind a feature of the
//! same name, so a service depends on `sekvent` alone and pays only for what
//! it enables.
//!
//! | Module | Crate | Feature | What it is for |
//! |---|---|---|---|
//! | [`config`] | `sekvent-config` | `config` (default) | `ConfigSource`, `Secret`, readers, `#[derive(EnvConfig)]` |
//! | [`error`] | `sekvent-error` | `error` (default) | `ErrorCode`, `AppError`, `WireError`; HTTP and gRPC mappings |
//! | [`context`] | `sekvent-context` | `context` (default) | `CallContext`, `ServiceIdentity`, `Clock`, header codec |
//! | [`telemetry`] | `sekvent-telemetry` | `telemetry` (default) | tracing init, log buffer, request ids, `truncate_for_log` |
//! | [`runtime`] | `sekvent-runtime` | `runtime` (default) | staged lifecycle, supervision, health, the combined server |
//! | [`resilience`] | `sekvent-resilience` | `resilience` | backoff, retry budget, rate gate, timeout, bulkhead, breaker |
//! | [`auth`] | `sekvent-auth` | `auth` | password hashing, JWT with injected time, login helper |
//! | [`link`] | `sekvent-link` | `link` | service-to-service tokens, middleware, interceptors |
//! | [`client`] | `sekvent-client` | `client` | outbound HTTP with policy, context propagation, OAuth 2.0 |
//! | [`db`] | `sekvent-db` | `db` | named pools, migrations, distinct-target check, list filters |
//!
//! # Sub-features
//!
//! Heavy integrations stay behind their own switches, each of which also
//! turns on its base module:
//!
//! - `error-http`, `error-grpc`: axum `IntoResponse` and `tonic::Status`
//!   mappings for `AppError`.
//! - `runtime-grpc-web`: serve gRPC-Web on the same listener as native gRPC.
//!   Off in the facade's defaults, unlike the `sekvent-runtime` crate's.
//! - `auth-axum`, `auth-tonic`: bearer extractors, interceptors, role guards.
//! - `link-axum`, `link-tonic`: inbound middleware and interceptors.
//! - `db-sqlx-postgres`, `db-sqlx-mysql`, `db-sea-orm-postgres`,
//!   `db-sea-orm-mysql`: database backends.
//! - `db-migrate`, `db-sea-orm-migrate`: migrations on boot.
//! - `full`: everything above.
//!
//! # Not re-exported
//!
//! Two crates are deliberately left out, because they belong in other
//! dependency sections:
//!
//! - `sekvent-testing` (Postgres and MySQL containers, the reaper,
//!   `await_until!`) goes under `[dev-dependencies]`.
//! - `sekvent-proto-build` (protobuf code generation) goes under
//!   `[build-dependencies]` and is driven from `build.rs`.
//!
//! # Prelude
//!
//! [`prelude`] brings the handful of names nearly every service touches:
//! `AppError`, `ErrorCode`, `CallContext`, `Secret`, `EnvConfig`,
//! `FromConfig` and the runtime and server builders.

#![forbid(unsafe_code)]

/// Configuration: sources, `Secret`, readers and `FromConfig`.
#[cfg(feature = "config")]
pub use sekvent_config as config;

/// Derive `FromConfig` for a config struct.
#[cfg(feature = "config")]
pub use sekvent_config::EnvConfig;

/// The error model and its wire mappings.
#[cfg(feature = "error")]
pub use sekvent_error as error;

/// Per-call context, identities and clocks.
#[cfg(feature = "context")]
pub use sekvent_context as context;

/// Tracing initialisation and log helpers.
#[cfg(feature = "telemetry")]
pub use sekvent_telemetry as telemetry;

/// Lifecycle, health and the combined server.
#[cfg(feature = "runtime")]
pub use sekvent_runtime as runtime;

/// Timeouts, retries, rate gates, bulkheads and circuit breakers.
#[cfg(feature = "resilience")]
pub use sekvent_resilience as resilience;

/// Passwords, JWT sessions and login.
#[cfg(feature = "auth")]
pub use sekvent_auth as auth;

/// Service-to-service authentication.
#[cfg(feature = "link")]
pub use sekvent_link as link;

/// The outbound HTTP client.
#[cfg(feature = "client")]
pub use sekvent_client as client;

/// Database pools, migrations and list filters.
#[cfg(feature = "db")]
pub use sekvent_db as db;

/// The names nearly every service uses: `use sekvent::prelude::*;`.
pub mod prelude {
    #[cfg(feature = "config")]
    pub use sekvent_config::{EnvConfig, FromConfig, Secret};

    #[cfg(feature = "error")]
    pub use sekvent_error::{AppError, ErrorCode};

    #[cfg(feature = "context")]
    pub use sekvent_context::CallContext;

    #[cfg(feature = "runtime")]
    pub use sekvent_runtime::{
        Ctx, Runtime, RuntimeBuilder, RuntimeHandle, Server, ServerBuilder, ShutdownTrigger, Stage,
        UnitContext, UnitPolicy,
    };
}
