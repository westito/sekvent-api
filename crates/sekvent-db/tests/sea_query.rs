//! sea-query statements run through sqlx, constraint names, and migrations
//! embedded with `sqlx::migrate!`, against real Postgres and MySQL servers.
//!
//! The tests that need a database run with a reachable Docker daemon:
//!
//! ```sh
//! SEKVENT_DOCKER_TESTS=1 cargo test -p sekvent-db --all-features -- --ignored
//! ```

#![cfg(all(
    feature = "sea-query-postgres",
    feature = "sea-query-mysql",
    feature = "migrate"
))]

use chrono::{DateTime, TimeZone, Utc};
use sekvent_config::Secret;
use sekvent_db::sea_query::{
    Expr, ExprTrait, Iden, MysqlQueryBuilder, OnConflict, Order, PgFunc, PostgresQueryBuilder,
    Query,
};
use sekvent_db::sea_query_sqlx::SqlxBinder;
use sekvent_db::{
    DbError, IntoAppError, PoolRegistry, PoolSpec, constraint_name, query, run_embedded_migrations,
    sqlstate,
};
use sekvent_error::ErrorCode;
use sekvent_testing::{MySqlHarness, PostgresHarness, docker_tests_enabled};
use serde_json::json;
use sqlx::migrate::Migrator;
use uuid::Uuid;

static MIGRATOR: Migrator = sqlx::migrate!("tests/fixtures/migrations/embedded_pg");
static BROKEN: Migrator = sqlx::migrate!("tests/fixtures/migrations/broken");

#[derive(Iden)]
enum Customers {
    Table,
    Id,
    Name,
}

#[derive(Iden)]
enum Orders {
    Table,
    Id,
    CustomerId,
    Reference,
    TotalCents,
    Attributes,
    Tags,
    PlacedAt,
    Note,
}

#[derive(Debug, sqlx::FromRow)]
struct OrderRow {
    id: i64,
    customer_id: Uuid,
    reference: String,
    total_cents: i64,
    attributes: serde_json::Value,
    tags: Vec<String>,
    placed_at: DateTime<Utc>,
    note: Option<String>,
}

fn skip() -> bool {
    if docker_tests_enabled() {
        return false;
    }
    eprintln!("skipped: set SEKVENT_DOCKER_TESTS=1 to run Docker-backed tests");
    true
}

async fn migrated_pool() -> sqlx::PgPool {
    let server = PostgresHarness::shared().await;
    let database = server.create_database().await.unwrap();
    let pool = sqlx::PgPool::connect(&database.url).await.unwrap();
    run_embedded_migrations(&pool, &MIGRATOR).await.unwrap();
    pool
}

fn placed_at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 14, 9, 26, 53).unwrap()
}

fn insert_customer(id: Uuid, name: &str) -> sekvent_db::sea_query::InsertStatement {
    Query::insert()
        .into_table(Customers::Table)
        .columns([Customers::Id, Customers::Name])
        .values_panic([id.into(), name.into()])
        .to_owned()
}

fn insert_order(
    customer: Uuid,
    reference: &str,
    total_cents: i64,
) -> sekvent_db::sea_query::InsertStatement {
    Query::insert()
        .into_table(Orders::Table)
        .columns([
            Orders::CustomerId,
            Orders::Reference,
            Orders::TotalCents,
            Orders::Attributes,
            Orders::Tags,
            Orders::PlacedAt,
        ])
        .values_panic([
            customer.into(),
            reference.into(),
            total_cents.into(),
            json!({ "channel": "web", "gift": true }).into(),
            vec!["priority".to_owned(), "fragile".to_owned()].into(),
            placed_at().into(),
        ])
        .returning_col(Orders::Id)
        .to_owned()
}

fn select_orders() -> sekvent_db::sea_query::SelectStatement {
    Query::select()
        .columns([
            Orders::Id,
            Orders::CustomerId,
            Orders::Reference,
            Orders::TotalCents,
            Orders::Attributes,
            Orders::Tags,
            Orders::PlacedAt,
            Orders::Note,
        ])
        .from(Orders::Table)
        .to_owned()
}

#[test]
fn identifiers_render_quoted_and_values_bound() {
    let (sql, values) = select_orders()
        .and_where(Expr::col(Orders::Reference).eq("ord-1"))
        .to_owned()
        .build_sqlx(PostgresQueryBuilder);
    assert!(
        sql.ends_with(r#"FROM "orders" WHERE "reference" = $1"#),
        "{sql}"
    );
    assert!(!sql.contains("ord-1"));
    assert_eq!(values.0.0.len(), 1);

    let (sql, _) = Query::select()
        .column(Orders::Id)
        .from(Orders::Table)
        .to_owned()
        .build_sqlx(MysqlQueryBuilder);
    assert_eq!(sql, "SELECT `id` FROM `orders`");
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn statements_round_trip_jsonb_uuid_timestamps_and_arrays() {
    if skip() {
        return;
    }
    let pool = migrated_pool().await;
    let customer = Uuid::now_v7();

    let inserted = query::execute(&pool, &insert_customer(customer, "Ada"))
        .await
        .unwrap();
    assert_eq!(inserted.rows_affected(), 1);
    let (first,): (i64,) = query::fetch_one(&pool, &insert_order(customer, "ord-1", 1250))
        .await
        .unwrap();
    let (second,): (i64,) = query::fetch_one(&pool, &insert_order(customer, "ord-2", 0))
        .await
        .unwrap();

    let order: OrderRow = query::fetch_one(
        &pool,
        select_orders()
            .and_where(Expr::col(Orders::CustomerId).eq(customer))
            .and_where(Expr::cust_with_values(
                "attributes @> $1",
                [json!({ "gift": true })],
            ))
            .and_where(Expr::col(Orders::Id).eq(first)),
    )
    .await
    .unwrap();
    assert_eq!(order.id, first);
    assert_eq!(order.customer_id, customer);
    assert_eq!(order.reference, "ord-1");
    assert_eq!(order.total_cents, 1250);
    assert_eq!(order.attributes["channel"], "web");
    assert_eq!(order.tags, ["priority", "fragile"]);
    assert_eq!(order.placed_at, placed_at());
    assert_eq!(order.note, None);

    let by_ids: Vec<OrderRow> = query::fetch_all(
        &pool,
        select_orders()
            .and_where(Expr::col(Orders::Id).eq(PgFunc::any(vec![first, second])))
            .order_by(Orders::Id, Order::Asc),
    )
    .await
    .unwrap();
    let references: Vec<_> = by_ids.iter().map(|o| o.reference.as_str()).collect();
    assert_eq!(references, ["ord-1", "ord-2"]);

    let missing: Option<OrderRow> = query::fetch_optional(
        &pool,
        select_orders().and_where(Expr::col(Orders::Reference).eq("ord-404")),
    )
    .await
    .unwrap();
    assert!(missing.is_none());

    let error = query::fetch_one::<OrderRow, _, _>(
        &pool,
        select_orders().and_where(Expr::col(Orders::Reference).eq("ord-404")),
    )
    .await
    .into_app_error()
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn update_returning_runs_inside_an_explicit_transaction() {
    if skip() {
        return;
    }
    let pool = migrated_pool().await;
    let customer = Uuid::now_v7();
    query::execute(&pool, &insert_customer(customer, "Ada"))
        .await
        .unwrap();
    let (id,): (i64,) = query::fetch_one(&pool, &insert_order(customer, "ord-1", 1250))
        .await
        .unwrap();
    let bump = |note: &str| {
        Query::update()
            .table(Orders::Table)
            .value(Orders::Note, note)
            .value(Orders::TotalCents, Expr::col(Orders::TotalCents).add(100))
            .and_where(Expr::col(Orders::Id).eq(id))
            .returning_col(Orders::TotalCents)
            .to_owned()
    };

    let mut tx = pool.begin().await.unwrap();
    let (total,): (i64,) = query::fetch_one(&mut *tx, &bump("rolled back"))
        .await
        .unwrap();
    assert_eq!(total, 1350);
    tx.rollback().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let (total,): (i64,) = query::fetch_one(&mut *tx, &bump("kept")).await.unwrap();
    assert_eq!(total, 1350);
    let seen: OrderRow = query::fetch_one(
        &mut *tx,
        select_orders().and_where(Expr::col(Orders::Id).eq(id)),
    )
    .await
    .unwrap();
    assert_eq!(seen.note.as_deref(), Some("kept"));
    tx.commit().await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let after: OrderRow = query::fetch_one(
        &mut *conn,
        select_orders().and_where(Expr::col(Orders::Id).eq(id)),
    )
    .await
    .unwrap();
    assert_eq!(
        (after.total_cents, after.note.as_deref()),
        (1350, Some("kept"))
    );
    drop(conn);
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn violations_name_their_constraint() {
    if skip() {
        return;
    }
    let pool = migrated_pool().await;
    let customer = Uuid::now_v7();
    query::execute(&pool, &insert_customer(customer, "Ada"))
        .await
        .unwrap();
    query::execute(&pool, &insert_order(customer, "ord-1", 1250))
        .await
        .unwrap();

    let unique = query::execute(&pool, &insert_order(customer, "ord-1", 1))
        .await
        .unwrap_err();
    assert_eq!(constraint_name(&unique), Some("orders_reference_key"));
    assert_eq!(sqlstate(&unique).as_deref(), Some("23505"));
    let mapped = Err::<(), _>(unique).into_app_error().unwrap_err();
    assert_eq!(mapped.code(), ErrorCode::AlreadyExists);

    let foreign = query::execute(&pool, &insert_order(Uuid::now_v7(), "ord-2", 1))
        .await
        .unwrap_err();
    assert_eq!(constraint_name(&foreign), Some("orders_customer_id_fkey"));
    assert_eq!(sqlstate(&foreign).as_deref(), Some("23503"));
    let mapped = Err::<(), _>(foreign).into_app_error().unwrap_err();
    assert_eq!(mapped.code(), ErrorCode::FailedPrecondition);

    let check = query::execute(&pool, &insert_order(customer, "ord-3", -1))
        .await
        .unwrap_err();
    assert_eq!(constraint_name(&check), Some("orders_total_non_negative"));
    assert_eq!(sqlstate(&check).as_deref(), Some("23514"));

    let upsert = Query::insert()
        .into_table(Customers::Table)
        .columns([Customers::Id, Customers::Name])
        .values_panic([customer.into(), "Ada Lovelace".into()])
        .on_conflict(
            OnConflict::column(Customers::Id)
                .update_column(Customers::Name)
                .to_owned(),
        )
        .to_owned();
    query::execute(&pool, &upsert).await.unwrap();
    let (name,): (String,) = query::fetch_one(
        &pool,
        Query::select()
            .column(Customers::Name)
            .from(Customers::Table)
            .and_where(Expr::col(Customers::Id).eq(customer)),
    )
    .await
    .unwrap();
    assert_eq!(name, "Ada Lovelace");
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn statements_run_on_mysql() {
    if skip() {
        return;
    }
    let server = MySqlHarness::shared().await;
    let database = server.create_database().await.unwrap();
    let pool = sqlx::MySqlPool::connect(&database.url).await.unwrap();
    sqlx::query("CREATE TABLE orders (id BIGINT PRIMARY KEY, reference VARCHAR(32) NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let insert = Query::insert()
        .into_table(Orders::Table)
        .columns([Orders::Id, Orders::Reference])
        .values_panic([7.into(), "ord-7".into()])
        .to_owned();
    let done = query::execute(&pool, &insert).await.unwrap();
    assert_eq!(done.rows_affected(), 1);
    let (reference,): (String,) = query::fetch_one(
        &pool,
        Query::select()
            .column(Orders::Reference)
            .from(Orders::Table)
            .and_where(Expr::col(Orders::Id).eq(7)),
    )
    .await
    .unwrap();
    assert_eq!(reference, "ord-7");
    let duplicate = query::execute(&pool, &insert).await.unwrap_err();
    assert_eq!(sqlstate(&duplicate).as_deref(), Some("23000"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn embedded_migrations_apply_once() {
    if skip() {
        return;
    }
    let pool = migrated_pool().await;
    query::execute(&pool, &insert_customer(Uuid::now_v7(), "Ada"))
        .await
        .unwrap();

    run_embedded_migrations(&pool, &MIGRATOR).await.unwrap();

    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied, 2);
    let customers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM customers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(customers, 1, "a second run leaves the data alone");
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_broken_embedded_migration_is_a_migrate_error() {
    if skip() {
        return;
    }
    let server = PostgresHarness::shared().await;
    let database = server.create_database().await.unwrap();
    let pool = sqlx::PgPool::connect(&database.url).await.unwrap();

    let error = run_embedded_migrations(&pool, &BROKEN).await.unwrap_err();

    match &error {
        DbError::Migrate { target, source } => {
            assert_eq!(target, "embedded migrations");
            assert!(!source.to_string().is_empty());
        }
        other => panic!("expected a migrate error, got {other:?}"),
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn migrate_all_runs_embedded_migrations_per_pool() {
    if skip() {
        return;
    }
    let pg = PostgresHarness::shared().await;
    let orders = pg.create_database().await.unwrap();
    let reports = pg.create_database().await.unwrap();
    let mysql = MySqlHarness::shared().await;
    let billing = mysql.create_database().await.unwrap();

    let registry = PoolRegistry::build(vec![
        PoolSpec::new("orders_db", Secret::new(orders.url.clone()))
            .with_embedded_migrations(&MIGRATOR),
        PoolSpec::new("billing_db", Secret::new(billing.url.clone()))
            .with_embedded_migrations(&BROKEN),
    ])
    .await
    .unwrap();
    assert_eq!(registry.migrations_dir("orders_db"), None);

    // Name order: `billing_db` runs, and fails, first.
    let error = registry.migrate_all().await.unwrap_err();
    assert!(
        matches!(&error, DbError::Migrate { target, .. } if target == "billing_db"),
        "{error:?}"
    );
    registry.close().await;

    let registry = PoolRegistry::build(vec![
        PoolSpec::new("orders_db", Secret::new(orders.url.clone()))
            .with_embedded_migrations(&MIGRATOR),
        PoolSpec::new("reports_db", Secret::new(reports.url.clone())),
    ])
    .await
    .unwrap();
    registry.migrate_all().await.unwrap();
    registry.migrate_all().await.unwrap();
    let pool = registry.get("orders_db").unwrap().postgres().unwrap();
    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(applied, 2);
    registry.close().await;
}

#[tokio::test]
async fn a_directory_and_embedded_migrations_together_are_rejected() {
    let error = PoolRegistry::build(vec![
        PoolSpec::new("orders_db", Secret::new("postgres://orders.invalid/orders"))
            .lazy()
            .with_migrations("migrations")
            .with_embedded_migrations(&MIGRATOR),
    ])
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::InvalidSetting { .. }), "{error:?}");
}
