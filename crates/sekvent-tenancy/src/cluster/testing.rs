//! The in-crate Docker tests' database: one of the shared test server whose
//! own `search_path` puts `public` before `pg_catalog`, so that a session
//! without the crate's startup options resolves unqualified names, and
//! operators not named with their schema, to `public` first: an operator of
//! `public` wins there even where `pg_catalog` has one of exactly the same
//! types.
//!
//! The tests skip when Docker tests are off; the container is reaped with
//! the process.

use std::str::FromStr;
use std::time::Duration;

use sekvent_testing::{PostgresHarness, docker_tests_enabled};
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, PgConnection};

/// The bound of one scenario.
pub(crate) const SCENARIO_TIMEOUT: Duration = Duration::from_secs(30);

/// A function body that raises: a statement under test must never call it.
pub(crate) const RAISES: &str =
    "LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'a hostile object was called'; END $$";

/// A fresh database with `objects(name)` (SQL, no trailing semicolon) run
/// on it by the bootstrap superuser, then its own `search_path` set to
/// `public, pg_catalog`; the superuser's options on it and its name.
/// `None` when Docker tests are off.
pub(crate) async fn public_first(
    objects: impl FnOnce(&str) -> String,
) -> Option<(PgConnectOptions, String)> {
    if !docker_tests_enabled() {
        return None;
    }
    let harness = PostgresHarness::shared().await;
    let database = harness
        .create_database()
        .await
        .expect("the harness created a database");
    let options = PgConnectOptions::from_str(&database.url)
        .expect("the harness URL parses")
        .disable_statement_logging();
    let mut admin = PgConnection::connect_with(&options)
        .await
        .expect("the bootstrap superuser connected");
    let statements = format!(
        "{}; ALTER DATABASE \"{}\" SET search_path = public, pg_catalog",
        objects(&database.name),
        database.name
    );
    execute(&mut admin, &statements).await;
    admin.close().await.expect("the admin session closed");
    Some((options, database.name))
}

/// Runs `sql` (one or more statements, no bind parameters) on `conn`,
/// panicking on an error.
pub(crate) async fn execute(conn: &mut PgConnection, sql: &str) {
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(conn)
        .await
        .expect("the test statement succeeded");
}
