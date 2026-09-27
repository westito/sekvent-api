//! Migrations on boot against real Postgres and MySQL servers.
//!
//! The tests that need a database run with a reachable Docker daemon:
//!
//! ```sh
//! SEKVENT_DOCKER_TESTS=1 cargo test -p sekvent-db --all-features -- --ignored
//! ```
//!
//! Without `SEKVENT_DOCKER_TESTS=1` each of them returns immediately. The
//! tests that fail before touching a server (a missing directory) always run.

#![cfg(all(feature = "migrate", feature = "sqlx-postgres", feature = "sqlx-mysql"))]

use std::path::{Path, PathBuf};

use sekvent_config::Secret;
use sekvent_db::{DbError, PoolRegistry, PoolSpec, migrate_on_boot};
use sekvent_testing::{MySqlHarness, PostgresHarness, docker_tests_enabled};

fn skip() -> bool {
    if docker_tests_enabled() {
        return false;
    }
    eprintln!("skipped: set SEKVENT_DOCKER_TESTS=1 to run Docker-backed tests");
    true
}

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/migrations")
        .join(name)
}

/// The directory named in a `DbError::Migrate`, after checking that the
/// migrator's own error is kept as the source.
fn migrate_target(error: &DbError) -> &str {
    match error {
        DbError::Migrate { target, source } => {
            assert!(!source.to_string().is_empty());
            target
        }
        other => panic!("expected a migrate error, got {other:?}"),
    }
}

async fn pg_pool() -> sqlx::PgPool {
    let server = PostgresHarness::shared().await;
    let database = server.create_database().await.unwrap();
    sqlx::PgPool::connect(&database.url).await.unwrap()
}

async fn mysql_pool() -> sqlx::MySqlPool {
    let server = MySqlHarness::shared().await;
    let database = server.create_database().await.unwrap();
    sqlx::MySqlPool::connect(&database.url).await.unwrap()
}

#[tokio::test]
async fn a_missing_directory_is_a_migrate_error_naming_it() {
    let pool = sqlx::PgPool::connect_lazy("postgres://orders.invalid/orders").unwrap();
    let dir = fixtures("absent");

    let error = migrate_on_boot(&pool, &dir).await.unwrap_err();

    assert_eq!(migrate_target(&error), dir.display().to_string());
    assert_eq!(
        error.to_string(),
        format!("migrations for `{}` failed", dir.display())
    );
    assert!(std::error::Error::source(&error).is_some());
}

#[tokio::test]
async fn a_missing_directory_fails_the_same_way_for_mysql() {
    let pool = sqlx::MySqlPool::connect_lazy("mysql://orders.invalid/orders").unwrap();
    let dir = fixtures("absent");

    let error = migrate_on_boot(&pool, &dir).await.unwrap_err();

    assert_eq!(migrate_target(&error), dir.display().to_string());
}

#[tokio::test]
async fn migrate_all_reports_the_pool_name_instead_of_the_directory() {
    let registry = PoolRegistry::build(vec![
        PoolSpec::new("billing_db", Secret::new("mysql://billing.invalid/billing"))
            .lazy()
            .with_migrations(fixtures("absent")),
        PoolSpec::new("orders_db", Secret::new("postgres://orders.invalid/orders"))
            .lazy()
            .with_migrations(fixtures("absent")),
    ])
    .await
    .unwrap();
    assert_eq!(
        registry.migrations_dir("orders_db"),
        Some(fixtures("absent").as_path())
    );

    let error = registry.migrate_all().await.unwrap_err();

    // Name order: `billing_db` runs, and fails, first.
    assert_eq!(migrate_target(&error), "billing_db");
    assert_eq!(error.to_string(), "migrations for `billing_db` failed");
}

#[tokio::test]
async fn migrate_all_skips_pools_without_migrations() {
    let registry = PoolRegistry::build(vec![
        PoolSpec::new("orders_db", Secret::new("postgres://orders.invalid/orders")).lazy(),
    ])
    .await
    .unwrap();

    registry.migrate_all().await.unwrap();
    assert_eq!(registry.migrations_dir("orders_db"), None);
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn postgres_migrations_apply_once() {
    if skip() {
        return;
    }
    let pool = pg_pool().await;
    let dir = fixtures("pg");

    migrate_on_boot(&pool, &dir).await.unwrap();
    sqlx::query("INSERT INTO orders (reference, total_cents) VALUES ('ord-1', 1250)")
        .execute(&pool)
        .await
        .unwrap();

    migrate_on_boot(&pool, &dir).await.unwrap();

    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied, 2);
    let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orders, 1, "a second run leaves the data alone");
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_broken_postgres_migration_is_a_migrate_error() {
    if skip() {
        return;
    }
    let pool = pg_pool().await;
    let dir = fixtures("broken");

    let error = migrate_on_boot(&pool, &dir).await.unwrap_err();

    assert_eq!(migrate_target(&error), dir.display().to_string());
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn mysql_migrations_apply_once() {
    if skip() {
        return;
    }
    let pool = mysql_pool().await;
    let dir = fixtures("mysql");

    migrate_on_boot(&pool, &dir).await.unwrap();
    sqlx::query("INSERT INTO orders (reference, total_cents) VALUES ('ord-1', 1250)")
        .execute(&pool)
        .await
        .unwrap();

    migrate_on_boot(&pool, &dir).await.unwrap();

    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied, 2);
    let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orders, 1, "a second run leaves the data alone");
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_broken_mysql_migration_is_a_migrate_error() {
    if skip() {
        return;
    }
    let pool = mysql_pool().await;
    let dir = fixtures("broken");

    let error = migrate_on_boot(&pool, &dir).await.unwrap_err();

    assert_eq!(migrate_target(&error), dir.display().to_string());
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_postgres_registry_connects_with_session_settings_and_migrates() {
    if skip() {
        return;
    }
    let server = PostgresHarness::shared().await;
    let orders = server.create_database().await.unwrap();
    let reports = server.create_database().await.unwrap();
    let registry = PoolRegistry::build(vec![
        PoolSpec::new("orders_db", Secret::new(orders.url))
            .with_search_path("public")
            .with_migrations(fixtures("pg")),
        PoolSpec::new("reports_db", Secret::new(reports.url)).read_only(),
    ])
    .await
    .unwrap();

    registry.migrate_all().await.unwrap();

    let orders_pool = registry.get("orders_db").unwrap().postgres().unwrap();
    assert!(registry.get("orders_db").unwrap().mysql().is_none());
    let search_path: String = sqlx::query_scalar("SHOW search_path")
        .fetch_one(orders_pool)
        .await
        .unwrap();
    assert_eq!(search_path, "public");
    let tables: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders")
        .fetch_one(orders_pool)
        .await
        .unwrap();
    assert_eq!(tables, 0);

    let reports_pool = registry.get("reports_db").unwrap().postgres().unwrap();
    let read_only: String = sqlx::query_scalar("SHOW default_transaction_read_only")
        .fetch_one(reports_pool)
        .await
        .unwrap();
    assert_eq!(read_only, "on");
    registry.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_mysql_registry_connects_with_session_settings_and_migrates() {
    if skip() {
        return;
    }
    let server = MySqlHarness::shared().await;
    let orders = server.create_database().await.unwrap();
    let reports = server.create_database().await.unwrap();
    let registry = PoolRegistry::build(vec![
        PoolSpec::new("orders_db", Secret::new(orders.url)).with_migrations(fixtures("mysql")),
        PoolSpec::new("reports_db", Secret::new(reports.url)).read_only(),
    ])
    .await
    .unwrap();

    registry.migrate_all().await.unwrap();

    let orders_pool = registry.get("orders_db").unwrap().mysql().unwrap();
    assert!(registry.get("orders_db").unwrap().postgres().is_none());
    let tables: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders")
        .fetch_one(orders_pool)
        .await
        .unwrap();
    assert_eq!(tables, 0);

    let reports_pool = registry.get("reports_db").unwrap().mysql().unwrap();
    let read_only: i64 = sqlx::query_scalar("SELECT CAST(@@transaction_read_only AS SIGNED)")
        .fetch_one(reports_pool)
        .await
        .unwrap();
    assert_eq!(read_only, 1);
    registry.close().await;
}

#[cfg(all(feature = "sea-orm-migrate", feature = "sea-orm-postgres"))]
mod sea_orm_migrations {
    use sea_orm::DbErr;
    use sea_orm::sea_query::Table;
    use sea_orm_migration::async_trait::async_trait;
    use sea_orm_migration::schema::{pk_auto, string};
    use sea_orm_migration::{MigrationName, MigrationTrait, MigratorTrait, SchemaManager};
    use sekvent_config::Secret;
    use sekvent_db::{DbError, PoolRegistry, PoolSpec, run_sea_orm_migrations};
    use sekvent_testing::PostgresHarness;

    use super::{migrate_target, skip};

    struct CreateOrders;

    impl MigrationName for CreateOrders {
        fn name(&self) -> &'static str {
            "m0001_create_orders"
        }
    }

    #[async_trait]
    impl MigrationTrait for CreateOrders {
        async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager
                .create_table(
                    Table::create()
                        .table("orders")
                        .if_not_exists()
                        .col(pk_auto("id"))
                        .col(string("reference"))
                        .to_owned(),
                )
                .await
        }
    }

    struct RefuseOrders;

    impl MigrationName for RefuseOrders {
        fn name(&self) -> &'static str {
            "m0002_refuse_orders"
        }
    }

    #[async_trait]
    impl MigrationTrait for RefuseOrders {
        async fn up(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
            Err(DbErr::Migration("orders migration refused".to_owned()))
        }
    }

    struct OrdersMigrator;

    impl MigratorTrait for OrdersMigrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            vec![Box::new(CreateOrders)]
        }
    }

    struct BrokenMigrator;

    impl MigratorTrait for BrokenMigrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            vec![Box::new(CreateOrders), Box::new(RefuseOrders)]
        }
    }

    async fn registry() -> PoolRegistry {
        let server = PostgresHarness::shared().await;
        let database = server.create_database().await.unwrap();
        PoolRegistry::build(vec![PoolSpec::new("orders_db", Secret::new(database.url))])
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn sea_orm_migrations_apply_once() {
        if skip() {
            return;
        }
        let registry = registry().await;
        let pool = registry.get("orders_db").unwrap();
        let db = pool.sea_orm().unwrap();

        run_sea_orm_migrations::<OrdersMigrator>(&db).await.unwrap();
        run_sea_orm_migrations::<OrdersMigrator>(&db).await.unwrap();

        let sqlx_pool = pool.postgres().unwrap();
        let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM seaql_migrations")
            .fetch_one(sqlx_pool)
            .await
            .unwrap();
        assert_eq!(applied, 1);
        let table: Option<String> = sqlx::query_scalar("SELECT to_regclass('orders')::text")
            .fetch_one(sqlx_pool)
            .await
            .unwrap();
        assert_eq!(table.as_deref(), Some("orders"));
        registry.close().await;
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn a_failing_sea_orm_migration_is_a_migrate_error() {
        if skip() {
            return;
        }
        let registry = registry().await;
        let db = registry.get("orders_db").unwrap().sea_orm().unwrap();

        let error: DbError = run_sea_orm_migrations::<BrokenMigrator>(&db)
            .await
            .unwrap_err();

        assert_eq!(migrate_target(&error), "sea-orm migrator");
        registry.close().await;
    }
}
