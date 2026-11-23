//! Tenants on a Postgres database and a role each.
//!
//! The layer above the multi-tenant building blocks for a product that serves
//! many tenants from one process, each with a database and a database role of
//! its own. The leaves: tenant ids and the object names made from them, host
//! and label canonicalisation, role passwords derived from a master secret,
//! the connector for tenant pools, privilege inspection, the startup
//! preflight, and the cluster operations a maintenance role runs (roles,
//! databases, the ownership handover). The serving half on top of them: the
//! [`control`] database with its reference schema, tenant rows and the
//! [`Registry`] with its lookup cache and admissions; tenant selection
//! ([`SelectorRules`]) and the [`TenantResolver`] for an App; the
//! [`HostSnapshot`] and the CORS predicate of [`hosts`]; the tenant cache
//! ([`Tenants`]); and the cross-process [`invalidation`] channel. The
//! lifecycle on top of both: the `lifecycle` module's `Provisioner` with the
//! product's `TenantHooks` (provision, adopt, suspend, resume, set hosts,
//! purge, usage, describe); the `admin` surface a product serves to an
//! operator service; and the `manager` kit that operator service calls it
//! with.
//!
//! The crate reads no configuration key: it offers parsers and builders the
//! product calls with its own values.
//!
//! # Features
//!
//! | Feature | Adds |
//! |---|---|
//! | (none) | [`TenantId`], [`ObjectNames`], [`TenantRole`], [`DatabaseName`], key ids, [`host`], [`label`], [`reasons`], [`ErrorDomains`] |
//! | `serving` (default) | role secrets ([`secrets`]: derivation), the [`Connector`], [`cluster::privileges`], [`cluster::preflight`]; [`TenantInfo`], the [`control`] registry and schema, [`SelectorRules`], the [`TenantResolver`], [`hosts`], the tenant cache [`Tenants`], the [`invalidation`] channel |
//! | `lifecycle` | the SCRAM verifier, the maintenance connection, `cluster::roles`, `cluster::databases`, `cluster::handover`; the `lifecycle` module: `Provisioner` and `TenantHooks` (turns on `serving`) |
//! | `admin` | the `admin` module: `Verifier`, `AdminClaims`, the authenticator, `AdminAudit`, `guard`; [`control::verify_schema`] then also requires the reference schema's `admin_audit` table (its six columns) and its key `admin_audit_pkey` (turns on `serving`, not `lifecycle`) |
//! | `manager` | the `manager` module: `Minter`, `AdminClient`, `CallOutcome`; needs no database driver and works without `serving` |
//! | `runtime` | `Tenants::into_unit`, `invalidation::Listener::into_unit` (turns on `serving`) |
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
//! sent. Database errors map through `sekvent_db::to_app_error`. A request
//! refused by the serving half carries one of the [`reasons`] under the
//! product's serving domain ([`ErrorDomains`]). No error carries a URL,
//! connect options, a role password or a verifier.

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

#[cfg(feature = "serving")]
pub mod control;
#[cfg(feature = "serving")]
pub mod hosts;
#[cfg(feature = "serving")]
mod info;
#[cfg(feature = "serving")]
pub mod invalidation;
#[cfg(feature = "serving")]
mod resolver;
#[cfg(feature = "serving")]
mod select;
#[cfg(feature = "serving")]
mod tenants;

#[cfg(feature = "admin")]
pub mod admin;
#[cfg(feature = "lifecycle")]
pub mod lifecycle;
#[cfg(feature = "manager")]
pub mod manager;

pub use error::{ErrorDomains, reasons};
pub use id::TenantId;
pub use names::{DatabaseName, KEY_ID_MAX, ObjectNames, TenantRole, is_adoptable_name, is_key_id};

#[cfg(feature = "serving")]
pub use connector::Connector;
#[cfg(feature = "serving")]
pub use secrets::{RoleKey, RoleSecrets};

#[cfg(feature = "serving")]
pub use control::{ProvisionStep, Registry, TenantRow};
#[cfg(feature = "serving")]
pub use hosts::HostSnapshot;
#[cfg(feature = "serving")]
pub use info::{Selector, TenantInfo, TenantState};
#[cfg(feature = "serving")]
pub use resolver::{StreamTenant, TenantResolver};
#[cfg(feature = "serving")]
pub use select::SelectorRules;
#[cfg(feature = "serving")]
pub use tenants::{
    ScopeFactory, ScopeFuture, TenantLease, Tenants, TenantsBuilder, TenantsStats, WalkReport,
};

#[cfg(feature = "lifecycle")]
pub use cluster::{Maintenance, MaintenanceConn, MaintenancePool};
#[cfg(feature = "lifecycle")]
pub use secrets::ScramVerifier;
