//! Cluster operations: they take checked names and verifiers and know no
//! tenant row.
//!
//! [`privileges`] and [`preflight`] read the catalogs on any connection and
//! change nothing. With `lifecycle`, the [`maintenance`] connection runs
//! [`roles`], [`databases`] and the [`handover`] of an adoption as a role that
//! is not a superuser.

pub mod preflight;
pub mod privileges;

#[cfg(feature = "lifecycle")]
pub mod databases;
#[cfg(feature = "lifecycle")]
pub mod handover;
#[cfg(feature = "lifecycle")]
pub mod maintenance;
#[cfg(feature = "lifecycle")]
pub mod roles;
#[cfg(feature = "lifecycle")]
mod sql;

#[cfg(feature = "lifecycle")]
pub use maintenance::{Maintenance, MaintenanceConn, MaintenancePool};
