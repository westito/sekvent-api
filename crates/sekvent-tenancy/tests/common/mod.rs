//! The Docker fixture of the integration tests.
//!
//! One Postgres server per test binary ([`PostgresHarness::shared`]) and, per
//! test, a cluster identity of its own: roles are server-wide, so every name
//! below is derived from the test's unique control database and tests on one
//! server never see each other's roles.
//!
//! - The **maintenance role** is not a superuser: `LOGIN CREATEDB CREATEROLE`
//!   with a known password. It owns no database: its maintenance database is
//!   the bootstrap admin's, and it holds an explicit `CONNECT` there.
//! - A separate `LOGIN` **control owner** owns the control database.
//! - `CONNECT` and `TEMPORARY` are revoked from `PUBLIC` on both, so the
//!   preflight passes on a fresh fixture.
//! - The bootstrap admin (a superuser) is used only to set up, to create
//!   drift on purpose and to observe.
//!
//! A test skips when Docker tests are off:
//!
//! ```ignore
//! #[tokio::test]
//! #[ignore = "needs Docker"]
//! async fn a_scenario() {
//!     let Some(cluster) = common::Cluster::start().await else { return };
//!     common::guard(async { /* … */ }).await;
//! }
//! ```
//!
//! Nothing is dropped per test; the container is reaped with the process.
//! Nothing here prints a password or connect options.
#![allow(dead_code, unreachable_pub)]

use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sekvent_config::Secret;
use sekvent_tenancy::{ObjectNames, TenantId};
use sekvent_testing::{PostgresHarness, docker_tests_enabled};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, PgConnection};
use uuid::Uuid;

/// The bound of one scenario.
pub const SCENARIO_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `scenario` under [`SCENARIO_TIMEOUT`]; panics when it takes longer.
pub async fn guard<F: Future>(scenario: F) -> F::Output {
    tokio::time::timeout(SCENARIO_TIMEOUT, scenario)
        .await
        .expect("the scenario finished within its 30 s guard")
}

/// A fresh random tenant id.
pub fn new_tenant() -> TenantId {
    TenantId::from_uuid(Uuid::new_v4())
}

/// A fresh random password: 32 hex digits, safe in a SQL literal.
pub fn new_password() -> Secret {
    Secret::new(Uuid::new_v4().simple().to_string())
}

/// Runs `sql` (one or more statements, no bind parameters) on `conn`.
pub async fn try_execute(conn: &mut PgConnection, sql: &str) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(AssertSqlSafe(sql)).execute(conn).await?;
    Ok(())
}

/// [`try_execute`], panicking on an error.
pub async fn execute(conn: &mut PgConnection, sql: &str) {
    try_execute(conn, sql)
        .await
        .expect("the fixture statement succeeded");
}

/// `"name"`. Fixture names are lower-case letters, digits and underscores.
pub fn quote(name: &str) -> String {
    assert!(
        name.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "fixture names are lower-case letters, digits and underscores"
    );
    format!("\"{name}\"")
}

/// One test's cluster identity on the shared server.
pub struct Cluster {
    harness: &'static PostgresHarness,
    /// 12 hex digits taken from the control database's unique name.
    stem: String,
    names: ObjectNames,
    control_database: String,
    control_owner: String,
    control_password: Secret,
    maintenance_role: String,
    maintenance_password: Secret,
    maintenance_database: String,
    next: AtomicU32,
}

impl Cluster {
    /// The fixture of one test; `None` when Docker tests are off.
    pub async fn start() -> Option<Self> {
        if !docker_tests_enabled() {
            return None;
        }
        let harness = PostgresHarness::shared().await;
        let control = harness
            .create_database()
            .await
            .expect("the harness created the control database");
        // The harness names databases `t_` and 32 hex digits; 12 of them keep
        // every derived name short and still unique on the server.
        let stem = control
            .name
            .get(2..14)
            .expect("a harness database name has 32 hex digits")
            .to_owned();
        let names = ObjectNames::new(&format!("app_{stem}_")).expect("the fixture prefix is valid");
        let this = Self {
            harness,
            control_database: control.name,
            control_owner: format!("fx_{stem}_control"),
            control_password: new_password(),
            maintenance_role: format!("fx_{stem}_maintenance"),
            maintenance_password: new_password(),
            maintenance_database: format!("fx_{stem}_maintenance"),
            names,
            stem,
            next: AtomicU32::new(0),
        };
        this.set_up().await;
        Some(this)
    }

    async fn set_up(&self) {
        let maintenance = quote(&self.maintenance_role);
        let maintenance_db = quote(&self.maintenance_database);
        let owner = quote(&self.control_owner);
        let control = quote(&self.control_database);
        let mut admin = self.admin().await;
        for statement in [
            format!(
                "CREATE ROLE {maintenance} LOGIN CREATEDB CREATEROLE PASSWORD '{}'",
                self.maintenance_password.expose()
            ),
            format!(
                "CREATE ROLE {owner} LOGIN PASSWORD '{}'",
                self.control_password.expose()
            ),
            format!("CREATE DATABASE {maintenance_db}"),
            format!("REVOKE CONNECT, TEMPORARY ON DATABASE {maintenance_db} FROM PUBLIC"),
            format!("GRANT CONNECT ON DATABASE {maintenance_db} TO {maintenance}"),
            format!("ALTER DATABASE {control} OWNER TO {owner}"),
            format!("REVOKE CONNECT, TEMPORARY ON DATABASE {control} FROM PUBLIC"),
        ] {
            execute(&mut admin, &statement).await;
        }
        admin.close().await.expect("the admin session closed");
    }

    /// The shared server.
    pub fn harness(&self) -> &'static PostgresHarness {
        self.harness
    }

    /// The test's object names (prefix `app_<stem>_`).
    pub fn names(&self) -> &ObjectNames {
        &self.names
    }

    /// The test's object-name prefix.
    pub fn prefix(&self) -> String {
        format!("app_{}_", self.stem)
    }

    /// The control database.
    pub fn control_database(&self) -> &str {
        &self.control_database
    }

    /// The `LOGIN` role that owns the control database.
    pub fn control_owner(&self) -> &str {
        &self.control_owner
    }

    /// The control owner's password.
    pub fn control_password(&self) -> &Secret {
        &self.control_password
    }

    /// The control owner on the control database.
    pub fn control_options(&self) -> PgConnectOptions {
        self.options_for(
            &self.control_owner,
            &self.control_password,
            &self.control_database,
        )
    }

    /// A small pool of the control owner on the control database.
    pub async fn control_pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(2)
            .connect_with(self.control_options())
            .await
            .expect("the control owner connected")
    }

    /// The maintenance role (`LOGIN CREATEDB CREATEROLE`, no superuser).
    pub fn maintenance_role(&self) -> &str {
        &self.maintenance_role
    }

    /// The maintenance role's password.
    pub fn maintenance_password(&self) -> &Secret {
        &self.maintenance_password
    }

    /// The maintenance database: the bootstrap admin's, with an explicit
    /// `CONNECT` for the maintenance role.
    pub fn maintenance_database(&self) -> &str {
        &self.maintenance_database
    }

    /// The maintenance role on the maintenance database, without any of the
    /// crate's session settings.
    pub fn maintenance_options(&self) -> PgConnectOptions {
        self.options_for(
            &self.maintenance_role,
            &self.maintenance_password,
            &self.maintenance_database,
        )
    }

    /// The crate's maintenance connection for this test's names.
    #[cfg(feature = "lifecycle")]
    pub fn maintenance(&self) -> sekvent_tenancy::Maintenance {
        sekvent_tenancy::Maintenance::new(self.maintenance_options(), self.names.clone())
    }

    /// `role` with `password` on `database` of the shared server.
    pub fn options_for(&self, role: &str, password: &Secret, database: &str) -> PgConnectOptions {
        PgConnectOptions::from_str(&self.harness.admin_url())
            .expect("the harness URL parses")
            .username(role)
            .password(password.expose())
            .database(database)
            .disable_statement_logging()
    }

    /// A session of `role` with `password` on `database`.
    pub async fn connect_as(
        &self,
        role: &str,
        password: &Secret,
        database: &str,
    ) -> Result<PgConnection, sqlx::Error> {
        PgConnection::connect_with(&self.options_for(role, password, database)).await
    }

    /// The bootstrap admin (a superuser) on `database`.
    pub fn admin_options(&self, database: &str) -> PgConnectOptions {
        PgConnectOptions::from_str(&self.harness.admin_url())
            .expect("the harness URL parses")
            .database(database)
            .disable_statement_logging()
    }

    /// A bootstrap-admin session on `postgres`.
    pub async fn admin(&self) -> PgConnection {
        self.admin_on("postgres").await
    }

    /// A bootstrap-admin session on `database`.
    pub async fn admin_on(&self, database: &str) -> PgConnection {
        PgConnection::connect_with(&self.admin_options(database))
            .await
            .expect("the bootstrap admin connected")
    }

    /// Runs `sql` as the bootstrap admin on `database`, on a session closed
    /// after.
    pub async fn admin_execute(&self, database: &str, sql: &str) {
        let mut admin = self.admin_on(database).await;
        execute(&mut admin, sql).await;
        admin.close().await.expect("the admin session closed");
    }

    /// `fx_<stem>_<label>_<n>`: unique on the server, never tenant-named.
    pub fn unique_name(&self, label: &str) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let name = format!("fx_{}_{label}_{n}", self.stem);
        quote(&name);
        name
    }

    /// `<prefix><label>_<n>`: tenant-named (it has the prefix) but not an
    /// object name.
    pub fn prefixed_name(&self, label: &str) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}{label}_{n}", self.prefix());
        quote(&name);
        name
    }

    /// Creates role `name` as the bootstrap admin with `attributes` (SQL, for
    /// example `LOGIN SUPERUSER`; empty for a `NOLOGIN` role).
    pub async fn create_role_named(&self, name: &str, attributes: &str) {
        let statement = format!("CREATE ROLE {} {attributes}", quote(name));
        self.admin_execute("postgres", &statement).await;
    }

    /// Creates a uniquely named role as the bootstrap admin; returns its name.
    pub async fn create_role(&self, label: &str, attributes: &str) -> String {
        let name = self.unique_name(label);
        self.create_role_named(&name, attributes).await;
        name
    }

    /// Creates a uniquely named `LOGIN` role with a fresh password.
    pub async fn create_login_role(&self, label: &str) -> (String, Secret) {
        let name = self.unique_name(label);
        let password = new_password();
        let statement = format!(
            "CREATE ROLE {} LOGIN PASSWORD '{}'",
            quote(&name),
            password.expose()
        );
        self.admin_execute("postgres", &statement).await;
        (name, password)
    }

    /// Creates database `name` owned by `owner` as the bootstrap admin, with
    /// the default ACL (`PUBLIC` holds `CONNECT` and `TEMPORARY`).
    pub async fn create_database_named(&self, name: &str, owner: &str) {
        let statement = format!("CREATE DATABASE {} OWNER {}", quote(name), quote(owner));
        self.admin_execute("postgres", &statement).await;
    }

    /// Creates a uniquely named database owned by `owner`; returns its name.
    pub async fn create_database(&self, label: &str, owner: &str) -> String {
        let name = self.unique_name(label);
        self.create_database_named(&name, owner).await;
        name
    }

    /// The number of sessions on `database`, from `pg_stat_activity`.
    pub async fn sessions(&self, database: &str) -> i64 {
        let mut admin = self.admin().await;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = $1")
                .bind(database)
                .fetch_one(&mut admin)
                .await
                .expect("pg_stat_activity is readable");
        admin.close().await.expect("the admin session closed");
        count
    }
}
