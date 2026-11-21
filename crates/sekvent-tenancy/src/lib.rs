//! Tenants on a Postgres database and a role each.
//!
//! The layer above the multi-tenant building blocks for a product that serves
//! many tenants from one process, each with a database and a database role of
//! its own. This release holds the leaves: tenant ids and the object names
//! made from them, host and label canonicalisation, role passwords derived
//! from a master secret, the connector for tenant pools, privilege
//! inspection, the startup preflight, and the cluster operations a
//! maintenance role runs (roles, databases, the ownership handover).
//!
//! The crate reads no configuration key: it offers parsers and builders the
//! product calls with its own values.
//!
//! # Features
//!
//! | Feature | Adds |
//! |---|---|
//! | (none) | [`TenantId`], [`ObjectNames`], [`TenantRole`], [`DatabaseName`], key ids, [`host`], [`label`], [`reasons`], [`ErrorDomains`] |
//! | `serving` (default) | role secrets ([`secrets`]: derivation), the [`Connector`], [`cluster::privileges`], [`cluster::preflight`] |
//! | `lifecycle` | the SCRAM verifier, the maintenance connection, [`cluster::roles`], [`cluster::databases`], [`cluster::handover`] (turns on `serving`) |
//! | `admin` | not built yet (turns on `serving`) |
//! | `manager` | not built yet; needs no database driver |
//! | `runtime` | not built yet (turns on `serving`) |
//!
//! `privileges` and `preflight` sit in `serving`, not `lifecycle`: a process
//! that serves tenants without holding the maintenance credential still
//! checks at startup that no tenant role reaches its own databases.
//!
//! # Errors
//!
//! Functions return [`AppError`](sekvent_error::AppError). Drift found on the
//! cluster, a refused name and a misconfiguration are `INTERNAL` without a
//! reason; the cause is in the error's source, which is logged and never
//! sent. Database errors map through `sekvent_db::to_app_error`. No error
//! carries a URL, connect options, a role password or a verifier.

#![forbid(unsafe_code)]

mod error;
mod id;
mod names;

pub mod host;
pub mod label;

#[cfg(feature = "serving")]
pub mod cluster;
#[cfg(feature = "serving")]
mod connector;
#[cfg(feature = "serving")]
pub mod secrets;

pub use error::{ErrorDomains, reasons};
pub use id::TenantId;
pub use names::{DatabaseName, KEY_ID_MAX, ObjectNames, TenantRole, is_adoptable_name, is_key_id};

#[cfg(feature = "serving")]
pub use connector::Connector;
#[cfg(feature = "serving")]
pub use secrets::{RoleKey, RoleSecrets};

#[cfg(feature = "lifecycle")]
pub use cluster::{Maintenance, MaintenanceConn, MaintenancePool};
#[cfg(feature = "lifecycle")]
pub use secrets::ScramVerifier;
