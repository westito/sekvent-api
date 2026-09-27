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
