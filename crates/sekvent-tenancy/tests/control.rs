//! The control schema, the control rows and the registry on a real server.
#![cfg(feature = "serving")]

mod common;

use std::error::Error as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::Cluster;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::control::{self, MAIN_CLUSTER, NewTenant, VersionStored, rows};
use sekvent_tenancy::{
    ProvisionStep, Registry, TenantId, TenantInfo, TenantRow, TenantState, reasons,
};
use sqlx::{Connection, PgPool};

/// Sessions of the control owner waiting on a lock.
const LOCK_WAITERS: &str =
    "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND wait_event_type = 'Lock'";

fn strings(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_owned()).collect()
}

fn source_of(error: &AppError) -> String {
    error.source().map(ToString::to_string).unwrap_or_default()
}

/// The reference schema applied and a pool of 4 on it.
async fn control_pool(cluster: &Cluster) -> PgPool {
    cluster.apply_control_schema().await;
    cluster.control_pool_of(4).await
}

/// Changes the control schema or its rows as the bootstrap admin.
async fn drift(cluster: &Cluster, sql: &str) {
    cluster.admin_execute(cluster.control_database(), sql).await;
}

/// A tenant labelled `label` with the fixture's names for `id`; `hosts[0]` is the primary.
fn tenant(cluster: &Cluster, id: TenantId, label: &str, hosts: &[&str]) -> NewTenant {
    NewTenant {
        id,
        subdomain: label.to_owned(),
        display_name: format!("{label} team"),
        hosts: strings(hosts),
        primary_host: hosts[0].to_owned(),
        database: cluster.names().database(id),
        role: cluster.names().role(id),
        role_key: cluster.role_key(),
    }
}

/// `rows::insert` in a transaction of its own: committed on success, rolled back on an error.
async fn insert(
    pool: &PgPool,
    tenant: &NewTenant,
    now: SystemTime,
) -> Result<(TenantRow, bool), AppError> {
    let mut tx = pool.begin().await.expect("a control transaction began");
    match rows::insert(&mut tx, tenant, now).await {
        Ok(done) => {
            tx.commit().await.expect("the insert was committed");
            Ok(done)
        }
        Err(error) => {
            tx.rollback().await.expect("the insert was rolled back");
            Err(error)
        }
    }
}

/// `rows::set_hosts` in a transaction of its own, like [`insert`].
async fn set_hosts(
    pool: &PgPool,
    id: TenantId,
    label: &str,
    hosts: &[&str],
    now: SystemTime,
) -> Result<(), AppError> {
    let primary = hosts.first().copied().unwrap_or_default();
    let mut tx = pool.begin().await.expect("a control transaction began");
    match rows::set_hosts(&mut tx, id, label, &strings(hosts), primary, now).await {
        Ok(()) => {
            tx.commit().await.expect("the hosts were committed");
            Ok(())
        }
        Err(error) => {
            tx.rollback().await.expect("the change was rolled back");
            Err(error)
        }
    }
}

async fn get(pool: &PgPool, id: TenantId) -> Result<Option<TenantRow>, AppError> {
    let mut conn = pool.acquire().await.expect("a control connection");
    rows::get(&mut conn, id).await
}

async fn by_host(pool: &PgPool, host: &str) -> Option<TenantRow> {
    let mut conn = pool.acquire().await.expect("a control connection");
    rows::by_host(&mut conn, host)
        .await
        .expect("the host lookup succeeded")
}

async fn by_label(pool: &PgPool, label: &str) -> Option<TenantRow> {
    let mut conn = pool.acquire().await.expect("a control connection");
    rows::by_loopback_label(&mut conn, label)
        .await
        .expect("the label lookup succeeded")
}

async fn store(pool: &PgPool, id: TenantId, version: i64) -> VersionStored {
    let mut conn = pool.acquire().await.expect("a control connection");
    rows::store_schema_version(&mut conn, id, version)
        .await
        .expect("the version statement succeeded")
}

async fn lock_waiters(cluster: &Cluster) -> i64 {
    let mut admin = cluster.admin().await;
    let waiting: i64 = sqlx::query_scalar(LOCK_WAITERS)
        .bind(cluster.control_owner())
        .fetch_one(&mut admin)
        .await
        .expect("pg_stat_activity is readable");
    admin.close().await.expect("the admin session closed");
    waiting
}

/// Resolves once a session of the control owner waits on a lock.
async fn until_a_lock_waits(cluster: &Cluster) {
    sekvent_testing::await_until!(lock_waiters(cluster).await > 0);
}

fn assert_schema_missing(result: Result<(), AppError>, key: &str, name: &str) {
    let error = result.expect_err("the control schema is incomplete");
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.reason(), Some(reasons::CONTROL_SCHEMA_MISSING));
    assert_eq!(
        error.message(),
        "the control schema is missing or incomplete"
    );
    assert_eq!(error.domain(), None);
    assert_eq!(error.metadata().len(), 1, "{:?}", error.metadata());
    assert_eq!(
        error.metadata().get(key).map(String::as_str),
        Some(name),
        "{:?}",
        error.metadata()
    );
}

fn assert_tenant_state(error: &AppError) {
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.reason(), Some(reasons::TENANT_STATE));
    assert_eq!(error.domain(), None);
    assert!(error.source().is_some(), "the database error is the source");
    assert!(!error.message().contains("example"), "{}", error.message());
}

fn assert_internal(result: Result<Option<TenantRow>, AppError>, column: &str) {
    let error = result.expect_err("the stored row is refused");
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), None);
    let source = source_of(&error);
    assert!(source.contains(column), "{source}");
    assert!(!source.contains("example"), "{source}");
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_reference_schema_and_a_products_additions_pass_verification() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        control::verify_schema(&pool)
            .await
            .expect("the reference schema passes");

        drift(
            &cluster,
            "ALTER TABLE tenants ADD COLUMN billing_plan text; \
             ALTER TABLE tenant_hosts ADD COLUMN verified_at timestamptz; \
             CREATE INDEX tenants_subdomain_idx ON tenants (subdomain)",
        )
        .await;
        control::verify_schema(&pool)
            .await
            .expect("extra columns and indexes pass");
        let row = cluster
            .insert_row(
                &pool,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        assert_eq!(row.info.subdomain, "billing");

        drift(
            &cluster,
            &format!(
                "REVOKE SELECT ON tenant_hosts FROM {}",
                common::quote(cluster.control_owner())
            ),
        )
        .await;
        let error = control::verify_schema(&pool)
            .await
            .expect_err("a select the owner may not run fails");
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
        assert_eq!(
            error.reason(),
            Some(sekvent_db::reasons::DB_PERMISSION_DENIED)
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_missing_column_or_table_names_its_table() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;

        drift(&cluster, "ALTER TABLE tenants DROP COLUMN display_name").await;
        let result = control::verify_schema(&pool).await;
        if let Err(error) = &result {
            assert!(error.source().is_some(), "the database error is the source");
        }
        assert_schema_missing(result, "table", "tenants");

        drift(
            &cluster,
            "ALTER TABLE tenants ADD COLUMN display_name text NOT NULL DEFAULT ''",
        )
        .await;
        control::verify_schema(&pool)
            .await
            .expect("the column is back");

        drift(&cluster, "ALTER TABLE tenant_hosts DROP COLUMN is_primary").await;
        assert_schema_missing(control::verify_schema(&pool).await, "table", "tenant_hosts");

        drift(&cluster, "DROP TABLE tenant_hosts").await;
        assert_schema_missing(control::verify_schema(&pool).await, "table", "tenant_hosts");

        drift(&cluster, "DROP TABLE tenants").await;
        assert_schema_missing(control::verify_schema(&pool).await, "table", "tenants");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_missing_constraint_or_index_is_named_in_the_checked_order() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;

        drift(&cluster, "DROP INDEX tenants_db_name_key").await;
        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "tenants_db_name_key",
        );

        drift(
            &cluster,
            "ALTER TABLE tenants DROP CONSTRAINT tenants_db_role_key",
        )
        .await;
        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "tenants_db_role_key",
        );

        drift(
            &cluster,
            "ALTER TABLE tenants ADD CONSTRAINT tenants_db_role_key UNIQUE (db_cluster, db_role); \
             CREATE UNIQUE INDEX tenants_db_name_key ON tenants (db_cluster, db_name) \
             WHERE state <> 'purged'",
        )
        .await;
        control::verify_schema(&pool)
            .await
            .expect("the keys are back");

        drift(
            &cluster,
            "ALTER TABLE tenant_hosts DROP CONSTRAINT tenant_hosts_pkey",
        )
        .await;
        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "tenant_hosts_pkey",
        );

        drift(
            &cluster,
            "CREATE INDEX tenant_hosts_pkey ON tenant_hosts (host)",
        )
        .await;
        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "tenant_hosts_pkey",
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_unique_index_with_the_wrong_predicate_is_named() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;

        // A partial index re-created without its `WHERE`.
        drift(
            &cluster,
            "DROP INDEX tenant_hosts_primary_key; \
             CREATE UNIQUE INDEX tenant_hosts_primary_key ON tenant_hosts (tenant_id)",
        )
        .await;
        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "tenant_hosts_primary_key",
        );
        drift(
            &cluster,
            "DROP INDEX tenant_hosts_primary_key; \
             CREATE UNIQUE INDEX tenant_hosts_primary_key ON tenant_hosts (tenant_id) \
             WHERE is_primary",
        )
        .await;
        control::verify_schema(&pool)
            .await
            .expect("the partial index is back");

        // A partial index re-created as a total one.
        drift(
            &cluster,
            "DROP INDEX tenants_db_name_key; \
             CREATE UNIQUE INDEX tenants_db_name_key ON tenants (db_cluster, db_name)",
        )
        .await;
        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "tenants_db_name_key",
        );
        drift(
            &cluster,
            "DROP INDEX tenants_db_name_key; \
             CREATE UNIQUE INDEX tenants_db_name_key ON tenants (db_cluster, db_name) \
             WHERE state <> 'purged'",
        )
        .await;
        control::verify_schema(&pool)
            .await
            .expect("the partial index is back");

        // A key that must cover every row, made partial.
        drift(
            &cluster,
            "ALTER TABLE tenants DROP CONSTRAINT tenants_db_role_key; \
             CREATE UNIQUE INDEX tenants_db_role_key ON tenants (db_cluster, db_role) \
             WHERE state <> 'purged'",
        )
        .await;
        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "tenants_db_role_key",
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_invalid_unique_index_left_by_a_failed_concurrent_build_is_named() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let row = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com"],
                TenantState::Active,
            )
            .await;
        // Two primary hosts of one tenant: the unique index cannot be built.
        drift(
            &cluster,
            &format!(
                "DROP INDEX tenant_hosts_primary_key; \
                 INSERT INTO tenant_hosts (host, tenant_id, is_primary) \
                 VALUES ('shop.example.com', '{}', true)",
                row.info.id
            ),
        )
        .await;

        // `CONCURRENTLY` runs outside a transaction block, so alone on its session. Its failure
        // leaves the index behind, marked invalid.
        let mut admin = cluster.admin_on(cluster.control_database()).await;
        let failed = common::try_execute(
            &mut admin,
            "CREATE UNIQUE INDEX CONCURRENTLY tenant_hosts_primary_key \
             ON tenant_hosts (tenant_id) WHERE is_primary",
        )
        .await;
        assert!(
            failed.is_err(),
            "the duplicate primary hosts fail the build"
        );
        let valid: Option<bool> = sqlx::query_scalar(
            "SELECT indisvalid FROM pg_index \
             WHERE indexrelid = to_regclass('tenant_hosts_primary_key')",
        )
        .fetch_optional(&mut admin)
        .await
        .expect("pg_index is readable");
        admin.close().await.expect("the admin session closed");
        assert_eq!(valid, Some(false), "the failed build left an invalid index");

        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "tenant_hosts_primary_key",
        );
    })
    .await;
}

#[cfg(feature = "admin")]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn with_feature_admin_the_audit_table_and_its_key_are_checked() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;

        drift(&cluster, "ALTER TABLE admin_audit DROP COLUMN outcome").await;
        assert_schema_missing(control::verify_schema(&pool).await, "table", "admin_audit");

        drift(
            &cluster,
            "ALTER TABLE admin_audit ADD COLUMN outcome text NOT NULL DEFAULT 'started'",
        )
        .await;
        control::verify_schema(&pool)
            .await
            .expect("the column is back");

        drift(
            &cluster,
            "ALTER TABLE admin_audit DROP CONSTRAINT admin_audit_pkey",
        )
        .await;
        assert_schema_missing(
            control::verify_schema(&pool).await,
            "constraint",
            "admin_audit_pkey",
        );

        drift(&cluster, "DROP TABLE admin_audit").await;
        assert_schema_missing(control::verify_schema(&pool).await, "table", "admin_audit");
    })
    .await;
}

#[cfg(not(feature = "admin"))]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn without_feature_admin_the_audit_table_may_be_absent() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        drift(&cluster, "DROP TABLE admin_audit").await;
        control::verify_schema(&pool)
            .await
            .expect("a control database without the audit table passes");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_inserted_row_reads_back_by_id_and_by_each_of_its_hosts() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let id = common::new_tenant();
        let now = common::fixture_time() + Duration::from_micros(250);
        let new = NewTenant {
            primary_host: "orders.example.com".to_owned(),
            ..tenant(
                &cluster,
                id,
                "orders",
                &[
                    "orders.example.com",
                    "api.orders.example.com",
                    "orders.example.com",
                ],
            )
        };

        let (row, inserted) = insert(&pool, &new, now).await.expect("the row is inserted");
        assert!(inserted);
        assert_eq!(row.info.id, id);
        assert_eq!(row.info.subdomain, "orders");
        assert_eq!(row.info.display_name, "orders team");
        assert_eq!(row.info.state, TenantState::Provisioning);
        assert_eq!(row.info.primary_host, "orders.example.com");
        assert_eq!(
            row.info.hosts,
            ["api.orders.example.com", "orders.example.com"]
        );
        assert_eq!(row.info.role_key, cluster.role_key());
        assert_eq!(row.provision_step, ProvisionStep::Inserted);
        assert_eq!(row.db_cluster, MAIN_CLUSTER);
        assert_eq!(row.db_name, cluster.names().database(id).as_str());
        assert_eq!(row.db_role, cluster.names().role(id).as_str());
        assert_eq!(row.schema_version, None);
        assert_eq!(row.created_at, now);
        assert_eq!(row.updated_at, now);

        assert_eq!(get(&pool, id).await.expect("readable").as_ref(), Some(&row));
        assert_eq!(
            by_host(&pool, "orders.example.com").await.as_ref(),
            Some(&row)
        );
        assert_eq!(
            by_host(&pool, "api.orders.example.com").await.as_ref(),
            Some(&row)
        );
        assert_eq!(
            get(&pool, common::new_tenant()).await.expect("readable"),
            None
        );
        assert_eq!(by_host(&pool, "billing.example.com").await, None);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_repeated_insert_changes_nothing_and_returns_the_first_row() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let id = common::new_tenant();
        let first = tenant(&cluster, id, "orders", &["orders.example.com"]);
        let (row, inserted) = insert(&pool, &first, common::fixture_time())
            .await
            .expect("the row is inserted");
        assert!(inserted);

        let again = tenant(&cluster, id, "shop", &["shop.example.com"]);
        let later = common::fixture_time() + Duration::from_secs(60);
        let (existing, inserted) = insert(&pool, &again, later)
            .await
            .expect("a repeated id is not an error");
        assert!(!inserted);
        assert_eq!(existing, row);
        assert_eq!(get(&pool, id).await.expect("readable"), Some(row));
        assert_eq!(by_host(&pool, "shop.example.com").await, None);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn invalid_hosts_are_refused_before_any_statement() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let id = common::new_tenant();
        let mut tx = pool.begin().await.expect("a control transaction began");
        for new in [
            tenant(&cluster, id, "orders", &["Orders.example.com"]),
            NewTenant {
                hosts: Vec::new(),
                ..tenant(&cluster, id, "orders", &["orders.example.com"])
            },
            NewTenant {
                primary_host: "billing.example.com".to_owned(),
                ..tenant(&cluster, id, "orders", &["orders.example.com"])
            },
        ] {
            let error = rows::insert(&mut tx, &new, common::fixture_time())
                .await
                .expect_err("the hosts are refused");
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert!(!error.message().contains("example"), "{}", error.message());
        }
        // Nothing ran: the transaction is not aborted and holds no row.
        assert_eq!(
            rows::get(&mut tx, id)
                .await
                .expect("the transaction is usable"),
            None
        );
        tx.rollback().await.expect("rolled back");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn names_another_tenant_holds_are_tenant_state() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let first = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com"],
                TenantState::Active,
            )
            .await;
        let first_id = first.info.id;
        let second = common::new_tenant();
        let now = common::fixture_time();

        let host_held = tenant(
            &cluster,
            second,
            "billing",
            &["billing.example.com", "orders.example.com"],
        );
        let role_held = NewTenant {
            role: cluster.names().role(first_id),
            ..tenant(&cluster, second, "billing", &["billing.example.com"])
        };
        let database_held = NewTenant {
            database: cluster.names().database(first_id),
            ..tenant(&cluster, second, "billing", &["billing.example.com"])
        };
        for new in [&host_held, &role_held, &database_held] {
            let error = insert(&pool, new, now)
                .await
                .expect_err("the name is held by the first tenant");
            assert_tenant_state(&error);
            assert_eq!(get(&pool, second).await.expect("readable"), None);
            assert_eq!(by_host(&pool, "billing.example.com").await, None);
        }
        assert_eq!(get(&pool, first_id).await.expect("readable"), Some(first));

        cluster
            .set_state(&pool, first_id, TenantState::Purged)
            .await;
        let (reused, inserted) = insert(&pool, &database_held, now)
            .await
            .expect("a purged tenant's database name is free again");
        assert!(inserted);
        assert_eq!(reused.db_name, cluster.names().database(first_id).as_str());

        let third = common::new_tenant();
        let role_still_held = NewTenant {
            role: cluster.names().role(first_id),
            ..tenant(&cluster, third, "inventory", &["inventory.example.com"])
        };
        let error = insert(&pool, &role_still_held, now)
            .await
            .expect_err("a role name is never free again");
        assert_tenant_state(&error);
        assert_eq!(get(&pool, third).await.expect("readable"), None);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_violated_constraint_of_the_product_is_not_tenant_state() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        drift(
            &cluster,
            "ALTER TABLE tenants ADD CONSTRAINT tenants_label_check CHECK (subdomain <> 'blocked')",
        )
        .await;
        let id = common::new_tenant();
        let error = insert(
            &pool,
            &tenant(&cluster, id, "blocked", &["blocked.example.com"]),
            common::fixture_time(),
        )
        .await
        .expect_err("the product's check refuses the row");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(get(&pool, id).await.expect("readable"), None);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn set_hosts_replaces_the_host_set_and_the_subdomain() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let row = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com", "old.example.com"],
                TenantState::Active,
            )
            .await;
        let id = row.info.id;
        cluster
            .insert_row(
                &pool,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let later = common::fixture_time() + Duration::from_secs(60);

        set_hosts(
            &pool,
            id,
            "shop",
            &["shop.example.com", "orders.example.com"],
            later,
        )
        .await
        .expect("the hosts are replaced");
        let changed = get(&pool, id)
            .await
            .expect("readable")
            .expect("the row exists");
        assert_eq!(changed.info.subdomain, "shop");
        assert_eq!(changed.info.primary_host, "shop.example.com");
        assert_eq!(
            changed.info.hosts,
            ["orders.example.com", "shop.example.com"]
        );
        assert_eq!(changed.updated_at, later);
        assert_eq!(changed.created_at, row.created_at);
        assert_eq!(by_host(&pool, "old.example.com").await, None);
        assert_eq!(
            by_host(&pool, "shop.example.com").await,
            Some(changed.clone())
        );

        let error = set_hosts(&pool, id, "shop", &["billing.example.com"], later)
            .await
            .expect_err("the host belongs to another tenant");
        assert_tenant_state(&error);
        assert_eq!(
            get(&pool, id).await.expect("readable"),
            Some(changed.clone())
        );

        let error = set_hosts(&pool, id, "shop", &[], later)
            .await
            .expect_err("a tenant keeps at least one host");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);

        let error = set_hosts(
            &pool,
            common::new_tenant(),
            "inventory",
            &["inventory.example.com"],
            later,
        )
        .await
        .expect_err("there is no such tenant");
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.reason(), None);
        assert_eq!(by_host(&pool, "inventory.example.com").await, None);
        assert_eq!(get(&pool, id).await.expect("readable"), Some(changed));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_waiting_lock_sees_the_hosts_the_holder_committed() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let row = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com"],
                TenantState::Active,
            )
            .await;
        let id = row.info.id;

        let mut holder = pool.begin().await.expect("a control transaction began");
        let locked = rows::lock(&mut holder, id)
            .await
            .expect("the lock is taken")
            .expect("the row exists");
        assert_eq!(locked, row);
        rows::set_hosts(
            &mut holder,
            id,
            "orders",
            &strings(&["shop.example.com", "orders.example.com"]),
            "shop.example.com",
            common::fixture_time(),
        )
        .await
        .expect("the holder changes the hosts");

        let mut waiter = pool.begin().await.expect("a control transaction began");
        {
            let waiting = rows::lock(&mut waiter, id);
            tokio::pin!(waiting);
            tokio::select! {
                biased;
                result = &mut waiting => panic!("the second lock did not wait: {result:?}"),
                () = until_a_lock_waits(&cluster) => {}
            }
            holder.commit().await.expect("the holder committed");
            let seen = waiting
                .await
                .expect("the lock is taken")
                .expect("the row exists");
            assert_eq!(seen.info.primary_host, "shop.example.com");
            assert_eq!(seen.info.hosts, ["orders.example.com", "shop.example.com"]);
        }
        waiter.rollback().await.expect("rolled back");

        let mut tx = pool.begin().await.expect("a control transaction began");
        assert_eq!(
            rows::lock(&mut tx, common::new_tenant())
                .await
                .expect("the statement succeeded"),
            None
        );
        tx.rollback().await.expect("rolled back");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn stored_values_are_decoded_or_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let row = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com", "api.orders.example.com"],
                TenantState::Active,
            )
            .await;
        let id = row.info.id;

        drift(
            &cluster,
            &format!("DELETE FROM tenant_hosts WHERE tenant_id = '{id}' AND is_primary"),
        )
        .await;
        assert_internal(get(&pool, id).await, "tenant_hosts.is_primary");

        drift(
            &cluster,
            &format!(
                "DELETE FROM tenant_hosts WHERE tenant_id = '{id}'; \
                 UPDATE tenants SET state = 'purged' WHERE id = '{id}'"
            ),
        )
        .await;
        let purged = get(&pool, id)
            .await
            .expect("a purged row without hosts decodes")
            .expect("the row exists");
        assert_eq!(purged.info.state, TenantState::Purged);
        assert_eq!(purged.info.primary_host, "");
        assert_eq!(purged.info.hosts, [] as [String; 0]);

        drift(
            &cluster,
            &format!("UPDATE tenants SET provision_step = 'migrated' WHERE id = '{id}'"),
        )
        .await;
        let migrated = get(&pool, id)
            .await
            .expect("a known step decodes")
            .expect("the row exists");
        assert_eq!(migrated.provision_step, ProvisionStep::Migrated);

        drift(
            &cluster,
            &format!("UPDATE tenants SET provision_step = 'unpacked' WHERE id = '{id}'"),
        )
        .await;
        assert_internal(get(&pool, id).await, "tenants.provision_step");

        drift(
            &cluster,
            &format!(
                "UPDATE tenants SET provision_step = '', db_cluster = 'replica' WHERE id = '{id}'"
            ),
        )
        .await;
        let elsewhere = get(&pool, id)
            .await
            .expect("another cluster label decodes")
            .expect("the row exists");
        assert_eq!(elsewhere.db_cluster, "replica");
        assert_eq!(elsewhere.provision_step, ProvisionStep::Inserted);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_loopback_label_selects_its_one_live_tenant_with_a_loopback_host() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let orders = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com", "orders.localhost:3000"],
                TenantState::Active,
            )
            .await;
        assert_eq!(by_label(&pool, "orders").await, Some(orders));

        let billing = cluster
            .insert_row(
                &pool,
                "billing",
                &["billing.localhost"],
                TenantState::Suspended,
            )
            .await;
        cluster
            .insert_row(
                &pool,
                "billing",
                &["old-billing.example.com"],
                TenantState::Purged,
            )
            .await;
        assert_eq!(by_label(&pool, "billing").await, Some(billing));

        cluster
            .insert_row(
                &pool,
                "inventory",
                &["inventory.localhost:5173"],
                TenantState::Active,
            )
            .await;
        cluster
            .insert_row(
                &pool,
                "inventory",
                &["inventory.example.com"],
                TenantState::Provisioning,
            )
            .await;
        assert_eq!(by_label(&pool, "inventory").await, None);

        cluster
            .insert_row(
                &pool,
                "reports",
                &["reports.example.com"],
                TenantState::Active,
            )
            .await;
        assert_eq!(by_label(&pool, "reports").await, None);
        assert_eq!(by_label(&pool, "unknown").await, None);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn list_active_returns_the_active_rows_in_id_order_with_their_hosts() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let orders = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com", "api.orders.example.com"],
                TenantState::Active,
            )
            .await;
        let billing = cluster
            .insert_row(
                &pool,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        cluster
            .insert_row(
                &pool,
                "reports",
                &["reports.example.com"],
                TenantState::Suspended,
            )
            .await;
        let inventory = cluster
            .insert_row(
                &pool,
                "inventory",
                &["inventory.example.com"],
                TenantState::Provisioning,
            )
            .await;

        let mut expected = vec![orders.info, billing.info];
        expected.sort_by_key(|info| info.id);
        let mut conn = pool.acquire().await.expect("a control connection");
        let active = rows::list_active(&mut conn)
            .await
            .expect("the list is read");
        assert_eq!(active, expected);
        assert_eq!(active[0].hosts.len() + active[1].hosts.len(), 3);

        let id = inventory.info.id;
        let later = common::fixture_time() + Duration::from_secs(5);
        assert!(
            rows::set_state(&mut conn, id, TenantState::Active, later)
                .await
                .expect("the state is set")
        );
        assert!(
            !rows::set_state(&mut conn, common::new_tenant(), TenantState::Active, later)
                .await
                .expect("the statement succeeded")
        );
        let active = rows::list_active(&mut conn)
            .await
            .expect("the list is read");
        assert_eq!(active.len(), 3);
        assert!(active.windows(2).all(|pair| pair[0].id < pair[1].id));
        let changed = get(&pool, id)
            .await
            .expect("readable")
            .expect("the row exists");
        assert_eq!(changed.info.state, TenantState::Active);
        assert_eq!(changed.updated_at, later);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_schema_version_only_rises_and_never_waits() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = control_pool(&cluster).await;
        let row = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com"],
                TenantState::Active,
            )
            .await;
        let id = row.info.id;
        let version_of = |stored: Option<TenantRow>| stored.expect("the row exists").schema_version;

        assert_eq!(store(&pool, id, 1).await, VersionStored::Stored);
        assert_eq!(version_of(get(&pool, id).await.expect("readable")), Some(1));
        assert_eq!(store(&pool, id, 2).await, VersionStored::Stored);
        assert_eq!(store(&pool, id, 2).await, VersionStored::Unchanged);
        assert_eq!(store(&pool, id, 1).await, VersionStored::Unchanged);
        let stored = get(&pool, id)
            .await
            .expect("readable")
            .expect("the row exists");
        assert_eq!(stored.schema_version, Some(2));
        assert_eq!(stored.updated_at, row.updated_at);

        let mut holder = pool.begin().await.expect("a control transaction began");
        rows::lock(&mut holder, id)
            .await
            .expect("the lock is taken")
            .expect("the row exists");
        assert_eq!(store(&pool, id, 3).await, VersionStored::Busy);
        holder.rollback().await.expect("rolled back");

        assert_eq!(
            store(&pool, common::new_tenant(), 1).await,
            VersionStored::Busy
        );
        assert_eq!(store(&pool, id, 3).await, VersionStored::Stored);
        assert_eq!(version_of(get(&pool, id).await.expect("readable")), Some(3));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_registry_reads_through_its_pool() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool) = cluster.registry(Arc::new(common::manual_clock())).await;
        let row = cluster
            .insert_row(
                &pool,
                "inventory",
                &["inventory.example.com"],
                TenantState::Provisioning,
            )
            .await;
        let id = row.info.id;

        let now = registry
            .tenant_now(id)
            .await
            .expect("readable")
            .expect("the row exists");
        assert_eq!(now, row);
        assert_eq!(
            registry.active().await.expect("the list is read"),
            Vec::<TenantInfo>::new()
        );

        cluster.set_state(&pool, id, TenantState::Active).await;
        let now = registry
            .tenant_now(id)
            .await
            .expect("readable")
            .expect("the row exists");
        assert_eq!(now.info.state, TenantState::Active);
        assert_eq!(
            registry.active().await.expect("the list is read"),
            std::slice::from_ref(&now.info)
        );
        assert_eq!(
            registry
                .tenant_now(common::new_tenant())
                .await
                .expect("readable"),
            None
        );

        assert_eq!(
            registry
                .store_schema_version(id, 2)
                .await
                .expect("the version is stored"),
            VersionStored::Stored
        );
        assert_eq!(
            registry
                .store_schema_version(id, 2)
                .await
                .expect("the statement succeeded"),
            VersionStored::Unchanged
        );
        assert_eq!(
            registry
                .tenant_now(id)
                .await
                .expect("readable")
                .expect("the row exists")
                .schema_version,
            Some(2)
        );
        assert_eq!(registry.pool().options().get_max_connections(), 4);

        let error = Registry::new(
            cluster.control_pool().await,
            Arc::new(common::manual_clock()),
        )
        .expect_err("a pool of two is too small");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
    })
    .await;
}
