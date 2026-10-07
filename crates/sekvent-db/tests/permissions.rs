//! Permission errors and probes against real Postgres and MySQL servers.
//!
//! ```sh
//! SEKVENT_DOCKER_TESTS=1 cargo test -p sekvent-db --all-features -- --ignored
//! ```
//!
//! Without `SEKVENT_DOCKER_TESTS=1` each test returns immediately. Users and
//! roles are server-wide, so each test derives their names from its own
//! freshly created database.

#![cfg(all(feature = "sqlx-postgres", feature = "sqlx-mysql"))]

use sekvent_db::{ConnectFailure, classify, classify_connect, reasons, to_app_error};
use sekvent_error::ErrorCode;
use sekvent_testing::{MySqlHarness, PostgresHarness, TestDatabase, docker_tests_enabled};
use sqlx::{AssertSqlSafe, Connection, MySqlConnection, PgConnection};

const PASSWORD: &str = "Granted7pass";

fn skip() -> bool {
    if docker_tests_enabled() {
        return false;
    }
    eprintln!("skipped: set SEKVENT_DOCKER_TESTS=1 to run Docker-backed tests");
    true
}

/// A user name unique to `database`, short enough for MySQL (32 chars).
fn user_for(database: &TestDatabase) -> String {
    let suffix: String = database.name.chars().rev().take(20).collect();
    format!("u{suffix}")
}

async fn admin_mysql(server: &MySqlHarness, database: &TestDatabase) -> MySqlConnection {
    MySqlConnection::connect(&server.url_for(&database.name))
        .await
        .unwrap()
}

async fn admin_pg(server: &PostgresHarness, database: &TestDatabase) -> PgConnection {
    PgConnection::connect(&server.url_for(&database.name))
        .await
        .unwrap()
}

async fn run_mysql(conn: &mut MySqlConnection, statements: &[String]) {
    for statement in statements {
        sqlx::raw_sql(AssertSqlSafe(statement.as_str()))
            .execute(&mut *conn)
            .await
            .unwrap();
    }
}

async fn run_pg(conn: &mut PgConnection, statements: &[String]) {
    for statement in statements {
        sqlx::raw_sql(AssertSqlSafe(statement.as_str()))
            .execute(&mut *conn)
            .await
            .unwrap();
    }
}

fn mysql_url(server: &MySqlHarness, user: &str, password: &str, database: &str) -> String {
    format!(
        "mysql://{user}:{password}@{}:{}/{database}",
        server.host(),
        server.port()
    )
}

fn pg_url(server: &PostgresHarness, user: &str, password: &str, database: &str) -> String {
    format!(
        "postgres://{user}:{password}@{}:{}/{database}",
        server.host(),
        server.port()
    )
}

/// A MySQL user allowed to `SELECT` from `allowed` only.
async fn mysql_reader() -> (&'static MySqlHarness, TestDatabase, String) {
    let server = MySqlHarness::shared().await;
    let database = server.create_database().await.unwrap();
    let user = user_for(&database);
    let mut admin = admin_mysql(server, &database).await;
    run_mysql(
        &mut admin,
        &[
            "CREATE TABLE allowed (id INT PRIMARY KEY)".to_owned(),
            "CREATE TABLE secret (id INT PRIMARY KEY)".to_owned(),
            format!("CREATE USER '{user}'@'%' IDENTIFIED BY '{PASSWORD}'"),
            format!(
                "GRANT SELECT ON `{}`.allowed TO '{user}'@'%'",
                database.name
            ),
        ],
    )
    .await;
    admin.close().await.unwrap();
    (server, database, user)
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_mysql_table_without_grant_is_permission_denied() {
    if skip() {
        return;
    }
    let (server, database, user) = mysql_reader().await;
    let mut conn = MySqlConnection::connect(&mysql_url(server, &user, PASSWORD, &database.name))
        .await
        .unwrap();

    sqlx::query("SELECT id FROM allowed")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    let error = sqlx::query("SELECT id FROM secret")
        .fetch_all(&mut conn)
        .await
        .unwrap_err();

    assert_eq!(mysql_errno(&error), Some(1142));
    assert_eq!(classify(&error), ErrorCode::PermissionDenied);
    let app = to_app_error(error);
    assert_eq!(app.message(), "permission denied");
    assert_eq!(app.reason(), Some(reasons::DB_PERMISSION_DENIED));
    assert!(!app.to_wire().message.contains("secret"));

    // A syntax error shares SQLSTATE 42000 and stays internal.
    let syntax = sqlx::query("SELEC 1").execute(&mut conn).await.unwrap_err();
    assert_eq!(classify(&syntax), ErrorCode::Internal);
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn mysql_connect_refusals_are_auth_failures() {
    if skip() {
        return;
    }
    let (server, database, user) = mysql_reader().await;

    // Rejected credentials: errno 1045, the service's own misconfiguration.
    let wrong_password =
        MySqlConnection::connect(&mysql_url(server, &user, "Wrong7pass", &database.name))
            .await
            .unwrap_err();
    assert_eq!(mysql_errno(&wrong_password), Some(1045));
    assert_eq!(classify_connect(&wrong_password), ConnectFailure::Auth);
    assert_eq!(classify(&wrong_password), ErrorCode::Internal);
    let app = to_app_error(wrong_password);
    assert_eq!(app.message(), "internal error");
    assert_eq!(app.reason(), Some(reasons::DB_CREDENTIALS_REJECTED));

    // A database the user has no grant on: errno 1044, SQLSTATE 42000.
    let other = server.create_database().await.unwrap();
    let no_grant = MySqlConnection::connect(&mysql_url(server, &user, PASSWORD, &other.name))
        .await
        .unwrap_err();
    assert_eq!(mysql_errno(&no_grant), Some(1044));
    assert_eq!(classify_connect(&no_grant), ConnectFailure::Auth);
    assert_eq!(classify(&no_grant), ErrorCode::PermissionDenied);
    assert_eq!(
        to_app_error(no_grant).reason(),
        Some(reasons::DB_PERMISSION_DENIED)
    );
}

fn mysql_errno(error: &sqlx::Error) -> Option<u16> {
    error
        .as_database_error()
        .and_then(|e| e.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>())
        .map(sqlx::mysql::MySqlDatabaseError::number)
}

/// A Postgres role that may log in but holds no privilege on `secret`.
async fn pg_role() -> (&'static PostgresHarness, TestDatabase, String) {
    let server = PostgresHarness::shared().await;
    let database = server.create_database().await.unwrap();
    let role = user_for(&database);
    let mut admin = admin_pg(server, &database).await;
    run_pg(
        &mut admin,
        &[
            "CREATE TABLE secret (id INT PRIMARY KEY)".to_owned(),
            format!("CREATE ROLE {role} LOGIN PASSWORD '{PASSWORD}'"),
        ],
    )
    .await;
    admin.close().await.unwrap();
    (server, database, role)
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_postgres_role_without_privileges_is_permission_denied() {
    if skip() {
        return;
    }
    let (server, database, role) = pg_role().await;
    let mut conn = PgConnection::connect(&pg_url(server, &role, PASSWORD, &database.name))
        .await
        .unwrap();

    let error = sqlx::query("SELECT id FROM secret")
        .fetch_all(&mut conn)
        .await
        .unwrap_err();

    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501")
    );
    assert_eq!(classify(&error), ErrorCode::PermissionDenied);
    assert_eq!(to_app_error(error).message(), "permission denied");
    conn.close().await.unwrap();

    let wrong_password =
        PgConnection::connect(&pg_url(server, &role, "Wrong7pass", &database.name))
            .await
            .unwrap_err();
    assert_eq!(classify_connect(&wrong_password), ConnectFailure::Auth);
    assert_eq!(classify(&wrong_password), ErrorCode::Internal);
    let app = to_app_error(wrong_password);
    assert_eq!(app.message(), "internal error");
    assert_eq!(app.reason(), Some(reasons::DB_CREDENTIALS_REJECTED));
}

#[cfg(feature = "runtime")]
mod probes {
    use std::time::Duration;

    use sekvent_config::Secret;
    use sekvent_db::{PoolProbe, PoolRegistry, PoolSpec};
    use sekvent_runtime::{DependencyProbe, ProbeFailure, ProbeStatus};

    use super::*;

    const GUARD: Duration = Duration::from_secs(30);

    async fn probe(probe: &PoolProbe) -> ProbeStatus {
        tokio::time::timeout(GUARD, probe.probe()).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn probes_are_up_against_both_servers_and_rejected_with_a_wrong_password() {
        if skip() {
            return;
        }
        let pg = PostgresHarness::shared().await;
        let pg_db = pg.create_database().await.unwrap();
        let mysql = MySqlHarness::shared().await;
        let mysql_db = mysql.create_database().await.unwrap();
        let (_, reader_db, reader) = mysql_reader().await;
        let (_, pg_role_db, role) = pg_role().await;
        let ungranted_db = mysql.create_database().await.unwrap();

        let registry = PoolRegistry::build(vec![
            PoolSpec::new(
                "ungranted_mysql",
                Secret::new(mysql_url(mysql, &reader, PASSWORD, &ungranted_db.name)),
            )
            .lazy(),
            PoolSpec::new("orders_db", Secret::new(pg_db.url.clone())),
            PoolSpec::new("legacy_db", Secret::new(mysql_db.url.clone())),
            PoolSpec::new(
                "wrong_pg",
                Secret::new(pg_url(pg, &role, "Wrong7pass", &pg_role_db.name)),
            )
            .lazy(),
            PoolSpec::new(
                "wrong_mysql",
                Secret::new(mysql_url(mysql, &reader, "Wrong7pass", &reader_db.name)),
            )
            .lazy(),
        ])
        .await
        .unwrap();

        let probes = registry.probes();
        for candidate in &probes {
            let status = probe(candidate).await;
            let expected = match candidate.name() {
                "orders_db" | "legacy_db" => ProbeStatus::Up,
                // Errno 1044: a missing grant, not rejected credentials.
                "ungranted_mysql" => ProbeStatus::Down(ProbeFailure::Rejected("access denied")),
                _ => ProbeStatus::Down(ProbeFailure::Rejected("credentials rejected")),
            };
            assert_eq!(status, expected, "{}", candidate.name());
        }
        registry.close().await;
        for candidate in &probes {
            assert_eq!(
                probe(candidate).await,
                ProbeStatus::Down(ProbeFailure::Unreachable("pool closed")),
                "{}",
                candidate.name()
            );
        }
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn a_saturated_pool_keeps_the_previous_result() {
        if skip() {
            return;
        }
        let server = PostgresHarness::shared().await;
        let database = server.create_database().await.unwrap();
        let registry = PoolRegistry::build(vec![
            PoolSpec::new("orders_db", Secret::new(database.url))
                .with_connections(0, 1)
                .with_acquire_timeout(Duration::from_millis(300)),
        ])
        .await
        .unwrap();
        let pool = registry
            .get("orders_db")
            .unwrap()
            .postgres()
            .unwrap()
            .clone();
        let probes = registry.probes();
        let warm = &probes[0];
        assert_eq!(probe(warm).await, ProbeStatus::Up);

        let held = pool.acquire().await.unwrap();
        // Every connection is in use: the last result stands, at once.
        assert_eq!(probe(warm).await, ProbeStatus::Up);
        // A probe without a previous result queues like any caller.
        let cold = PoolProbe::new("orders_db", registry.get("orders_db").unwrap().clone());
        assert_eq!(
            probe(&cold).await,
            ProbeStatus::Down(ProbeFailure::Unreachable("no connection available in time"))
        );
        drop(held);
        // The connection goes back to the pool in the background.
        sekvent_testing::await_until!(pool.num_idle() == 1, timeout = GUARD);
        assert_eq!(probe(&cold).await, ProbeStatus::Up);
        registry.close().await;
    }
}
