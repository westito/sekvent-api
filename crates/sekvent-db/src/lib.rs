//! Database pools, migrations, probes, leases and generic list filtering for
//! sqlx and sea-orm.
//!
//! Every integration is behind a feature; with none enabled the crate offers
//! only the backend-neutral pieces ([`PoolSpec`], [`assert_distinct_targets`]).
//!
//! | feature | adds |
//! |---|---|
//! | `sqlx-postgres`, `sqlx-mysql` | [`PoolRegistry`], `classify`, `to_app_error` |
//! | `sea-orm` | `classify_db_err`, the [`list`] helpers |
//! | `sea-orm-postgres`, `sea-orm-mysql` | `Pool::sea_orm` over the registry's pools |
//! | `migrate` | `migrate_on_boot`, [`PoolRegistry::migrate_all`] |
//! | `sea-orm-migrate` | `run_sea_orm_migrations` |
//! | `serde` | serde derives on the list parameters |
//! | `runtime` (with a sqlx backend) | `PoolProbe`, `PoolRegistry::probes` for `sekvent-runtime` readiness |
//! | `lease` (with a sqlx backend) | `LeaseStore`, `Lease`, `HeldLease`: leases with fencing tokens |
//! | `lease` + `runtime` | `LeaseGuard`: singleton jobs for `sekvent-runtime` |
//!
//! # Probes
//!
//! `registry.probes()` returns one readiness probe per connected pool,
//! required exactly when the pool's spec was:
//!
//! ```ignore
//! let builder = registry.probes().into_iter().fold(Runtime::builder(), |b, p| b.probe(p));
//! ```
//!
//! # Leases
//!
//! A lease is one row per name in a table the application owns
//! (`LeaseStore::schema_sql` for its migrations, or `ensure_schema`). Expiry
//! is computed by the database's clock; every acquisition increments the
//! row's fencing token, so a write that must not come from a stale holder
//! checks it (`check_fence_postgres` / `check_fence_mysql`) in its own
//! transaction. `Lease::keep_alive` renews in the background and reports a
//! lost lease through a cancellation token. `LeaseGuard` turns a
//! `sekvent-runtime` job into a singleton: each tick runs on one instance,
//! once.
//!
//! # Secrets
//!
//! Database URLs carry passwords. They live in [`sekvent_config::Secret`] and
//! this crate never logs or formats them, nor the text of a connect error
//! (drivers embed URL fragments in some messages). A failed connect is
//! reported as the pool name plus a [`ConnectFailure`] class.
//!
//! # Transactions
//!
//! Transactions are not hidden behind helpers: call `pool.begin()` and pass
//! the transaction to the code that needs it. The outbox pattern in
//! particular requires the caller's transaction, so the business write and
//! the outbox row commit or roll back together.
//!
//! ```ignore
//! let mut tx = pool.begin().await.map_err(sekvent_db::to_app_error)?;
//! orders::insert(&mut *tx, &order).await?;
//! outbox::enqueue(&mut *tx, &event).await?;
//! tx.commit().await.map_err(sekvent_db::to_app_error)?;
//! ```

#![forbid(unsafe_code)]

mod error;
mod spec;
mod target;

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
mod registry;

#[cfg(feature = "migrate")]
mod migrate;

#[cfg(feature = "sea-orm")]
pub mod list;

#[cfg(all(
    feature = "runtime",
    any(feature = "sqlx-postgres", feature = "sqlx-mysql")
))]
mod probe;

#[cfg(all(
    feature = "lease",
    any(feature = "sqlx-postgres", feature = "sqlx-mysql")
))]
mod lease;

/// Stable `reason` values of the errors this crate returns.
#[cfg(any(feature = "lease", feature = "sqlx-postgres", feature = "sqlx-mysql"))]
pub mod reasons;

pub use error::{ConnectFailure, DbError, public_message};
pub use spec::PoolSpec;
pub use target::{Target, assert_distinct_targets, parse_target};

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql", feature = "sea-orm"))]
pub use error::IntoAppError;
#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
pub use error::{classify, classify_connect, to_app_error};
#[cfg(feature = "sea-orm")]
pub use error::{classify_db_err, db_err_to_app_error};

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
pub use registry::{Pool, PoolRegistry};

#[cfg(all(
    feature = "runtime",
    any(feature = "sqlx-postgres", feature = "sqlx-mysql")
))]
pub use probe::PoolProbe;

#[cfg(all(
    feature = "lease",
    any(feature = "sqlx-postgres", feature = "sqlx-mysql")
))]
pub use lease::{
    DEFAULT_LEASE_TABLE, DEFAULT_LEASE_TTL, FencingToken, HeldLease, Lease, LeaseInfo, LeaseStore,
};

#[cfg(all(
    feature = "lease",
    feature = "runtime",
    any(feature = "sqlx-postgres", feature = "sqlx-mysql")
))]
pub use lease::LeaseGuard;

#[cfg(all(
    feature = "migrate",
    any(feature = "sqlx-postgres", feature = "sqlx-mysql")
))]
pub use migrate::migrate_on_boot;
#[cfg(feature = "sea-orm-migrate")]
pub use migrate::run_sea_orm_migrations;
