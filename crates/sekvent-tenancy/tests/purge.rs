//! Purge on a real server: the database and the role dropped, the row kept as a tombstone, the
//! states it refuses, resumed after a failure at each step, a database it refuses or leaves
//! alone, and a row changed by hand refused before any maintenance session.
#![cfg(feature = "lifecycle")]

mod common;

use std::error::Error as StdError;
use std::sync::{Arc, Mutex, PoisonError};

use common::Cluster;
use common::product::OrdersFactory;
use common::product::hooks::OrdersHooks;
use sekvent_context::Clock;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::control::{NewTenant, rows};
use sekvent_tenancy::invalidation::{ChangeKind, Sink};
use sekvent_tenancy::lifecycle::Provisioner;
use sekvent_tenancy::{
    DatabaseName, Registry, SelectorRules, TenantId, TenantRow, TenantState, reasons,
};
use sqlx::PgPool;

async fn set_up(cluster: &Cluster) -> (Registry, PgPool, OrdersHooks) {
    let clock: Arc<dyn Clock> = Arc::new(common::manual_clock());
    let (registry, pool) = cluster.registry(clock).await;
    (registry, pool, OrdersHooks::new())
}

/// A provisioned tenant labelled `label`, suspended.
async fn suspended(provisioner: &Provisioner<OrdersHooks>, label: &str) -> TenantRow {
    let host = format!("{label}.example.com");
    let row = provisioner
        .provision(common::provision_request(label, &[host.as_str()]))
        .await
        .expect("the tenant was provisioned")
        .row;
    provisioner
        .suspend(row.info.id)
        .await
        .expect("the tenant was suspended")
}

async fn state(provisioner: &Provisioner<OrdersHooks>, id: TenantId) -> TenantState {
    provisioner
        .tenant(id)
        .await
        .expect("the tenant's row is readable")
        .info
        .state
}

/// Whether a database of the name exists, read as the bootstrap admin.
async fn database_exists(cluster: &Cluster, name: &str) -> bool {
    let mut admin = cluster.admin().await;
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(name)
        .fetch_one(&mut admin)
        .await
        .expect("pg_database is readable")
}

/// The owner of a database, read as the bootstrap admin.
async fn owner_of(cluster: &Cluster, name: &str) -> String {
    let mut admin = cluster.admin().await;
    sqlx::query_scalar(
        "SELECT pg_catalog.pg_get_userbyid(datdba)::text FROM pg_database WHERE datname = $1",
    )
    .bind(name)
    .fetch_one(&mut admin)
    .await
    .expect("the database exists")
}

/// Whether a role of the name exists, read as the bootstrap admin.
async fn role_exists(cluster: &Cluster, name: &str) -> bool {
    let mut admin = cluster.admin().await;
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
        .bind(name)
        .fetch_one(&mut admin)
        .await
        .expect("pg_roles is readable")
}

/// A sink that records every change it hears.
#[derive(Default)]
struct Recorded(Mutex<Vec<(TenantId, ChangeKind)>>);

impl Recorded {
    fn of(&self, tenant: TenantId) -> Vec<ChangeKind> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(id, _)| *id == tenant)
            .map(|(_, kind)| kind.clone())
            .collect()
    }
}

impl Sink for Recorded {
    fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((tenant, kind.clone()));
    }
}

fn assert_tenant_state(error: &AppError) {
    assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{error:?}");
    assert_eq!(error.reason(), Some(reasons::TENANT_STATE));
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn a_suspended_tenant_is_purged_to_a_tombstone_and_its_database_name_is_free() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, hooks) = set_up(&cluster).await;
        let sink = Arc::new(Recorded::default());
        let factory = OrdersFactory::new();
        let tenants = cluster.tenants(&registry, SelectorRules::new(), &factory, 8);
        let provisioner = cluster
            .provisioner_builder(&registry, &hooks)
            .sink(Arc::clone(&sink))
            .tenants(tenants.clone())
            .build()
            .expect("the provisioner settings are valid");
        let row = suspended(&provisioner, "billing").await;
        let id = row.info.id;
        let calls = hooks.calls();
        let before = sink.of(id).len();

        let purged = provisioner.purge(id).await.expect("purged");
        assert_eq!(purged.info.state, TenantState::Purged);
        assert_eq!(purged.info.hosts, [] as [String; 0]);
        assert_eq!(purged.info.subdomain, "billing", "the label stays");
        assert_eq!(purged.db_name, row.db_name);
        assert_eq!(purged.db_role, row.db_role);
        assert_eq!(provisioner.tenant(id).await.expect("the row stays"), purged);
        assert!(!database_exists(&cluster, &row.db_name).await);
        assert!(!role_exists(&cluster, &row.db_role).await);
        assert_eq!(hooks.calls(), calls, "nothing of the product ran");
        assert_eq!(
            sink.of(id)[before..],
            [ChangeKind::STATE, ChangeKind::STATE],
            "purging, then purged"
        );
        let refused = tenants.get(id).await.expect_err("purged");
        assert_eq!(refused.reason(), Some(reasons::TENANT_NOT_HERE));
        assert_eq!(
            tenants
                .hosts()
                .expect("a keyed cache has a snapshot")
                .tenant_of("billing.example.com"),
            None
        );

        let again = provisioner.purge(id).await.expect("already purged");
        assert_eq!(again, purged);
        assert_eq!(sink.of(id).len(), before + 3, "announced again");

        let reuse = common::new_tenant();
        let tenant = NewTenant {
            id: reuse,
            subdomain: "billing".to_owned(),
            display_name: "billing".to_owned(),
            hosts: vec!["billing.example.com".to_owned()],
            primary_host: "billing.example.com".to_owned(),
            database: DatabaseName::parse(&row.db_name).expect("a stored database name"),
            role: cluster.names().role(reuse),
            role_key: cluster.role_key(),
        };
        let mut tx = pool.begin().await.expect("a control transaction");
        let (_, inserted) = rows::insert(&mut tx, &tenant, common::fixture_time())
            .await
            .expect("the database name and the host are free again");
        assert!(inserted);
        tx.commit().await.expect("committed");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn active_and_provisioning_tenants_are_not_purged() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let active = provisioner
            .provision(common::provision_request(
                "billing",
                &["billing.example.com"],
            ))
            .await
            .expect("provisioned")
            .row;
        let error = provisioner.purge(active.info.id).await.expect_err("active");
        assert_tenant_state(&error);
        assert_eq!(
            state(&provisioner, active.info.id).await,
            TenantState::Active
        );
        assert!(database_exists(&cluster, &active.db_name).await);
        assert!(role_exists(&cluster, &active.db_role).await);

        let provisioning = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com"],
                TenantState::Provisioning,
            )
            .await;
        let error = provisioner
            .purge(provisioning.info.id)
            .await
            .expect_err("provisioning");
        assert_tenant_state(&error);
        assert_eq!(
            state(&provisioner, provisioning.info.id).await,
            TenantState::Provisioning
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_purging_row_without_a_database_or_a_role_finishes() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let row = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com"],
                TenantState::Purging,
            )
            .await;
        let purged = provisioner.purge(row.info.id).await.expect("purged");
        assert_eq!(purged.info.state, TenantState::Purged);
        assert_eq!(purged.info.hosts, [] as [String; 0]);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_database_another_role_owns_is_refused_until_it_is_given_back() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let row = suspended(&provisioner, "billing").await;
        let id = row.info.id;
        let database = common::quote(&row.db_name);
        let third = cluster.create_role("third", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "ALTER DATABASE {database} OWNER TO {}",
                    common::quote(&third)
                ),
            )
            .await;

        for _ in 0..2 {
            let error = provisioner.purge(id).await.expect_err("refused");
            assert_eq!(error.code(), ErrorCode::Internal, "{error:?}");
            assert_eq!(state(&provisioner, id).await, TenantState::Purging);
            assert!(database_exists(&cluster, &row.db_name).await);
            assert_eq!(owner_of(&cluster, &row.db_name).await, third);
            assert!(role_exists(&cluster, &row.db_role).await);
        }

        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "ALTER DATABASE {database} OWNER TO {}",
                    common::quote(&row.db_role)
                ),
            )
            .await;
        let purged = provisioner.purge(id).await.expect("the repeat finishes");
        assert_eq!(purged.info.state, TenantState::Purged);
        assert!(!database_exists(&cluster, &row.db_name).await);
        assert!(!role_exists(&cluster, &row.db_role).await);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_role_that_cannot_be_dropped_stays_purging_until_its_privilege_is_revoked() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let row = suspended(&provisioner, "billing").await;
        let id = row.info.id;
        let third = cluster.create_role("third", "").await;
        let elsewhere = common::quote(&cluster.create_database("elsewhere", &third).await);
        let role = common::quote(&row.db_role);
        cluster
            .admin_execute(
                "postgres",
                &format!("GRANT CONNECT ON DATABASE {elsewhere} TO {role}"),
            )
            .await;

        let error = provisioner.purge(id).await.expect_err("the role stays");
        assert_ne!(error.code(), ErrorCode::Ok);
        assert_eq!(state(&provisioner, id).await, TenantState::Purging);
        assert!(
            !database_exists(&cluster, &row.db_name).await,
            "dropped first"
        );
        assert!(role_exists(&cluster, &row.db_role).await);
        assert_eq!(
            provisioner.tenant(id).await.expect("readable").info.hosts,
            row.info.hosts,
            "the hosts stay until the purge commits"
        );

        cluster
            .admin_execute(
                "postgres",
                &format!("REVOKE CONNECT ON DATABASE {elsewhere} FROM {role}"),
            )
            .await;
        let purged = provisioner.purge(id).await.expect("the repeat finishes");
        assert_eq!(purged.info.state, TenantState::Purged);
        assert!(!role_exists(&cluster, &row.db_role).await);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_reused_database_name_of_another_owner_is_left_alone() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let row = suspended(&provisioner, "billing").await;
        let id = row.info.id;
        let database = common::quote(&row.db_name);
        cluster
            .admin_execute(
                "postgres",
                &format!("DROP DATABASE {database} WITH (FORCE)"),
            )
            .await;
        cluster
            .admin_execute(
                "postgres",
                &format!("DROP ROLE {}", common::quote(&row.db_role)),
            )
            .await;
        let third = cluster.create_role("third", "").await;
        cluster.create_database_named(&row.db_name, &third).await;

        let purged = provisioner.purge(id).await.expect("purged");
        assert_eq!(purged.info.state, TenantState::Purged);
        assert!(database_exists(&cluster, &row.db_name).await);
        assert_eq!(owner_of(&cluster, &row.db_name).await, third);
        assert!(!role_exists(&cluster, &row.db_role).await);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_stored_role_that_is_not_the_derived_one_is_refused_before_any_maintenance_session() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let row = suspended(&provisioner, "billing").await;
        let id = row.info.id;
        let foreign = cluster.unique_name("foreign");
        sqlx::query("UPDATE tenants SET db_role = $1 WHERE id = $2::uuid")
            .bind(&foreign)
            .bind(id.to_string())
            .execute(&pool)
            .await
            .expect("the control owner changed the row");
        // No maintenance session can sign in: the refusal must come from the stored-name check.
        let maintenance = common::quote(cluster.maintenance_role());
        cluster
            .admin_execute("postgres", &format!("ALTER ROLE {maintenance} NOLOGIN"))
            .await;

        let error = provisioner.purge(id).await.expect_err("a changed row");
        assert_eq!(error.code(), ErrorCode::Internal, "{error:?}");
        let source = StdError::source(&error).map(ToString::to_string);
        assert!(
            source
                .as_deref()
                .is_some_and(|text| text.contains("db_role")),
            "{source:?}"
        );
        assert_eq!(state(&provisioner, id).await, TenantState::Purging);
        assert!(database_exists(&cluster, &row.db_name).await);
        assert!(role_exists(&cluster, &row.db_role).await);
        cluster
            .admin_execute("postgres", &format!("ALTER ROLE {maintenance} LOGIN"))
            .await;
    })
    .await;
}
