//! Testcontainers harness for Postgres and MySQL with crash-safe cleanup.
//!
//! This crate is for tests only: add it under `[dev-dependencies]`.
//!
//! # What it gives you
//!
//! - [`Harness`]: the label namespace and run id stamped on every container
//!   this crate starts, so cleanup can select exactly this harness's
//!   resources and nothing else.
//! - `PostgresHarness` (feature `postgres`) and `MySqlHarness` (feature
//!   `mysql`): one database server per test binary, started lazily and
//!   shared by every test in it, with a fresh, uniquely named database per
//!   test via `create_database()`.
//! - [`Reaper`]: a detached watchdog process that removes a container once the
//!   test process is gone, even when it was killed with `SIGKILL`.
//! - [`container_addr`]: the address a container is reachable on, honouring
//!   [`HOST_OVERRIDE_ENV`] for remote Docker daemons.
//! - [`Harness::sweep_run`] and [`Harness::sweep_stale`]: label-scoped cleanup
//!   for leftovers, used by the CLI.
//! - [`await_until!`]: a bounded poll for tests that wait on an observable
//!   condition.
//!
//! # Running the container tests
//!
//! Tests that need Docker are `#[ignore]`d and also check
//! [`DOCKER_TESTS_ENV`], so a plain `cargo test` never touches Docker:
//!
//! ```sh
//! SEKVENT_DOCKER_TESTS=1 cargo test -p sekvent-testing --all-features -- --ignored
//! ```
//!
//! With a remote daemon, export `DOCKER_HOST` and set
//! `TESTCONTAINERS_HOST_OVERRIDE` to the host that publishes the ports.
//! `SEKVENT_HARNESS_NAMESPACE` ([`HARNESS_NAMESPACE_ENV`]) picks the label
//! namespace of the shared containers, and `SEKVENT_TEST_RUN_ID`
//! ([`RUN_ID_ENV`]) their run id.
//!
//! # Credentials and exposure
//!
//! Every server container gets a fresh random admin password, so a
//! container's published port cannot be logged into with a well-known
//! password. The port is published on all of the daemon host's interfaces:
//! a test process running in a sibling container reaches it through the
//! Docker bridge gateway, which a loopback-only binding would not serve.
//! The URLs carry that password; [`TestDatabase`]'s `Debug` and the
//! harnesses' `Debug` print no URL.
//!
//! # Safety note
//!
//! The crate is `#![forbid(unsafe_code)]`. That rules out `pre_exec` hooks
//! (for example `prctl(PR_SET_PDEATHSIG)`) on the reaper; the reaper instead
//! relies on its stdin pipe closing when the parent dies, and joins its own
//! process group through the safe
//! [`std::os::unix::process::CommandExt::process_group`].

#![forbid(unsafe_code)]

mod addr;
mod await_until;
mod error;
mod harness;
mod reaper;
mod server;
mod sweep;

#[cfg(feature = "mysql")]
mod mysql;
#[cfg(feature = "postgres")]
mod postgres;

pub use addr::{HOST_OVERRIDE_ENV, container_addr, mapped_addr, resolve_host};
pub use await_until::{DEFAULT_AWAIT_INTERVAL, DEFAULT_AWAIT_TIMEOUT};
pub use error::HarnessError;
pub use harness::{DEFAULT_NAMESPACE, HARNESS_NAMESPACE_ENV, Harness, RUN_ID_ENV, generate_run_id};
pub use reaper::Reaper;
pub use server::{DOCKER_TESTS_ENV, ServerImage, TestDatabase, docker_tests_enabled};
pub use sweep::SweepReport;

#[cfg(feature = "mysql")]
pub use mysql::MySqlHarness;
#[cfg(feature = "postgres")]
pub use postgres::PostgresHarness;

#[doc(hidden)]
pub mod __private {
    pub use crate::await_until::Poller;
}
