//! Migrations on boot.

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
use crate::DbError;

/// Apply the sqlx migrations in `dir` to `pool`, skipping those already
/// applied. Run it once at startup, before serving traffic.
///
/// The error names the directory; the migrator's own error is its source.
#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
pub async fn migrate_on_boot<DB>(
    pool: &sqlx::Pool<DB>,
    dir: &std::path::Path,
) -> Result<(), DbError>
where
    DB: sqlx::Database,
    DB::Connection: sqlx::migrate::Migrate,
{
    let failed = |source: sqlx::migrate::MigrateError| DbError::Migrate {
        target: dir.display().to_string(),
        source: Box::new(source),
    };
    let migrator = sqlx::migrate::Migrator::new(dir).await.map_err(failed)?;
    migrator.run(pool).await.map_err(failed)
}

/// The `target` of a [`DbError::Migrate`] from [`run_embedded_migrations`].
#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
const EMBEDDED_TARGET: &str = "embedded migrations";

/// Apply migrations compiled into the binary with `sqlx::migrate!`,
/// skipping those already applied. Run it once at startup, before serving
/// traffic. On Postgres the migrator holds an advisory lock, so instances
/// starting together apply each migration once.
///
/// ```ignore
/// static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
/// run_embedded_migrations(&pool, &MIGRATOR).await?;
/// ```
///
/// The error's target is `embedded migrations`; the migrator's own error is
/// its source.
#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
pub async fn run_embedded_migrations<DB>(
    pool: &sqlx::Pool<DB>,
    migrator: &sqlx::migrate::Migrator,
) -> Result<(), DbError>
where
    DB: sqlx::Database,
    DB::Connection: sqlx::migrate::Migrate,
{
    migrator.run(pool).await.map_err(|source| DbError::Migrate {
        target: EMBEDDED_TARGET.to_owned(),
        source: Box::new(source),
    })
}

/// Apply every pending sea-orm migration of `M`.
#[cfg(feature = "sea-orm-migrate")]
pub async fn run_sea_orm_migrations<M>(
    db: &sea_orm::DatabaseConnection,
) -> Result<(), crate::DbError>
where
    M: sea_orm_migration::MigratorTrait,
{
    M::up(db, None)
        .await
        .map_err(|source| crate::DbError::Migrate {
            target: "sea-orm migrator".to_owned(),
            source: Box::new(source),
        })
}
