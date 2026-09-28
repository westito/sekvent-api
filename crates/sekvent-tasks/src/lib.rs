//! The task implementations behind `cargo sekvent`.
//!
//! The `cargo-sekvent` binary is a thin argument parser over this crate, and
//! a project's own `xtask` can call it too. Everything that spawns a process
//! goes through [`process::Runner`] and everything that removes containers
//! through [`harness::Sweeper`], so the tasks are tested by asserting on the
//! commands they would run.
//!
//! # Where commands run
//!
//! A developer machine never compiles. Compiling commands (`gate`, `check`,
//! `clippy`, `test`, `coverage`, `harness-clean`, remote custom tasks and the
//! `xtask` fallback) are forwarded to the remote builder as
//! `rrb run sekvent <args>` unless [`location::decide_location`] says the
//! current machine is the place to compile: inside the builder, in CI, with
//! `SEKVENT_LOCAL=1` or with `[remote].mode = "local"`. A missing or failing
//! `rrb` is an error, never a reason to compile locally.
//!
//! Commands that write into the source tree (`new`, `init`, `add`, `deps
//! sync`, `ci generate`, `agents`, `skills install`, `self-update`, `sdk
//! update`, `contract emit`) always run locally, as do the read-only `deps
//! check`, `sdk status`, `config show`, `boundaries` (it only reads `cargo
//! metadata`) and `contract check` (it compiles protos in-process, not
//! Rust).

#![forbid(unsafe_code)]

pub mod agents;
pub mod boundaries;
pub mod config;
pub mod context;
pub mod contract;
pub mod coverage;
pub mod deps;
pub mod dispatch;
pub mod embedded;
pub mod gate;
pub mod harness;
pub mod location;
pub mod metadata;
pub mod plan;
pub mod process;
pub mod remote_build;
pub mod scaffold;
pub mod sdk;
pub mod self_update;
pub mod skills;
pub mod template;

pub use config::{Config, ConfigError, Project};
pub use context::{Context, install_interrupt_handler};
pub use location::{EnvMap, Location, decide_location, process_env};
pub use process::{Cmd, Runner, SystemRunner};

#[cfg(test)]
pub(crate) mod fixtures {
    pub(crate) const METADATA: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/metadata.json"
    ));
    pub(crate) const COVERAGE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/coverage.json"
    ));
}
