//! Adoption on a real server: an existing database becomes a tenant's after the handover's checks
//! and the product's, each refusal leaving no control row and no role, repeats and resumption.
#![cfg(feature = "lifecycle")]

mod common;

use std::borrow::Cow;
use std::error::Error as _;
use std::sync::{Arc, Mutex, PoisonError};

use common::Cluster;
use common::product::hooks::{ADOPTED, CHECK_ADOPTABLE, OrdersHooks};
use common::product::{self, OrdersFactory};
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::cluster::handover::CHECK_KEY;
use sekvent_tenancy::cluster::privileges;
use sekvent_tenancy::control::{ProvisionStep, rows};
use sekvent_tenancy::invalidation::Sender;
use sekvent_tenancy::lifecycle::{
    AdoptCandidate, AdoptRequest, HookFuture, NOT_CHECKED, Provisioner, TenantHooks,
};
use sekvent_tenancy::{Registry, SelectorRules, TenantId, TenantState, reasons};
use sqlx::{Connection, PgConnection, PgPool};

/// The control registry and its pool, on the fixture's frozen clock.
async fn registry(cluster: &Cluster) -> (Registry, PgPool) {
    cluster.registry(Arc::new(common::manual_clock())).await
}

/// A provisioner like the fixture's, over any hooks.
fn provisioner_with<H: TenantHooks>(
    cluster: &Cluster,
    registry: &Registry,
    hooks: H,
) -> Provisioner<H> {
    let sender = Sender::new(registry.pool().clone(), cluster.channel());
    Provisioner::builder(
        registry.clone(),
        cluster.maintenance(),
        cluster.connector(),
        &product::TENANT_MIGRATOR,
        sender,
        hooks,
    )
    .role_key_id("k1")
    .connection_limit(20)
    .tenant_cap(100)
    .clock(Arc::new(common::manual_clock()))
    .build()
    .expect("the provisioner settings are valid")
}

/// An adoption of `database` for a new tenant labelled `label` on `<label>.example.com`.
fn request(label: &str, database: &str) -> AdoptRequest {
    let host = format!("{label}.example.com");
    AdoptRequest {
        tenant: common::new_tenant(),
        subdomain: label.to_owned(),
        display_name: label.to_owned(),
        database: database.to_owned(),
        hosts: vec![host.clone()],
        primary_host: host,
    }
}

async fn adopt<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    request: AdoptRequest,
) -> Result<sekvent_tenancy::TenantRow, AppError> {
    Box::pin(provisioner.adopt(request)).await
}

/// A database the maintenance role creates, and so owns, with the default ACL.
async fn candidate(cluster: &Cluster) -> String {
    let name = cluster.unique_name("legacy");
    let mut conn = PgConnection::connect_with(&cluster.maintenance_options())
        .await
        .expect("the maintenance role connected");
    common::execute(
        &mut conn,
        &format!("CREATE DATABASE {}", common::quote(&name)),
    )
    .await;
    conn.close().await.expect("the session closed");
    name
}

/// A session of the maintenance role on `database`.
async fn as_maintenance(cluster: &Cluster, database: &str) -> PgConnection {
    cluster
        .connect_as(
            cluster.maintenance_role(),
            cluster.maintenance_password(),
            database,
        )
        .await
        .expect("the maintenance role connected")
}

/// A candidate as a product left it: the tenant schema migrated by the maintenance role (so
/// `orders` and `_sqlx_migrations` are its) and one order, item `kept`.
async fn product_candidate(cluster: &Cluster) -> String {
    let name = candidate(cluster).await;
    let mut conn = as_maintenance(cluster, &name).await;
    product::TENANT_MIGRATOR
        .run(&mut conn)
        .await
        .expect("the product's schema was applied");
    common::execute(&mut conn, "INSERT INTO orders (item) VALUES ('kept')").await;
    conn.close().await.expect("the session closed");
    name
}

/// Waits until no session is left on `database`: a closed session leaves `pg_stat_activity` a
/// moment after its client is gone.
async fn no_sessions(cluster: &Cluster, database: &str) {
    sekvent_testing::await_until!(cluster.sessions(database).await == 0);
}

async fn role_exists(cluster: &Cluster, role: &str) -> bool {
    let mut admin = cluster.admin().await;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(role)
            .fetch_one(&mut admin)
            .await
            .expect("pg_roles is readable");
    admin.close().await.expect("the admin session closed");
    exists
}

/// No control row and no role of `tenant`.
async fn nothing_left(cluster: &Cluster, control: &PgPool, tenant: TenantId) {
    let mut conn = control.acquire().await.expect("a control connection");
    let row = rows::get(&mut conn, tenant)
        .await
        .expect("the control row is readable");
    assert!(row.is_none(), "no control row: {row:?}");
    drop(conn);
    let role = cluster.names().role(tenant);
    assert!(
        !role_exists(cluster, role.as_str()).await,
        "the tenant role was not created"
    );
}

/// The owner of `database`.
async fn database_owner(cluster: &Cluster, database: &str) -> String {
    let mut admin = cluster.admin().await;
    let owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname = $1",
    )
    .bind(database)
    .fetch_one(&mut admin)
    .await
    .expect("the database exists");
    admin.close().await.expect("the admin session closed");
    owner
}

/// Whether `PUBLIC` holds `CONNECT` on `database`.
async fn public_connect(cluster: &Cluster, database: &str) -> bool {
    let mut admin = cluster.admin().await;
    let connect = privileges::public_may_connect(&mut admin, database)
        .await
        .expect("the ACL is readable")
        .expect("the database exists");
    admin.close().await.expect("the admin session closed");
    connect
}

/// The check a handover refusal names, after its shape is asserted.
fn refused_check(err: &AppError) -> String {
    assert_eq!(err.code(), ErrorCode::FailedPrecondition, "{err:?}");
    assert_eq!(err.reason(), Some(reasons::ADOPTION_REFUSED));
    assert_eq!(err.message(), "the database cannot be adopted");
    err.metadata()
        .get(CHECK_KEY)
        .cloned()
        .expect("the refusal names its check")
}

fn sqlstate(err: &sqlx::Error) -> Option<String> {
    sekvent_db::sqlstate(err).map(Cow::into_owned)
}

/// Adopts `database` with the test product's hooks and expects the handover refusal `check`,
/// leaving no row and no role.
async fn refused_with(cluster: &Cluster, database: &str, check: &str) {
    let (registry, control) = registry(cluster).await;
    let hooks = OrdersHooks::new();
    let provisioner = cluster.provisioner(&registry, &hooks);
    let request = request("orders", database);
    let tenant = request.tenant;
    let err = adopt(&provisioner, request)
        .await
        .expect_err("the handover refuses");
    assert_eq!(refused_check(&err), check);
    assert_eq!(hooks.count(CHECK_ADOPTABLE), 0, "the product was not asked");
    nothing_left(cluster, &control, tenant).await;
}

fn not_ours() -> AppError {
    AppError::failed_precondition("not a database of this product").with_reason("NOT_OURS")
}

fn adopted_failed() -> AppError {
    AppError::unavailable("the product could not take the database over")
        .with_reason("ADOPTED_FAILED")
}

/// The test product's hooks without `check_adoptable`: every database is refused.
struct Defaults;

impl TenantHooks for Defaults {
    type Seed = ();
}

/// Hooks whose check tries a write and records the SQLSTATE it got (`written` when the write
/// went through), then accepts.
#[derive(Clone, Default)]
struct WritingHooks {
    outcomes: Arc<Mutex<Vec<String>>>,
}

impl WritingHooks {
    fn outcomes(&self) -> Vec<String> {
        self.outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl TenantHooks for WritingHooks {
    type Seed = ();

    fn check_adoptable<'a>(
        &'a self,
        candidate: &'a AdoptCandidate,
        pool: &'a PgPool,
    ) -> HookFuture<'a, ()> {
        let _ = candidate;
        Box::pin(async move {
            let outcome = match sqlx::query("CREATE TABLE probe (id bigint)")
                .execute(pool)
                .await
            {
                Ok(_) => "written".to_owned(),
                Err(err) => sqlstate(&err).unwrap_or_else(|| "none".to_owned()),
            };
            self.outcomes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(outcome);
            Ok(())
        })
    }
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn an_adoption_hands_the_database_over_with_its_data() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, _control) = registry(&cluster).await;
        let factory = OrdersFactory::new();
        let tenants = cluster.tenants(&registry, SelectorRules::new(), &factory, 8);
        let hooks = OrdersHooks::new();
        let provisioner = cluster
            .provisioner_builder(&registry, &hooks)
            .tenants(tenants.clone())
            .build()
            .expect("the provisioner settings are valid");
        let name = product_candidate(&cluster).await;
        let request = request("orders", &name);
        let tenant = request.tenant;
        let role = cluster.names().role(tenant);

        let row = adopt(&provisioner, request)
            .await
            .expect("the database was adopted");
        assert_eq!(row.info.id, tenant);
        assert_eq!(row.info.state, TenantState::Active);
        assert_eq!(row.provision_step, ProvisionStep::Active);
        assert_eq!(row.info.subdomain, "orders");
        assert_eq!(row.info.primary_host, "orders.example.com");
        assert_eq!(row.info.role_key, cluster.role_key());
        assert_eq!(row.db_name, name);
        assert_eq!(row.db_role, role.as_str());
        assert_eq!(row.schema_version, Some(product::LATEST_TENANT_VERSION));
        // The product is asked before the row exists and again in the database step, while
        // the maintenance role still owns the database; `adopted` runs once.
        assert_eq!(hooks.calls(), [CHECK_ADOPTABLE, CHECK_ADOPTABLE, ADOPTED]);

        assert_eq!(database_owner(&cluster, &name).await, role.as_str());
        let mut admin = cluster.admin_on(&name).await;
        let owners: Vec<(String, String)> = sqlx::query_as(
            "SELECT relname::text, pg_get_userbyid(relowner)::text FROM pg_class
              WHERE relname IN ('orders', '_sqlx_migrations') AND relkind = 'r'
              ORDER BY 1",
        )
        .fetch_all(&mut admin)
        .await
        .expect("the tables are readable");
        admin.close().await.expect("the admin session closed");
        assert_eq!(
            owners,
            [
                ("_sqlx_migrations".to_owned(), role.as_str().to_owned()),
                ("orders".to_owned(), role.as_str().to_owned()),
            ]
        );

        let password = cluster
            .secrets()
            .derive(tenant, &cluster.role_key())
            .expect("the fixture key derives a password");
        let mut conn = cluster
            .connect_as(role.as_str(), &password, &name)
            .await
            .expect("the tenant role signs in with its derived password");
        let items: Vec<String> = sqlx::query_scalar("SELECT item FROM orders ORDER BY id")
            .fetch_all(&mut conn)
            .await
            .expect("the orders are readable");
        conn.close().await.expect("the session closed");
        assert_eq!(items, ["kept"]);

        let refused = cluster
            .connect_as(
                cluster.maintenance_role(),
                cluster.maintenance_password(),
                &name,
            )
            .await
            .expect_err("the maintenance role may no longer connect");
        assert_eq!(sqlstate(&refused).as_deref(), Some("42501"));

        let lease = tenants
            .get(tenant)
            .await
            .expect("the cache serves the tenant");
        assert_eq!(lease.tenant, tenant);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
            .fetch_one(&lease.pool)
            .await
            .expect("the tenant's pool answers");
        assert_eq!(count, 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_product_check_runs_read_only() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, _control) = registry(&cluster).await;
        let hooks = WritingHooks::default();
        let provisioner = provisioner_with(&cluster, &registry, hooks.clone());
        let name = product_candidate(&cluster).await;
        let row = adopt(&provisioner, request("orders", &name))
            .await
            .expect("the database was adopted");
        assert_eq!(row.info.state, TenantState::Active);
        let outcomes = hooks.outcomes();
        assert!(!outcomes.is_empty(), "the check ran");
        assert!(
            outcomes.iter().all(|outcome| outcome == "25006"),
            "every write was refused as read-only: {outcomes:?}"
        );
        let mut admin = cluster.admin_on(&name).await;
        let absent: bool = sqlx::query_scalar("SELECT to_regclass('public.probe') IS NULL")
            .fetch_one(&mut admin)
            .await
            .expect("the catalog is readable");
        admin.close().await.expect("the admin session closed");
        assert!(absent, "the check wrote nothing");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_product_refusal_reaches_the_caller_and_leaves_nothing() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, control) = registry(&cluster).await;
        let hooks = OrdersHooks::new();
        hooks.fail(CHECK_ADOPTABLE, Some(not_ours));
        let provisioner = cluster.provisioner(&registry, &hooks);
        let name = product_candidate(&cluster).await;
        let request = request("orders", &name);
        let tenant = request.tenant;
        let err = adopt(&provisioner, request)
            .await
            .expect_err("the product refuses");
        assert_eq!(err.code(), ErrorCode::FailedPrecondition);
        assert_eq!(err.reason(), Some("NOT_OURS"));
        assert_eq!(err.message(), "not a database of this product");
        assert_eq!(hooks.calls(), [CHECK_ADOPTABLE]);
        nothing_left(&cluster, &control, tenant).await;
        assert_eq!(
            database_owner(&cluster, &name).await,
            cluster.maintenance_role()
        );
    }))
    .await;
}

/// A role of the tenant's name that exists before the adoption and fails the tenant-role check
/// is refused by the handover's first step: before `PUBLIC`'s `CONNECT` is revoked, before the
/// product is asked and before a control row exists.
#[tokio::test]
#[ignore = "needs Docker"]
async fn an_existing_role_with_drift_is_refused_before_anything_changes() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, control) = registry(&cluster).await;
        let hooks = OrdersHooks::new();
        let provisioner = cluster.provisioner(&registry, &hooks);
        let name = product_candidate(&cluster).await;
        let request = request("orders", &name);
        let tenant = request.tenant;
        let role = cluster.names().role(tenant);
        cluster
            .create_role_named(role.as_str(), "NOINHERIT CREATEDB")
            .await;
        assert!(public_connect(&cluster, &name).await);

        let err = adopt(&provisioner, request)
            .await
            .expect_err("the drifted role is refused");
        assert_eq!(err.code(), ErrorCode::Internal, "{err:?}");
        assert_eq!(err.reason(), None);
        let why = err.source().map(ToString::to_string).unwrap_or_default();
        assert!(
            why.contains(&format!("tenant role {} is not as expected", role.as_str())),
            "{why}"
        );
        assert!(why.contains("it holds CREATEDB"), "{why}");
        assert_eq!(hooks.count(CHECK_ADOPTABLE), 0, "the product was not asked");
        assert!(
            public_connect(&cluster, &name).await,
            "PUBLIC's CONNECT was not revoked"
        );
        assert_eq!(
            database_owner(&cluster, &name).await,
            cluster.maintenance_role()
        );
        let mut conn = control.acquire().await.expect("a control connection");
        let row = rows::get(&mut conn, tenant)
            .await
            .expect("the control row is readable");
        assert!(row.is_none(), "no control row: {row:?}");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_default_hooks_refuse_every_database() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, control) = registry(&cluster).await;
        let provisioner = provisioner_with(&cluster, &registry, Defaults);
        let name = candidate(&cluster).await;
        let request = request("orders", &name);
        let tenant = request.tenant;
        let err = adopt(&provisioner, request)
            .await
            .expect_err("the default check refuses");
        assert_eq!(refused_check(&err), NOT_CHECKED);
        nothing_left(&cluster, &control, tenant).await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_missing_database_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let name = cluster.unique_name("absent");
        refused_with(&cluster, &name, "missing").await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_template_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let name = candidate(&cluster).await;
        cluster
            .admin_execute(
                "postgres",
                &format!("ALTER DATABASE {} IS_TEMPLATE true", common::quote(&name)),
            )
            .await;
        refused_with(&cluster, &name, "template").await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_database_of_a_third_role_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let third = cluster.create_role("owner", "").await;
        let name = cluster.create_database("foreign", &third).await;
        refused_with(&cluster, &name, "owner").await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_grant_to_a_third_role_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let name = candidate(&cluster).await;
        let third = cluster.create_role("reader", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT CONNECT ON DATABASE {} TO {}",
                    common::quote(&name),
                    common::quote(&third)
                ),
            )
            .await;
        refused_with(&cluster, &name, "grantee").await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_maintenance_role_owning_another_database_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let name = candidate(&cluster).await;
        let _other = candidate(&cluster).await;
        refused_with(&cluster, &name, "owns_more").await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn an_object_of_a_third_role_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let name = candidate(&cluster).await;
        let third = cluster.create_role("stray", "").await;
        cluster
            .admin_execute(
                &name,
                &format!(
                    "CREATE TABLE public.stray (id bigint); ALTER TABLE public.stray OWNER TO {}",
                    common::quote(&third)
                ),
            )
            .await;
        no_sessions(&cluster, &name).await;
        refused_with(&cluster, &name, "foreign_object").await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_session_of_another_role_is_refused_until_it_leaves() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let name = product_candidate(&cluster).await;
        let (visitor, visitor_password) = cluster.create_login_role("visitor").await;
        let session = cluster
            .connect_as(&visitor, &visitor_password, &name)
            .await
            .expect("PUBLIC may connect");
        let (registry, control) = registry(&cluster).await;
        let hooks = OrdersHooks::new();
        let provisioner = cluster.provisioner(&registry, &hooks);
        let first = request("orders", &name);
        let tenant = first.tenant;
        let err = adopt(&provisioner, first.clone())
            .await
            .expect_err("a connected session is refused");
        assert_eq!(refused_check(&err), "session");
        assert_eq!(hooks.count(CHECK_ADOPTABLE), 0);
        nothing_left(&cluster, &control, tenant).await;

        session.close().await.expect("the visitor left");
        no_sessions(&cluster, &name).await;
        let row = adopt(&provisioner, first)
            .await
            .expect("the repeat adopts the database");
        assert_eq!(row.info.state, TenantState::Active);
        assert_eq!(
            database_owner(&cluster, &name).await,
            cluster.names().role(tenant).as_str()
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn a_database_field_the_rules_refuse_is_invalid() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, control) = registry(&cluster).await;
        let hooks = OrdersHooks::new();
        let provisioner = cluster.provisioner(&registry, &hooks);
        let prefixed = cluster.prefixed_name("legacy");
        let object = cluster
            .names()
            .database(common::new_tenant())
            .as_str()
            .to_owned();
        for database in [
            prefixed.as_str(),
            object.as_str(),
            cluster.control_database(),
            cluster.maintenance_database(),
            "postgres",
            "template1",
            "Legacy_orders",
            "",
        ] {
            let request = request("orders", database);
            let tenant = request.tenant;
            let err = adopt(&provisioner, request)
                .await
                .expect_err("the database field is refused");
            assert_eq!(err.code(), ErrorCode::InvalidArgument, "{database:?}");
            let fields: Vec<&str> = err
                .field_violations()
                .iter()
                .map(|violation| violation.field.as_str())
                .collect();
            assert_eq!(fields, ["database"], "{database:?}");
            for violation in err.field_violations() {
                if !database.is_empty() {
                    assert!(
                        !violation.description.contains(database),
                        "{}",
                        violation.description
                    );
                }
            }
            nothing_left(&cluster, &control, tenant).await;
        }

        let mut both = request("-orders", "Legacy");
        both.hosts = vec!["orders.example.com".to_owned()];
        both.primary_host = "orders.example.com".to_owned();
        let tenant = both.tenant;
        let err = adopt(&provisioner, both)
            .await
            .expect_err("every problem is reported");
        let fields: Vec<&str> = err
            .field_violations()
            .iter()
            .map(|violation| violation.field.as_str())
            .collect();
        assert_eq!(fields, ["subdomain", "database"]);
        assert_eq!(hooks.calls(), [] as [&str; 0]);
        nothing_left(&cluster, &control, tenant).await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_repeat_returns_the_active_row_and_a_mismatch_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, _control) = registry(&cluster).await;
        let hooks = OrdersHooks::new();
        let provisioner = cluster.provisioner(&registry, &hooks);
        let name = product_candidate(&cluster).await;
        let first = request("orders", &name);
        let row = adopt(&provisioner, first.clone())
            .await
            .expect("the database was adopted");
        let calls = hooks.calls();
        assert_eq!(hooks.count(ADOPTED), 1);

        let again = adopt(&provisioner, first.clone())
            .await
            .expect("the repeat returns the row");
        assert_eq!(again, row);
        assert_eq!(hooks.calls(), calls, "no hook ran again");

        let other_database = AdoptRequest {
            database: cluster.unique_name("other"),
            ..first.clone()
        };
        let err = adopt(&provisioner, other_database)
            .await
            .expect_err("another database is refused");
        assert_eq!(err.reason(), Some(reasons::TENANT_STATE));

        let other_name = AdoptRequest {
            display_name: "Orders Ltd".to_owned(),
            ..first
        };
        let err = adopt(&provisioner, other_name)
            .await
            .expect_err("another display name is refused");
        assert_eq!(err.reason(), Some(reasons::TENANT_STATE));
        assert_eq!(hooks.calls(), calls, "no hook ran for a refused repeat");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_new_tenant_at_the_cap_is_refused_without_a_role() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, control) = registry(&cluster).await;
        cluster
            .create_tenant(&control, "first", &["first.example.com"])
            .await;
        let hooks = OrdersHooks::new();
        let provisioner = cluster
            .provisioner_builder(&registry, &hooks)
            .tenant_cap(1)
            .build()
            .expect("the provisioner settings are valid");
        let name = candidate(&cluster).await;
        let request = request("orders", &name);
        let tenant = request.tenant;
        let err = adopt(&provisioner, request)
            .await
            .expect_err("the cap is reached");
        assert_eq!(err.reason(), Some(reasons::TENANT_LIMIT_REACHED));
        nothing_left(&cluster, &control, tenant).await;
        assert_eq!(
            database_owner(&cluster, &name).await,
            cluster.maintenance_role()
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn an_adoption_resumes_after_its_last_hook_failed() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let (registry, control) = registry(&cluster).await;
        let hooks = OrdersHooks::new();
        hooks.fail(ADOPTED, Some(adopted_failed));
        let provisioner = cluster.provisioner(&registry, &hooks);
        let name = product_candidate(&cluster).await;
        let first = request("orders", &name);
        let tenant = first.tenant;
        let role = cluster.names().role(tenant);

        let err = adopt(&provisioner, first.clone())
            .await
            .expect_err("the product's last hook fails");
        assert_eq!(err.reason(), Some("ADOPTED_FAILED"));
        assert_eq!(
            err.message(),
            "the product could not take the database over"
        );
        let mut conn = control.acquire().await.expect("a control connection");
        let row = rows::get(&mut conn, tenant)
            .await
            .expect("the control row is readable")
            .expect("the row stays");
        drop(conn);
        assert_eq!(row.info.state, TenantState::Provisioning);
        assert_eq!(row.provision_step, ProvisionStep::Migrated);
        assert_eq!(database_owner(&cluster, &name).await, role.as_str());
        let checks = hooks.count(CHECK_ADOPTABLE);

        hooks.fail(ADOPTED, None);
        let row = adopt(&provisioner, first)
            .await
            .expect("the repeat finishes the adoption");
        assert_eq!(row.info.state, TenantState::Active);
        assert_eq!(row.provision_step, ProvisionStep::Active);
        assert_eq!(hooks.count(ADOPTED), 2);
        assert_eq!(
            hooks.count(CHECK_ADOPTABLE),
            checks,
            "no second check and no second handover"
        );
    }))
    .await;
}
