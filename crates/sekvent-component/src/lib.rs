//! Components: units of business logic behind a typed contract, wired into
//! one App and called through bindings chosen by configuration.
//!
//! A component is a trait marked `#[component(...)]`. The macro generates a
//! cloneable handle (`InventoryHandle`) whose methods mirror the trait's, and
//! the plumbing that lets the same handle call the implementation directly
//! (`local`), across a serialization boundary on a task of its own
//! (`local-serialized`), or in another process over gRPC (`grpc`, with the
//! `grpc` feature), with identical results. The trait's proto service is its
//! contract: `proto = "..."` points the macro at the module
//! `sekvent-proto-build` generated, and a mismatch fails the build.
//!
//! ```ignore
//! let mut builder = App::builder(&EnvSource);
//! InventoryHandle::install(&mut builder, |_deps| Ok(InventoryService::default()))?;
//! OrdersHandle::install(&mut builder, |deps| {
//!     Ok(OrdersService::new(deps.handle::<InventoryHandle>()?))
//! })?;
//! let app = builder.build()?;
//! app.start().await?;
//! ```
//!
//! # Building
//!
//! [`AppBuilder::build`] fails closed: unknown `SEKVENT_COMPONENT_*` keys,
//! malformed values, unavailable bindings, mode mismatches, colliding keys
//! and two exposed components serving one gRPC service name are all
//! reported together, naming keys and never values, before any factory
//! runs. Factories then run in install order and may take the handles of
//! components installed before them.
//!
//! # Configuration
//!
//! | Key | Values |
//! |---|---|
//! | `SEKVENT_COMPONENT_BINDING` | `local` (default), `local-serialized`, `grpc` |
//! | `SEKVENT_COMPONENT_MAX_HOPS` | deepest chain of component calls, 1–1000 (default 16) |
//! | `SEKVENT_COMPONENT_<C>_BINDING` | same, for component `C` |
//! | `SEKVENT_COMPONENT_<C>_ENDPOINT` | `http://host:port` of `C` under `grpc` |
//! | `SEKVENT_COMPONENT_<C>_LINK` | link whose `SEKVENT_LINK_OUTBOUND_<LINK>` token is presented (default `C`) |
//! | `SEKVENT_COMPONENT_<C>_AUTH` | `link` (default) or `none` |
//! | `SEKVENT_COMPONENT_<C>_SERVE` | `none` (default) or `grpc`: expose a locally bound `C` |
//! | `SEKVENT_COMPONENT_<C>_SERVE_AUTH` | `link` (default) or `none` |
//! | `SEKVENT_COMPONENT_<C>_POLICY`, `…_<C>_<M>_POLICY` | a named policy, `SEKVENT_POLICY_<N>_*` |
//! | `SEKVENT_COMPONENT_<C>_<FIELD>` | resilience field for every method of `C` |
//! | `SEKVENT_COMPONENT_<C>_<M>_<FIELD>` | resilience field of method `M` |
//!
//! Fields are those of `sekvent_resilience::PolicySpec` except `RATE_LIMIT_*`:
//! `TIMEOUT` (caller side, and serving side for gRPC-served calls), the
//! `BULKHEAD_*` fields (serving side), the `RETRY_*` fields (the `grpc`
//! caller, idempotent methods only) and, at component level only, the retry
//! budget and `BREAKER_*` fields (one breaker and budget per remote
//! component). Precedence, lowest first: framework default (three attempts,
//! breaker on), `#[call]` attribute, named policy, component keys, method
//! keys. The key set does not depend on the binding. Without the `grpc`
//! feature, `grpc` parses but is not available.
//!
//! # Calls
//!
//! A call is shed before any work when its context is cancelled or past its
//! deadline, runs within the method's timeout (never past the caller's
//! deadline), and is rejected with `RESOURCE_EXHAUSTED` when the method's
//! bulkhead is full or with `UNAVAILABLE` when the component is not serving.
//! Typed errors ([`ComponentError`](trait@ComponentError)) travel as [`AppError`]s and decode back
//! into their variant.
//!
//! # Lifecycle
//!
//! [`App::start`] runs the optional [`Lifecycle`] hooks in install order and
//! [`App::stop`] drains and stops the components in reverse order. Start and
//! stop never overlap: a stop during the start cancels the `on_start` still
//! running, and every component that started is stopped exactly once, on a
//! task of its own, so dropping either future never leaves the App half
//! started or stuck stopping. With the `runtime` feature, `App::register`
//! runs the whole App as one `sekvent-runtime` unit, fits its drain within
//! the runtime's shutdown deadline so the `on_stop` hooks keep time to run,
//! and keeps the health of the exposed gRPC services in step.
//!
//! # Serving over gRPC
//!
//! With the `grpc` feature, `App::grpc_routes` serves every component the
//! configuration exposes, on paths `/<package>.<Trait>/<Rpc>`, through the
//! same gate, bulkheads and deadlines as in-process calls, after checking
//! the caller's link token.

#![forbid(unsafe_code)]

// Generated code names this crate `::sekvent_component`, inside it too.
extern crate self as sekvent_component;

mod app;
mod binding;
mod config;
mod contract;
mod descriptor;
mod error;
#[cfg(feature = "grpc")]
mod grpc;
mod lifecycle;
mod link;
mod policy;
#[cfg(feature = "runtime")]
mod runtime;
mod server;
mod wire;

#[doc(hidden)]
pub mod __private;
pub mod reasons;

pub use app::{App, AppBuilder, ComponentState, Deps};
pub use binding::{Binding, ComponentMode};
pub use descriptor::{ComponentDescriptor, ComponentHandle, MethodDescriptor, MethodKind};
pub use error::{BuildError, ComponentError};
pub use lifecycle::Lifecycle;
pub use sekvent_context::CallContext;
pub use sekvent_error::{AppError, ErrorCode};
/// `#[component(...)]` on a trait, and `#[derive(ComponentError)]` on an
/// error enum.
#[cfg(feature = "macros")]
pub use sekvent_macros::{ComponentError, component};

/// Caller name a component sees for calls made in this process, under both
/// local bindings. Reserved: no link may be named `local`, so a remote caller
/// is never mistaken for an in-process one.
pub const LOCAL_CALLER: &str = "local";
/// Prefix of every component configuration key.
pub const CONFIG_PREFIX: &str = "SEKVENT_COMPONENT_";
/// Default binding for every standard component without its own binding key.
pub const DEFAULT_BINDING_KEY: &str = "SEKVENT_COMPONENT_BINDING";
/// Deepest chain of component calls a handle makes or a gRPC-served
/// component accepts.
pub const MAX_HOPS_KEY: &str = "SEKVENT_COMPONENT_MAX_HOPS";
/// Default of [`MAX_HOPS_KEY`].
pub const DEFAULT_MAX_HOPS: u32 = 16;
/// Prefix of named policy keys, `SEKVENT_POLICY_<NAME>_<FIELD>`.
pub const POLICY_PREFIX: &str = "SEKVENT_POLICY_";
