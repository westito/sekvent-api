//! Service lifecycle, supervision, health and the combined gRPC/gRPC-Web/REST server.
//!
//! # Lifecycle
//!
//! A [`Runtime`] runs named *units*: futures produced by a factory, so a
//! unit can be restarted. Each unit belongs to a [`Stage`]. Stages start in
//! order (infrastructure, components, workers, ingress); a stage counts as
//! started once every unit in it has called [`UnitContext::ready`], or, for
//! a critical or best-effort unit, exited.
//! Shutdown, triggered by a signal, a critical unit exiting or a
//! [`ShutdownTrigger`], first turns health to not-serving, waits the
//! configured delay, then drains stages in reverse order, each within its
//! grace period, all within an overall deadline.
//!
//! What happens when a unit exits on its own is its [`UnitPolicy`]:
//! critical units take everything down, restarting units back off and retry,
//! best-effort units are logged and forgotten. A panic, in a unit's future or
//! in the factory that builds it, is a failure like any other.
//!
//! # Health
//!
//! The [`HealthRegistry`] tracks liveness, readiness and per-service status,
//! fed by the lifecycle and by [`DependencyProbe`]s. It is served over HTTP
//! ([`HealthRegistry::http_routes`]) and as `grpc.health.v1`
//! ([`HealthRegistry::grpc_service`]).
//!
//! # Server
//!
//! [`Server`] serves native gRPC, gRPC-Web (feature `grpc-web`, on by
//! default) and axum REST routes on one listener, optionally under a path
//! prefix, with the health endpoints at the root. Every request carries a
//! [`CallContext`](sekvent_context::CallContext), read with the [`Ctx`]
//! extractor. The server applies validated [`Cors`], a REST body limit,
//! application tower layers ([`ServerBuilder::layer`]) and a per-request
//! access log with request ids; [`Download`] builds file responses.
//!
//! # Jobs
//!
//! [`RuntimeBuilder::job`] runs interval, cron (feature `cron`, UTC) and
//! manual jobs described by a [`JobSpec`]: a run never overlaps the
//! previous one, a failed or panicking run is logged and the next tick
//! still comes, and drain cancels the running job. A [`JobHandle`] triggers
//! runs and reports status. A [`JobGuard`] (for example a database lease)
//! turns a job into a singleton across instances.

#![forbid(unsafe_code)]

mod cors;
mod download;
mod health;
mod health_http;
mod job;
mod probe;
pub mod reasons;
mod runtime;
mod server;
mod signal;
mod stage;
mod unit;

pub use cors::Cors;
pub use download::{
    CacheControl, Disposition, Download, content_disposition, sanitize_filename, sniff_content_type,
};
pub use health::{HealthRegistry, ProbeState, Readiness, ServiceStatus};
pub use health_http::HealthVisibility;
pub use job::{
    CancelReason, JobContext, JobGuard, JobHandle, JobPermit, JobRun, JobRunOutcome, JobSpec,
    JobState, JobStatus, RunStarted, RunTrigger, Schedule, TokioWallClock, TriggerError,
    TriggerErrorKind,
};
pub use probe::{DependencyProbe, ProbeFailure, ProbeStatus};
pub use runtime::{
    PROBE_UNIT, RunReport, Runtime, RuntimeBuilder, RuntimeHandle, ShutdownReason, ShutdownTrigger,
    UnitExit, UnitReport,
};
pub use server::{Authenticator, Ctx, Server, ServerBuilder};
pub use stage::{RestartPolicy, Stage, UnitPolicy};
pub use unit::UnitContext;
