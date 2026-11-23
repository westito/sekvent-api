//! Provisioning on a real server: end to end, repeats, validation, a resume after a failure at
//! each step, the cap, mismatching repeats, held names, refused states, the lifecycle admission,
//! the announce reaching this process's tenant cache, and the reads `tenant` and `list`.
#![cfg(feature = "lifecycle")]

mod common;

use std::error::Error as StdError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::Cluster;
use common::product::hooks::{OrdersHooks, OrdersSeed, SEED, handover_of};
use common::product::{LATEST_TENANT_VERSION, OrdersFactory};
use sekvent_config::Secret;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::lifecycle::{ProvisionRequest, Provisioned, Provisioner};
use sekvent_tenancy::{
    ProvisionStep, Registry, SelectorRules, TenantId, TenantRow, TenantState, reasons,
};
use sqlx::{Connection, PgConnection, PgPool};

/// Sessions of a role that sit idle inside a transaction.
const IDLE_IN_TRANSACTION: &str = "SELECT count(*) FROM pg_stat_activity \
     WHERE usename = $1 AND state = 'idle in transaction'";

/// What every test starts from: a registry on a frozen clock with one lifecycle permit, its
/// control pool and the test product's hooks.
struct Fixture {
    registry: Registry,
    control: PgPool,
    hooks: OrdersHooks,
}

async fn fixture(cluster: &Cluster) -> Fixture {
    let (registry, control) = cluster.registry(Arc::new(common::manual_clock())).await;
    Fixture {
        registry,
        control,
        hooks: OrdersHooks::new(),
    }
}

impl Fixture {
    fn provisioner(&self, cluster: &Cluster) -> Provisioner<OrdersHooks> {
        cluster.provisioner(&self.registry, &self.hooks)
    }
}

/// A request for a new tenant labelled `label` on `<label>.example.com`.
fn request(label: &str) -> ProvisionRequest<OrdersSeed> {
    let host = format!("{label}.example.com");
    common::provision_request(label, &[host.as_str()])
}

fn role_of(cluster: &Cluster, id: TenantId) -> String {
    cluster.names().role(id).as_str().to_owned()
}

fn database_of(cluster: &Cluster, id: TenantId) -> String {
    cluster.names().database(id).as_str().to_owned()
}

/// The tenant's role password under the fixture's key.
fn password_of(cluster: &Cluster, id: TenantId) -> Secret {
    cluster
        .secrets()
        .derive(id, &cluster.role_key())
        .expect("the fixture key derives a password")
}

fn exposed(handover: Option<&Secret>) -> Option<String> {
    handover.map(|secret| secret.expose().to_owned())
}

#[track_caller]
fn assert_reason(error: &AppError, code: ErrorCode, reason: &str) {
    assert_eq!(error.reason(), Some(reason), "{:?}", error.code());
    assert_eq!(error.code(), code, "{reason}");
}

async fn step_of(provisioner: &Provisioner<OrdersHooks>, id: TenantId) -> ProvisionStep {
    provisioner
        .tenant(id)
        .await
        .expect("the row exists")
        .provision_step
}

async fn assert_no_row(provisioner: &Provisioner<OrdersHooks>, id: TenantId) {
    let error = provisioner.tenant(id).await.expect_err("no row");
    assert_reason(&error, ErrorCode::NotFound, reasons::TENANT_NOT_FOUND);
}

/// The orders in the tenant's database, read as the tenant's role.
async fn orders_of(cluster: &Cluster, id: TenantId) -> i64 {
    let mut conn = cluster
        .connect_as(
            &role_of(cluster, id),
            &password_of(cluster, id),
            &database_of(cluster, id),
        )
        .await
        .expect("the tenant's role signs in with its derived password");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
        .fetch_one(&mut conn)
        .await
        .expect("the orders were counted");
    conn.close().await.expect("the tenant session closed");
    count
}

/// The control owner's sessions that sit idle inside a transaction.
async fn idle_in_transaction(cluster: &Cluster) -> i64 {
    let mut admin = cluster.admin().await;
    let count: i64 = sqlx::query_scalar(IDLE_IN_TRANSACTION)
        .bind(cluster.control_owner())
        .fetch_one(&mut admin)
        .await
        .expect("pg_stat_activity is readable");
    admin.close().await.expect("the admin session closed");
    count
}

/// A whole provisioning is active, seeded once, and its handover is the seed's.
#[track_caller]
fn assert_provisioned(done: &Provisioned, id: TenantId) {
    assert_eq!(done.row.info.id, id);
    assert_eq!(done.row.info.state, TenantState::Active);
    assert_eq!(done.row.provision_step, ProvisionStep::Active);
    assert_eq!(done.row.schema_version, Some(LATEST_TENANT_VERSION));
    assert_eq!(
        exposed(done.handover.as_ref()),
        Some(handover_of(&done.row.info))
    );
}

fn seed_refused() -> AppError {
    AppError::failed_precondition("the seed refused").with_reason("SEED_REFUSED")
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_tenant_is_provisioned_end_to_end() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let request = request("orders");
        let id = request.tenant;

        let done = provisioner.provision(request).await.expect("provisioned");
        assert_provisioned(&done, id);
        assert_eq!(done.row.info.subdomain, "orders");
        assert_eq!(done.row.info.hosts, ["orders.example.com"]);
        assert_eq!(done.row.info.role_key, cluster.role_key());
        assert_eq!(done.row.db_role, role_of(&cluster, id));
        assert_eq!(done.row.db_name, database_of(&cluster, id));
        assert_eq!(
            provisioner.tenant(id).await.expect("the row exists"),
            done.row
        );
        assert_eq!(fx.hooks.calls(), [SEED]);

        let role = role_of(&cluster, id);
        sekvent_testing::await_until!(cluster.sessions_of(&role).await == 0);
        assert_eq!(orders_of(&cluster, id).await, 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_repeat_returns_the_row_without_seeding_again() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let request = request("orders");
        let id = request.tenant;
        let first = provisioner
            .provision(request.clone())
            .await
            .expect("provisioned");
        assert_provisioned(&first, id);

        let again = provisioner.provision(request).await.expect("repeated");
        assert!(again.handover.is_none(), "the handover is returned once");
        assert_eq!(again.row, first.row);
        assert_eq!(fx.hooks.count(SEED), 1);
        assert_eq!(orders_of(&cluster, id).await, 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn an_invalid_request_is_refused_before_any_row() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let mut request = request("orders");
        let id = request.tenant;
        request.subdomain = "-orders".to_owned();
        request.display_name = String::new();
        request.hosts = vec!["not a host".to_owned()];

        let error = provisioner
            .provision(request)
            .await
            .expect_err("the request is invalid");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.reason(), None);
        let fields: Vec<&str> = error
            .field_violations()
            .iter()
            .map(|violation| violation.field.as_str())
            .collect();
        assert_eq!(fields, ["subdomain", "hosts[0]", "display_name"]);
        assert_no_row(&provisioner, id).await;
        assert_eq!(
            provisioner.list(None, 10).await.expect("listed"),
            [] as [TenantRow; 0]
        );
        assert_eq!(fx.hooks.calls(), [] as [&str; 0]);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_role_step_resumes_after_drift_is_removed() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let request = request("orders");
        let id = request.tenant;
        let role = common::quote(&role_of(&cluster, id));

        // Created by the maintenance role, as the crate creates it, but with CREATEDB.
        let mut maintenance = PgConnection::connect_with(&cluster.maintenance_options())
            .await
            .expect("the maintenance role connected");
        common::execute(
            &mut maintenance,
            &format!("CREATE ROLE {role} LOGIN NOINHERIT CREATEDB"),
        )
        .await;
        maintenance.close().await.expect("closed");

        let error = provisioner
            .provision(request.clone())
            .await
            .expect_err("a widened role is refused");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(step_of(&provisioner, id).await, ProvisionStep::Inserted);

        cluster
            .admin_execute("postgres", &format!("ALTER ROLE {role} NOCREATEDB"))
            .await;
        let done = provisioner.provision(request).await.expect("resumed");
        assert_provisioned(&done, id);
        assert_eq!(orders_of(&cluster, id).await, 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_database_step_resumes_after_a_foreign_database_is_dropped() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let request = request("orders");
        let id = request.tenant;
        let database = database_of(&cluster, id);
        let third = cluster.create_role("third", "").await;
        cluster.create_database_named(&database, &third).await;

        let error = provisioner
            .provision(request.clone())
            .await
            .expect_err("a database of another owner is refused");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(step_of(&provisioner, id).await, ProvisionStep::Role);

        cluster
            .admin_execute(
                "postgres",
                &format!("DROP DATABASE {}", common::quote(&database)),
            )
            .await;
        let done = provisioner.provision(request).await.expect("resumed");
        assert_provisioned(&done, id);
        assert_eq!(orders_of(&cluster, id).await, 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn the_migrated_step_resumes_after_a_conflicting_table_is_dropped() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let request = request("orders");
        let id = request.tenant;
        let database = database_of(&cluster, id);
        let quoted = common::quote(&database);

        // Stop at the role step: the database name is held by a third role.
        let third = cluster.create_role("third", "").await;
        cluster.create_database_named(&database, &third).await;
        provisioner
            .provision(request.clone())
            .await
            .expect_err("a database of another owner is refused");
        assert_eq!(step_of(&provisioner, id).await, ProvisionStep::Role);

        // The tenant's own database, holding an `orders` table of the third role.
        cluster
            .admin_execute("postgres", &format!("DROP DATABASE {quoted}"))
            .await;
        cluster
            .create_database_named(&database, &role_of(&cluster, id))
            .await;
        cluster
            .admin_execute(
                &database,
                &format!(
                    "CREATE TABLE orders (id int); ALTER TABLE orders OWNER TO {}",
                    common::quote(&third)
                ),
            )
            .await;

        let error = provisioner
            .provision(request.clone())
            .await
            .expect_err("the migration fails");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), None);
        assert_eq!(error.message(), "the tenant's database could not be opened");
        assert_eq!(
            StdError::source(&error).map(ToString::to_string).as_deref(),
            Some("opening the tenant for a lifecycle operation failed: migrate (sqlstate 42P07)")
        );
        assert_eq!(step_of(&provisioner, id).await, ProvisionStep::Database);
        let role = role_of(&cluster, id);
        sekvent_testing::await_until!(cluster.sessions_of(&role).await == 0);

        cluster.admin_execute(&database, "DROP TABLE orders").await;
        let done = provisioner.provision(request).await.expect("resumed");
        assert_provisioned(&done, id);
        assert_eq!(orders_of(&cluster, id).await, 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_seeded_step_resumes_and_that_call_returns_the_handover() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let request = request("orders");
        let id = request.tenant;

        fx.hooks.fail(SEED, Some(seed_refused));
        let error = provisioner
            .provision(request.clone())
            .await
            .expect_err("the seed refuses");
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), Some("SEED_REFUSED"));
        assert_eq!(error.message(), "the seed refused");
        let row = provisioner.tenant(id).await.expect("the row exists");
        assert_eq!(row.provision_step, ProvisionStep::Migrated);
        assert_eq!(row.info.state, TenantState::Provisioning);
        assert_eq!(row.schema_version, Some(LATEST_TENANT_VERSION));
        let role = role_of(&cluster, id);
        sekvent_testing::await_until!(cluster.sessions_of(&role).await == 0);

        fx.hooks.fail(SEED, None);
        let done = provisioner.provision(request).await.expect("resumed");
        assert_provisioned(&done, id);
        assert_eq!(fx.hooks.count(SEED), 2);
        assert_eq!(orders_of(&cluster, id).await, 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_new_id_at_the_cap_is_refused_and_a_repeat_is_not() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = cluster
            .provisioner_builder(&fx.registry, &fx.hooks)
            .tenant_cap(1)
            .build()
            .expect("a cap of one");
        let first = request("orders");
        provisioner
            .provision(first.clone())
            .await
            .expect("the first fits");

        let second = request("billing");
        let error = provisioner
            .provision(second.clone())
            .await
            .expect_err("the cap is reached");
        assert_reason(
            &error,
            ErrorCode::ResourceExhausted,
            reasons::TENANT_LIMIT_REACHED,
        );
        assert_no_row(&provisioner, second.tenant).await;

        let again = provisioner.provision(first).await.expect("a repeat");
        assert!(again.handover.is_none());
        assert_eq!(again.row.info.state, TenantState::Active);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_mismatching_repeat_is_refused_and_changes_nothing() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let request = request("orders");
        let id = request.tenant;
        let done = provisioner
            .provision(request.clone())
            .await
            .expect("provisioned");

        let mut renamed = request.clone();
        renamed.display_name = "Orders Ltd".to_owned();
        let mut moved = request.clone();
        moved.hosts = vec!["shop.example.com".to_owned()];
        moved.primary_host = "shop.example.com".to_owned();
        let mut widened = request.clone();
        widened.hosts.push("www.orders.example.com".to_owned());
        for other in [renamed, moved, widened] {
            let error = provisioner
                .provision(other)
                .await
                .expect_err("the request does not match the row");
            assert_reason(&error, ErrorCode::FailedPrecondition, reasons::TENANT_STATE);
            assert_eq!(provisioner.tenant(id).await.expect("the row"), done.row);
        }

        // The same hosts spelled differently are the same request.
        let mut respelled = request;
        respelled.subdomain = "ORDERS".to_owned();
        respelled.hosts = vec![
            "Orders.Example.com.".to_owned(),
            "orders.example.com".to_owned(),
        ];
        respelled.primary_host = "ORDERS.example.com".to_owned();
        let again = provisioner.provision(respelled).await.expect("it matches");
        assert_eq!(again.row, done.row);
        assert_eq!(fx.hooks.count(SEED), 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_host_another_tenant_holds_is_refused_without_a_row() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        provisioner
            .provision(request("orders"))
            .await
            .expect("provisioned");

        let mut taken = request("billing");
        taken.hosts = vec!["orders.example.com".to_owned()];
        taken.primary_host = "orders.example.com".to_owned();
        let id = taken.tenant;
        let error = provisioner
            .provision(taken)
            .await
            .expect_err("the host is taken");
        assert_reason(&error, ErrorCode::FailedPrecondition, reasons::TENANT_STATE);
        assert_no_row(&provisioner, id).await;
        assert_eq!(fx.hooks.count(SEED), 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn suspended_purging_and_purged_tenants_are_not_provisioned() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let request = request("orders");
        let id = request.tenant;
        provisioner
            .provision(request.clone())
            .await
            .expect("provisioned");

        for state in [
            TenantState::Suspended,
            TenantState::Purging,
            TenantState::Purged,
        ] {
            cluster.set_state(&fx.control, id, state).await;
            let error = provisioner
                .provision(request.clone())
                .await
                .expect_err("the state refuses");
            assert_reason(&error, ErrorCode::FailedPrecondition, reasons::TENANT_STATE);
            let row = provisioner.tenant(id).await.expect("the row");
            assert_eq!(row.info.state, state);
        }
        assert_eq!(fx.hooks.count(SEED), 1);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_second_call_waits_for_the_lifecycle_admission_then_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        let first = request("orders");
        let first_id = first.tenant;

        fx.hooks.hold(SEED);
        let running = tokio::spawn({
            let provisioner = provisioner.clone();
            async move { provisioner.provision(first).await }
        });
        fx.hooks.arrived(1).await;
        // The step's control transaction stays open while the seed runs.
        sekvent_testing::await_until!(idle_in_transaction(&cluster).await >= 1);

        let second = request("billing");
        let second_id = second.tenant;
        let started = Instant::now();
        let error = provisioner
            .provision(second)
            .await
            .expect_err("no lifecycle permit");
        assert!(started.elapsed() >= Duration::from_secs(10));
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), None);
        assert_no_row(&provisioner, second_id).await;

        fx.hooks.release();
        let done = running
            .await
            .expect("the first call finished")
            .expect("provisioned");
        assert_provisioned(&done, first_id);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn activation_reaches_this_process_tenant_cache() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let factory = OrdersFactory::new();
        let tenants = cluster.tenants(&fx.registry, SelectorRules::new(), &factory, 4);
        let provisioner = cluster
            .provisioner_builder(&fx.registry, &fx.hooks)
            .tenants(tenants.clone())
            .build()
            .expect("the provisioner settings are valid");
        let request = request("orders");
        let id = request.tenant;

        fx.hooks.hold(SEED);
        let running = tokio::spawn({
            let provisioner = provisioner.clone();
            async move { provisioner.provision(request).await }
        });
        fx.hooks.arrived(1).await;
        let Err(error) = tenants.get(id).await else {
            panic!("a provisioning tenant is not served");
        };
        assert_eq!(error.reason(), Some(reasons::TENANT_PROVISIONING));

        fx.hooks.release();
        running
            .await
            .expect("the call finished")
            .expect("provisioned");
        let lease = tenants
            .get(id)
            .await
            .expect("the cached lookup was forgotten on activation");
        drop(lease);
        tenants.close().await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn tenants_are_read_one_by_one_and_listed_by_id() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let fx = fixture(&cluster).await;
        let provisioner = fx.provisioner(&cluster);
        assert_no_row(&provisioner, common::new_tenant()).await;

        let mut rows = Vec::new();
        for (label, state) in [
            ("orders", TenantState::Provisioning),
            ("billing", TenantState::Active),
            ("inventory", TenantState::Purged),
        ] {
            let host = format!("{label}.example.com");
            rows.push(
                cluster
                    .insert_row(&fx.control, label, &[host.as_str()], state)
                    .await,
            );
        }
        rows.sort_by_key(|row| row.info.id.to_string());
        for row in &rows {
            assert_eq!(
                &provisioner.tenant(row.info.id).await.expect("the row"),
                row
            );
        }

        let ids = |page: &[TenantRow]| page.iter().map(|row| row.info.id).collect::<Vec<_>>();
        let first = provisioner.list(None, 2).await.expect("the first page");
        assert_eq!(ids(&first), ids(&rows[..2]));
        let rest = provisioner
            .list(Some(rows[1].info.id), 2)
            .await
            .expect("the rest");
        assert_eq!(ids(&rest), ids(&rows[2..]));
        let none = provisioner
            .list(Some(rows[2].info.id), 2)
            .await
            .expect("past the end");
        assert_eq!(none, [] as [TenantRow; 0]);
        let zero = provisioner.list(None, 0).await.expect("nothing asked");
        assert_eq!(zero, [] as [TenantRow; 0]);
    }))
    .await;
}
