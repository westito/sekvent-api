//! The sekvent backend framework: one dependency for the `sekvent-*` crates.
//!
//! Each library crate is re-exported as a module behind a feature of the
//! same name, so a service depends on this package alone and pays only for
//! what it enables. The package is `sekvent-api`; its library is `sekvent`.
//!
//! | Module | Crate | Feature | What it is for |
//! |---|---|---|---|
//! | [`config`] | `sekvent-config` | `config` (default) | `ConfigSource`, `Secret`, readers, `#[derive(EnvConfig)]` |
//! | [`error`] | `sekvent-error` | `error` (default) | `ErrorCode`, `AppError`, `WireError`; HTTP and gRPC mappings |
//! | [`context`] | `sekvent-context` | `context` (default) | `CallContext`, `ServiceIdentity`, `Clock`, header codec |
//! | [`telemetry`] | `sekvent-telemetry` | `telemetry` (default) | tracing init, log buffer, request ids, access log, `truncate_for_log` |
//! | [`runtime`] | `sekvent-runtime` | `runtime` (default) | staged lifecycle, supervision, health, the combined server (CORS, limits, layers, access log), jobs, downloads |
//! | [`resilience`] | `sekvent-resilience` | `resilience` | backoff, retry budget, rate gate, timeout, bulkhead, breaker, TTL cache |
//! | [`auth`] | `sekvent-auth` | `auth` | argon2id and bcrypt password hashing, JWT with injected time, login helper |
//! | [`link`] | `sekvent-link` | `link` | service-to-service tokens, middleware, interceptors |
//! | [`client`] | `sekvent-client` | `client` | outbound HTTP with policy, context propagation, OAuth 2.0 |
//! | [`db`] | `sekvent-db` | `db` | named pools, migrations, distinct-target check, list filters, readiness probes, leases |
//! | [`component`](mod@component) | `sekvent-component` | `component` | components, the App builder, local, serialized and gRPC bindings |
//! | [`sso`] | `sekvent-sso` | `sso` | browser single sign-on: OAuth 2.0 code flow, identity providers (Bitbucket Cloud), one-time handoff codes |
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
//! - `runtime-cron`: cron schedules for jobs (`JobSpec::cron`, UTC).
//! - `auth-axum`, `auth-tonic`: bearer extractors, interceptors, role guards.
//! - `auth-tokio`: password hashing and verification on tokio's blocking
//!   pool (`hash_async`, `verify_async`, `authenticate_async`).
//! - `link-axum`, `link-tonic`: inbound middleware and interceptors.
//! - `db-sqlx-postgres`, `db-sqlx-mysql`, `db-sea-orm-postgres`,
//!   `db-sea-orm-mysql`: database backends.
//! - `db-migrate`, `db-sea-orm-migrate`: migrations on boot.
//! - `db-lease`: database leases with fencing tokens (`LeaseStore`), on a
//!   sqlx backend; with `runtime` also `LeaseGuard` for singleton jobs.
//! - `db` and `runtime` together give pool readiness probes
//!   (`PoolRegistry::probes`) on a sqlx backend.
//! - `component` also turns on `config`, `error` and `context`; with
//!   `runtime` on as well, `App::register` runs the App as a runtime unit.
//! - `component-grpc`: the component `grpc` binding and serving components
//!   over gRPC (`App::grpc_routes`).
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
//! `FromConfig`, the runtime and server builders and `JobSpec` and
//! `JobContext` for background jobs, and with `component`
//! the `App`, the `ComponentError` trait and derive, and `Lifecycle`.
//!
//! # Components
//!
//! With `component`, `#[sekvent::component(...)]` declares a component and
//! `#[derive(sekvent::ComponentError)]` its typed error; the generated code
//! finds its runtime at `sekvent::component` when `sekvent` is the only
//! dependency. The module [`component`](mod@component) and the attribute
//! [`component`](macro@component) share the name in different namespaces.

#![forbid(unsafe_code)]

/// Configuration: sources, `Secret`, readers and `FromConfig`.
#[cfg(feature = "config")]
pub use sekvent_config as config;

/// Derive `FromConfig` for a config struct.
///
/// Works with `sekvent` as the only dependency: the generated code finds the
/// runtime at `sekvent::config` (under whatever name the dependency has).
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

/// Components: the App builder, bindings, lifecycle and the support code
/// of the generated handles.
#[cfg(feature = "component")]
pub use sekvent_component as component;

/// Browser single sign-on with OAuth 2.0 identity providers.
#[cfg(feature = "sso")]
pub use sekvent_sso as sso;

/// `#[component(...)]` on a trait; the `App`; the `ComponentError` trait and
/// its derive.
#[cfg(feature = "component")]
pub use sekvent_component::{App, ComponentError, component};

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
        Ctx, JobContext, JobSpec, Runtime, RuntimeBuilder, RuntimeHandle, Server, ServerBuilder,
        ShutdownTrigger, Stage, UnitContext, UnitPolicy,
    };

    #[cfg(feature = "component")]
    pub use sekvent_component::{App, ComponentError, Lifecycle};
}
