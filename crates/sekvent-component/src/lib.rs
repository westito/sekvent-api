//! Components: units of business logic behind a typed contract, wired into
//! one App and called through bindings chosen by configuration.
//!
//! A component is a trait marked `#[component(...)]`. The macro generates a
//! cloneable handle (`InventoryHandle`) whose methods mirror the trait's, and
//! the plumbing that lets the same handle call the implementation directly
//! (`local`) or across a serialization boundary on a task of its own
//! (`local-serialized`), with identical results.
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
//! malformed values, unavailable bindings, mode mismatches and colliding keys
//! are all reported together, naming keys and never values, before any
//! factory runs. Factories then run in install order and may take the
//! handles of components installed before them.
//!
//! # Configuration
//!
//! | Key | Values |
//! |---|---|
//! | `SEKVENT_COMPONENT_BINDING` | `local` (default), `local-serialized`, `grpc` |
//! | `SEKVENT_COMPONENT_<C>_BINDING` | same, for component `C` |
//! | `SEKVENT_COMPONENT_<C>_TIMEOUT` | default call timeout of `C`'s methods |
//! | `SEKVENT_COMPONENT_<C>_BULKHEAD_MAX_CONCURRENT` | default bulkhead of each of `C`'s methods |
//! | `SEKVENT_COMPONENT_<C>_<M>_TIMEOUT` | timeout of method `M` |
//! | `SEKVENT_COMPONENT_<C>_<M>_BULKHEAD_MAX_CONCURRENT` | bulkhead of method `M` |
//!
//! `grpc` parses but is not available in this build.
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
//! [`App::stop`] drains and stops the components in reverse order. With the
//! `runtime` feature, `App::register` runs the whole App as one
//! `sekvent-runtime` unit.

#![forbid(unsafe_code)]

// Generated code names this crate `::sekvent_component`, inside it too.
extern crate self as sekvent_component;

mod app;
mod binding;
mod config;
mod descriptor;
mod error;
mod lifecycle;
mod link;
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
/// local bindings.
pub const LOCAL_CALLER: &str = "local";
/// Prefix of every component configuration key.
pub const CONFIG_PREFIX: &str = "SEKVENT_COMPONENT_";
/// Default binding for every standard component without its own binding key.
pub const DEFAULT_BINDING_KEY: &str = "SEKVENT_COMPONENT_BINDING";
