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
//! With feature `serving` the fixture also holds the control schema
//! ([`Cluster::apply_control_schema`]), a registry on it, control rows
//! without a database ([`Cluster::insert_row`]), whole tenants (feature
//! `lifecycle`, [`Cluster::create_tenant`]) and the test product of
//! [`product`]: a tenant cache over it is [`Cluster::tenants`].
//!
//! Nothing is dropped per test; the container is reaped with the process.
//! Nothing here prints a password or connect options.
#![allow(dead_code, unreachable_pub)]

#[cfg(feature = "serving")]
pub mod product;

use std::future::Future;
use std::str::FromStr;
#[cfg(feature = "serving")]
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
#[cfg(feature = "serving")]
use std::time::SystemTime;

use sekvent_config::Secret;
#[cfg(feature = "serving")]
use sekvent_context::{Clock, ManualClock};
#[cfg(feature = "serving")]
use sekvent_tenancy::control::{self, NewTenant, rows};
#[cfg(feature = "serving")]
use sekvent_tenancy::{
    Connector, Registry, RoleKey, RoleSecrets, SelectorRules, TenantRow, TenantState, Tenants,
    TenantsBuilder,
};
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

/// The time the fixture writes into control rows: 1 700 000 000 s after the
/// epoch, the start of [`manual_clock`].
#[cfg(feature = "serving")]
pub fn fixture_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// A clock frozen at 1 700 000 000 s after the epoch.
#[cfg(feature = "serving")]
pub fn manual_clock() -> ManualClock {
    ManualClock::new(fixture_time())
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

    /// A pool of the control owner on the control database with
    /// `max_connections` connections and a 5 s acquire timeout (the
    /// [`control_pool`](Self::control_pool) keeps its 2).
    pub async fn control_pool_of(&self, max_connections: u32) -> PgPool {
        PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(5))
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

    /// The sessions `role` has on the server, from `pg_stat_activity`.
    pub async fn sessions_of(&self, role: &str) -> i64 {
        let mut admin = self.admin().await;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE usename = $1")
                .bind(role)
                .fetch_one(&mut admin)
                .await
                .expect("pg_stat_activity is readable");
        admin.close().await.expect("the admin session closed");
        count
    }
}

#[cfg(feature = "serving")]
impl Cluster {
    /// Applies [`control::REFERENCE_SCHEMA`] (all three tables) as the
    /// control owner.
    pub async fn apply_control_schema(&self) {
        let mut owner = PgConnection::connect_with(&self.control_options())
            .await
            .expect("the control owner connected");
        execute(&mut owner, control::REFERENCE_SCHEMA).await;
        owner.close().await.expect("the control session closed");
    }

    /// The schema applied, a pool of 4 and a registry on it.
    pub async fn registry(&self, clock: Arc<dyn Clock>) -> (Registry, PgPool) {
        self.apply_control_schema().await;
        let pool = self.control_pool_of(4).await;
        let registry = Registry::new(pool.clone(), clock).expect("a pool of 4 is enough");
        (registry, pool)
    }

    /// The fixture's master secrets: key id `k1`, context `fixture`, a
    /// secret derived from the stem.
    pub fn secrets(&self) -> RoleSecrets {
        let text = Secret::new(format!("k1:{}0123", self.stem.repeat(5)));
        RoleSecrets::parse(&text, "fixture").expect("the fixture's master secret is valid")
    }

    /// `RoleKey { key_id: "k1", generation: 1 }`.
    #[allow(clippy::unused_self)] // a method, like the fixture's other identity values
    pub fn role_key(&self) -> RoleKey {
        RoleKey {
            key_id: "k1".to_owned(),
            generation: 1,
        }
    }

    /// A connector whose base options are the bootstrap admin's (never used
    /// for a tenant: the connector replaces the role, the password and the
    /// database).
    pub fn connector(&self) -> Connector {
        Connector::new(self.admin_options("postgres"), self.secrets())
    }

    /// Inserts a control row only (no role, no database) and sets `state`.
    /// `hosts[0]` is the primary; hosts are canonical. `label` is the
    /// subdomain.
    pub async fn insert_row(
        &self,
        control: &PgPool,
        label: &str,
        hosts: &[&str],
        state: TenantState,
    ) -> TenantRow {
        self.insert_row_with(control, new_tenant(), label, hosts, state)
            .await
    }

    async fn insert_row_with(
        &self,
        control: &PgPool,
        id: TenantId,
        label: &str,
        hosts: &[&str],
        state: TenantState,
    ) -> TenantRow {
        let hosts: Vec<String> = hosts.iter().map(|host| (*host).to_owned()).collect();
        let primary_host = hosts.first().expect("a fixture row has a host").clone();
        let tenant = NewTenant {
            id,
            subdomain: label.to_owned(),
            display_name: label.to_owned(),
            hosts,
            primary_host,
            database: self.names().database(id),
            role: self.names().role(id),
            role_key: self.role_key(),
        };
        let mut tx = control.begin().await.expect("a control transaction began");
        let (_, inserted) = rows::insert(&mut tx, &tenant, fixture_time())
            .await
            .expect("the fixture row was inserted");
        assert!(inserted, "the fixture row is new");
        if state != TenantState::Provisioning {
            let changed = rows::set_state(&mut tx, id, state, fixture_time())
                .await
                .expect("the fixture row's state was set");
            assert!(changed, "the fixture row exists");
        }
        tx.commit().await.expect("the fixture row was committed");
        let mut conn = control.acquire().await.expect("a control connection");
        rows::get(&mut conn, id)
            .await
            .expect("the fixture row is readable")
            .expect("the fixture row exists")
    }

    /// Sets a row's state in a transaction of its own.
    pub async fn set_state(&self, control: &PgPool, id: TenantId, state: TenantState) {
        let mut tx = control.begin().await.expect("a control transaction began");
        let changed = rows::set_state(&mut tx, id, state, fixture_time())
            .await
            .expect("the row's state was set");
        assert!(changed, "the row exists");
        tx.commit().await.expect("the state was committed");
    }

    /// Replaces a row's hosts and subdomain in a transaction of its own.
    /// `hosts[0]` is the primary.
    pub async fn set_hosts(&self, control: &PgPool, id: TenantId, label: &str, hosts: &[&str]) {
        let hosts: Vec<String> = hosts.iter().map(|host| (*host).to_owned()).collect();
        let primary_host = hosts.first().expect("a fixture row has a host").clone();
        let mut tx = control.begin().await.expect("a control transaction began");
        rows::set_hosts(&mut tx, id, label, &hosts, &primary_host, fixture_time())
            .await
            .expect("the row's hosts were set");
        tx.commit().await.expect("the hosts were committed");
    }

    /// A whole tenant: the role (verifier of the derived password,
    /// connection limit 20) and the database through the crate's
    /// maintenance connection, then the control row, `Active`.
    #[cfg(feature = "lifecycle")]
    pub async fn create_tenant(&self, control: &PgPool, label: &str, hosts: &[&str]) -> TenantRow {
        use sekvent_tenancy::ScramVerifier;
        use sekvent_tenancy::cluster::{databases, roles};

        let id = new_tenant();
        let role = self.names().role(id);
        let database = self.names().database(id);
        let password = self
            .secrets()
            .derive(id, &self.role_key())
            .expect("the fixture key derives a password");
        let verifier = ScramVerifier::new(&password).expect("a verifier of the password");
        let mut conn = self
            .maintenance()
            .connect()
            .await
            .expect("the maintenance role connected");
        roles::create(&mut conn, &role, &verifier, 20)
            .await
            .expect("the tenant role was created");
        databases::create(&mut conn, &database, &role)
            .await
            .expect("the tenant database was created");
        conn.close().await.expect("the maintenance session closed");
        self.insert_row_with(control, id, label, hosts, TenantState::Active)
            .await
    }

    /// Tenants over the test product with `capacity`, 2 connections per
    /// tenant and default domains.
    pub fn tenants(
        &self,
        registry: &Registry,
        rules: SelectorRules,
        factory: &product::OrdersFactory,
        capacity: usize,
    ) -> Tenants<product::OrdersScope> {
        self.tenants_builder(registry, rules, factory)
            .capacity(capacity)
            .build()
            .expect("the tenant cache settings are valid")
    }

    /// The builder behind [`tenants`](Self::tenants), for a test that changes
    /// a setting.
    pub fn tenants_builder(
        &self,
        registry: &Registry,
        rules: SelectorRules,
        factory: &product::OrdersFactory,
    ) -> TenantsBuilder<product::OrdersScope> {
        Tenants::builder(
            registry.clone(),
            self.connector(),
            self.names().clone(),
            rules,
            &product::TENANT_MIGRATOR,
            factory.clone(),
        )
        .max_connections(2)
    }
}
