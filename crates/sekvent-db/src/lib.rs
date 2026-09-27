//! Database pools, migrations and generic list filtering for sqlx and sea-orm.
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
    feature = "migrate",
    any(feature = "sqlx-postgres", feature = "sqlx-mysql")
))]
pub use migrate::migrate_on_boot;
#[cfg(feature = "sea-orm-migrate")]
pub use migrate::run_sea_orm_migrations;
