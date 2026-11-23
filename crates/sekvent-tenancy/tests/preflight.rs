//! The startup preflight on a real server: a clean fixture passes, each finding alone, one shared database, many roles in one call, a maintenance session that switched role.
#![cfg(feature = "serving")]

mod common;

use common::{Cluster, quote};
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::cluster::preflight::{self, Finding, Protected};
use sekvent_tenancy::cluster::privileges::DatabasePrivilege;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};

/// Runs `statement` as the bootstrap admin.
async fn admin_sql(cluster: &Cluster, statement: &str) {
    cluster.admin_execute("postgres", statement).await;
}

/// A session of the control owner on the control database.
async fn control_session(cluster: &Cluster) -> PgConnection {
    cluster
        .connect_as(
            cluster.control_owner(),
            cluster.control_password(),
            cluster.control_database(),
        )
        .await
        .expect("the control owner connected")
}

/// The control database as the product protects it, allowing nobody but
/// its owner.
fn control(cluster: &Cluster) -> Protected {
    Protected {
        label: "control".to_owned(),
        database: cluster.control_database().to_owned(),
        allowed: Vec::new(),
    }
}

/// The findings on `protected`, with the maintenance role known or not.
async fn findings_of(
    cluster: &Cluster,
    protected: &Protected,
    maintenance: Option<&str>,
) -> Vec<Finding> {
    let mut conn = control_session(cluster).await;
    let facts = preflight::facts(
        &mut conn,
        &protected.database,
        &protected.allowed,
        maintenance,
        cluster.names(),
    )
    .await
    .unwrap()
    .expect("the database exists");
    conn.close().await.unwrap();
    preflight::findings(&facts, &protected.allowed, cluster.names())
}

/// `check` on `protected` with the maintenance role known.
async fn check(cluster: &Cluster, protected: &Protected) -> Result<(), AppError> {
    let mut conn = control_session(cluster).await;
    let outcome = preflight::check(
        &mut conn,
        protected,
        Some(cluster.maintenance_role()),
        cluster.names(),
    )
    .await;
    conn.close().await.unwrap();
    outcome
}

/// Asserts the error contract of a finding: `FAILED_PRECONDITION`, no
/// reason, the label, the database and `role` named, no credential.
fn assert_refused(cluster: &Cluster, error: &AppError, label: &str, database: &str, role: &str) {
    assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{error:?}");
    assert_eq!(error.reason(), None);
    let message = error.message();
    assert!(
        message.contains(&format!("{label} database {database}")),
        "{message}"
    );
    assert!(message.contains(role), "{message}");
    let printed = format!("{error:?}");
    for secret in [cluster.maintenance_password(), cluster.control_password()] {
        assert!(
            !printed.contains(secret.expose()),
            "no credential is printed"
        );
    }
}

async fn wait_for_no_sessions(cluster: &Cluster, database: &str) {
    sekvent_testing::await_until!(cluster.sessions(database).await == 0);
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_fresh_fixture_passes() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let pool = cluster.control_pool().await;
        preflight::check_control_and_maintenance(
            &pool,
            &cluster.maintenance_options(),
            cluster.names(),
        )
        .await
        .expect("a fresh fixture passes");
        // The preflight closed the maintenance connection it opened.
        wait_for_no_sessions(&cluster, cluster.maintenance_database()).await;

        preflight::check_session_database(
            &pool,
            "control",
            Some(cluster.maintenance_role()),
            cluster.names(),
        )
        .await
        .expect("the control pool's own database passes");
        check(&cluster, &control(&cluster)).await.unwrap();

        let mut maintenance = cluster
            .connect_as(
                cluster.maintenance_role(),
                cluster.maintenance_password(),
                cluster.maintenance_database(),
            )
            .await
            .unwrap();
        let protected = Protected {
            label: "maintenance".to_owned(),
            database: cluster.maintenance_database().to_owned(),
            allowed: vec![cluster.maintenance_role().to_owned()],
        };
        preflight::check(
            &mut maintenance,
            &protected,
            Some(cluster.maintenance_role()),
            cluster.names(),
        )
        .await
        .unwrap();
        maintenance.close().await.unwrap();
        pool.close().await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn public_temporary_alone_is_a_finding() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        admin_sql(
            &cluster,
            &format!(
                "GRANT TEMPORARY ON DATABASE {} TO PUBLIC",
                quote(cluster.control_database())
            ),
        )
        .await;
        let protected = control(&cluster);
        assert_eq!(
            findings_of(&cluster, &protected, Some(cluster.maintenance_role())).await,
            [Finding::Public(DatabasePrivilege::Temporary)]
        );
        let error = check(&cluster, &protected).await.unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control",
            cluster.control_database(),
            "PUBLIC holds TEMPORARY",
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn public_connect_is_a_finding() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO PUBLIC",
                quote(cluster.control_database())
            ),
        )
        .await;
        let found = findings_of(
            &cluster,
            &control(&cluster),
            Some(cluster.maintenance_role()),
        )
        .await;
        // With PUBLIC's CONNECT every LOGIN role of the shared server may
        // connect, so those findings follow the PUBLIC one.
        assert_eq!(found[0], Finding::Public(DatabasePrivilege::Connect));
        assert!(
            found[1..]
                .iter()
                .all(|finding| matches!(finding, Finding::RoleMayConnect(_))),
            "{found:?}"
        );
        assert!(found.contains(&Finding::RoleMayConnect(
            cluster.maintenance_role().to_owned()
        )));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_tenant_named_owner_is_a_finding() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let owner = cluster.prefixed_name("owner");
        cluster.create_role_named(&owner, "").await;
        let database = cluster.create_database("reports", &owner).await;
        admin_sql(
            &cluster,
            &format!(
                "REVOKE CONNECT, TEMPORARY ON DATABASE {} FROM PUBLIC",
                quote(&database)
            ),
        )
        .await;
        let protected = Protected {
            label: "reports".to_owned(),
            database: database.clone(),
            allowed: Vec::new(),
        };
        assert_eq!(
            findings_of(&cluster, &protected, Some(cluster.maintenance_role())).await,
            [Finding::TenantOwner(owner.clone())]
        );
        let error = check(&cluster, &protected).await.unwrap_err();
        assert_refused(&cluster, &error, "reports", &database, &owner);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_tenant_named_role_that_may_connect_is_a_finding() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let role = cluster.prefixed_name("guest");
        cluster.create_role_named(&role, "").await;
        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(cluster.control_database()),
                quote(&role)
            ),
        )
        .await;
        let protected = control(&cluster);
        // A tenant role is one whether or not the maintenance role is known.
        assert_eq!(
            findings_of(&cluster, &protected, None).await,
            [Finding::TenantMayConnect(role.clone())]
        );
        let error = check(&cluster, &protected).await.unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control",
            cluster.control_database(),
            &role,
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_role_the_maintenance_role_reaches_is_a_tenant_role() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        // maintenance -> held -> deeper: deeper is reached through a chain.
        // The edges do not inherit, so only deeper may connect: reach follows
        // every edge whatever its options, and the finding stands alone.
        let held = cluster.create_role("held", "").await;
        let deeper = cluster.create_role("deeper", "").await;
        for statement in [
            format!(
                "GRANT {} TO {} WITH INHERIT FALSE",
                quote(&held),
                quote(cluster.maintenance_role())
            ),
            format!(
                "GRANT {} TO {} WITH INHERIT FALSE",
                quote(&deeper),
                quote(&held)
            ),
            format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(cluster.control_database()),
                quote(&deeper)
            ),
        ] {
            admin_sql(&cluster, &statement).await;
        }
        let protected = control(&cluster);
        assert_eq!(
            findings_of(&cluster, &protected, Some(cluster.maintenance_role())).await,
            [Finding::TenantMayConnect(deeper.clone())]
        );
        // Unknown maintenance role: a NOLOGIN role that is not tenant-named
        // is no finding.
        assert_eq!(
            findings_of(&cluster, &protected, None).await,
            [] as [Finding; 0]
        );
        let error = check(&cluster, &protected).await.unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control",
            cluster.control_database(),
            &deeper,
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn another_login_role_is_a_finding_unless_allowed() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (reader, _) = cluster.create_login_role("reader").await;
        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(cluster.control_database()),
                quote(&reader)
            ),
        )
        .await;
        let protected = control(&cluster);
        assert_eq!(
            findings_of(&cluster, &protected, Some(cluster.maintenance_role())).await,
            [Finding::RoleMayConnect(reader.clone())]
        );
        let error = check(&cluster, &protected).await.unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control",
            cluster.control_database(),
            &reader,
        );

        let allowed = Protected {
            allowed: vec![reader.clone()],
            ..control(&cluster)
        };
        check(&cluster, &allowed)
            .await
            .expect("an allowed role passes");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_superuser_passes_unless_it_is_tenant_named() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        cluster.create_role("operator", "LOGIN SUPERUSER").await;
        let protected = control(&cluster);
        check(&cluster, &protected)
            .await
            .expect("a superuser is the cluster operator's");

        let root = cluster.prefixed_name("root");
        cluster.create_role_named(&root, "LOGIN SUPERUSER").await;
        assert_eq!(
            findings_of(&cluster, &protected, Some(cluster.maintenance_role())).await,
            [Finding::TenantMayConnect(root.clone())]
        );
        let error = check(&cluster, &protected).await.unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control",
            cluster.control_database(),
            &root,
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_missing_database_is_a_finding() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let absent = cluster.unique_name("absent");
        let mut conn = control_session(&cluster).await;
        let facts = preflight::facts(
            &mut conn,
            &absent,
            &[],
            Some(cluster.maintenance_role()),
            cluster.names(),
        )
        .await
        .unwrap();
        assert_eq!(facts, None);
        conn.close().await.unwrap();

        let protected = Protected {
            label: "archive".to_owned(),
            database: absent.clone(),
            allowed: Vec::new(),
        };
        let error = check(&cluster, &protected).await.unwrap_err();
        assert_refused(&cluster, &error, "archive", &absent, "does not exist");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_maintenance_database_is_checked_on_its_own_connection() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (visitor, _) = cluster.create_login_role("visitor").await;
        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(cluster.maintenance_database()),
                quote(&visitor)
            ),
        )
        .await;
        let pool = cluster.control_pool().await;
        let error = preflight::check_control_and_maintenance(
            &pool,
            &cluster.maintenance_options(),
            cluster.names(),
        )
        .await
        .unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "maintenance",
            cluster.maintenance_database(),
            &visitor,
        );
        // Closed after a failed check too.
        wait_for_no_sessions(&cluster, cluster.maintenance_database()).await;

        // The control pool's own check does not see the maintenance database.
        preflight::check_session_database(
            &pool,
            "control",
            Some(cluster.maintenance_role()),
            cluster.names(),
        )
        .await
        .unwrap();

        // Only a session in the maintenance database reads its own copy of
        // the membership catalog's privileges: with it closed to PUBLIC
        // there, the maintenance check fails as a database error, which a
        // check run on the control connection could not.
        cluster
            .admin_execute(
                cluster.maintenance_database(),
                "REVOKE SELECT ON pg_catalog.pg_auth_members FROM PUBLIC",
            )
            .await;
        let error = preflight::check_control_and_maintenance(
            &pool,
            &cluster.maintenance_options(),
            cluster.names(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::PermissionDenied, "{error:?}");
        assert_eq!(
            error.reason(),
            Some(sekvent_db::reasons::DB_PERMISSION_DENIED)
        );
        wait_for_no_sessions(&cluster, cluster.maintenance_database()).await;
        cluster
            .admin_execute(
                cluster.maintenance_database(),
                "GRANT SELECT ON pg_catalog.pg_auth_members TO PUBLIC",
            )
            .await;
        pool.close().await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_session_role_of_a_pool_that_is_not_the_owner_is_allowed() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (service, password) = cluster.create_login_role("service").await;
        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(cluster.control_database()),
                quote(&service)
            ),
        )
        .await;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(cluster.options_for(&service, &password, cluster.control_database()))
            .await
            .expect("the service role connected");

        // Without the allowance the service role is a finding.
        let error = check(&cluster, &control(&cluster)).await.unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control",
            cluster.control_database(),
            &format!("role {service} may connect"),
        );

        // The pool's session role is the one allowed role.
        preflight::check_session_database(
            &pool,
            "control",
            Some(cluster.maintenance_role()),
            cluster.names(),
        )
        .await
        .expect("the session role is allowed");
        preflight::check_control_and_maintenance(
            &pool,
            &cluster.maintenance_options(),
            cluster.names(),
        )
        .await
        .expect("the control pool's session role is allowed");
        wait_for_no_sessions(&cluster, cluster.maintenance_database()).await;
        pool.close().await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_shared_database_is_checked_once_and_allows_both_roles() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(cluster.control_database()),
                quote(cluster.maintenance_role())
            ),
        )
        .await;
        let shared = cluster.options_for(
            cluster.maintenance_role(),
            cluster.maintenance_password(),
            cluster.control_database(),
        );
        let pool = cluster.control_pool().await;
        preflight::check_control_and_maintenance(&pool, &shared, cluster.names())
            .await
            .expect("one database allows the control and the maintenance role");

        // Checked as the control database alone, the maintenance role is a
        // finding: the shared check is what allowed it.
        let error = preflight::check_session_database(
            &pool,
            "control",
            Some(cluster.maintenance_role()),
            cluster.names(),
        )
        .await
        .unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control",
            cluster.control_database(),
            cluster.maintenance_role(),
        );

        let (reader, _) = cluster.create_login_role("reader").await;
        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(cluster.control_database()),
                quote(&reader)
            ),
        )
        .await;
        let error = preflight::check_control_and_maintenance(&pool, &shared, cluster.names())
            .await
            .unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control and maintenance",
            cluster.control_database(),
            &reader,
        );
        assert!(
            !error
                .message()
                .contains(&format!("role {} may connect", cluster.maintenance_role()))
        );
        pool.close().await;
    })
    .await;
}

/// Three hundred tenant-named roles are all reported, sorted, by one
/// `facts` call and one `check` call. The number of statements per
/// database is fixed by the queries themselves and is not measured here.
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn three_hundred_tenant_roles_are_checked_by_one_call() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let prefix = cluster.prefix();
        let control_database = cluster.control_database();
        // The roles are made by one server-side loop.
        admin_sql(
            &cluster,
            &format!(
                "DO $$ BEGIN \
                 FOR i IN 1..300 LOOP \
                 EXECUTE format('CREATE ROLE %I', '{prefix}bulk_' || i); \
                 EXECUTE format('GRANT CONNECT ON DATABASE %I TO %I', '{control_database}', '{prefix}bulk_' || i); \
                 END LOOP; \
                 END $$"
            ),
        )
        .await;
        let mut conn = control_session(&cluster).await;
        let facts = preflight::facts(
            &mut conn,
            control_database,
            &[],
            Some(cluster.maintenance_role()),
            cluster.names(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(facts.tenants_connecting.len(), 300);
        let mut sorted = facts.tenants_connecting.clone();
        sorted.sort();
        assert_eq!(facts.tenants_connecting, sorted);
        let found = preflight::findings(&facts, &[], cluster.names());
        assert_eq!(found.len(), 300);
        assert!(
            found
                .iter()
                .all(|finding| matches!(finding, Finding::TenantMayConnect(_)))
        );

        let error = preflight::check(
            &mut conn,
            &control(&cluster),
            Some(cluster.maintenance_role()),
            cluster.names(),
        )
        .await
        .unwrap_err();
        conn.close().await.unwrap();
        assert_refused(
            &cluster,
            &error,
            "control",
            control_database,
            &format!("tenant role {prefix}bulk_1 may connect"),
        );
        assert!(error.message().contains(&format!("{prefix}bulk_300")));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_refused_maintenance_sign_in_prints_no_credential() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let wrong = common::new_password();
        let options = cluster.options_for(
            cluster.maintenance_role(),
            &wrong,
            cluster.maintenance_database(),
        );
        let pool = cluster.control_pool().await;
        let error = preflight::check_control_and_maintenance(&pool, &options, cluster.names())
            .await
            .unwrap_err();
        // A refused sign-in is a database error, not a finding.
        assert_eq!(error.code(), ErrorCode::Internal, "{error:?}");
        assert_eq!(
            error.reason(),
            Some(sekvent_db::reasons::DB_CREDENTIALS_REJECTED)
        );
        let printed = format!("{error:?} {error}");
        assert!(!printed.contains(wrong.expose()));
        assert!(!printed.contains(cluster.maintenance_password().expose()));
        pool.close().await;
    })
    .await;
}

/// A maintenance session whose role setting switches it to another role at
/// startup acts as that role: the preflight refuses it, as the role's
/// setting and as the database's, on the maintenance database and on a
/// shared one.
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_maintenance_session_that_switched_role_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let maintenance = quote(cluster.maintenance_role());
        let maintenance_db = quote(cluster.maintenance_database());
        let stand_in = cluster.create_role("stand_in", "").await;
        let switched = format!(
            "the maintenance session runs as role {stand_in}, not as its session role {}",
            cluster.maintenance_role()
        );
        admin_sql(
            &cluster,
            &format!("GRANT {} TO {maintenance}", quote(&stand_in)),
        )
        .await;
        admin_sql(
            &cluster,
            &format!("ALTER ROLE {maintenance} SET role = {}", quote(&stand_in)),
        )
        .await;
        let pool = cluster.control_pool().await;
        let error = preflight::check_control_and_maintenance(
            &pool,
            &cluster.maintenance_options(),
            cluster.names(),
        )
        .await
        .unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "maintenance",
            cluster.maintenance_database(),
            &switched,
        );
        wait_for_no_sessions(&cluster, cluster.maintenance_database()).await;

        // On a database shared with the control pool, the one check reports it.
        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO {maintenance}",
                quote(cluster.control_database())
            ),
        )
        .await;
        let shared = cluster.options_for(
            cluster.maintenance_role(),
            cluster.maintenance_password(),
            cluster.control_database(),
        );
        let error = preflight::check_control_and_maintenance(&pool, &shared, cluster.names())
            .await
            .unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "control and maintenance",
            cluster.control_database(),
            &switched,
        );
        admin_sql(
            &cluster,
            &format!(
                "REVOKE CONNECT ON DATABASE {} FROM {maintenance}",
                quote(cluster.control_database())
            ),
        )
        .await;

        // The database's setting switches it just the same.
        admin_sql(&cluster, &format!("ALTER ROLE {maintenance} RESET role")).await;
        admin_sql(
            &cluster,
            &format!(
                "ALTER DATABASE {maintenance_db} SET role = {}",
                quote(&stand_in)
            ),
        )
        .await;
        let error = preflight::check_control_and_maintenance(
            &pool,
            &cluster.maintenance_options(),
            cluster.names(),
        )
        .await
        .unwrap_err();
        assert_refused(
            &cluster,
            &error,
            "maintenance",
            cluster.maintenance_database(),
            &switched,
        );
        wait_for_no_sessions(&cluster, cluster.maintenance_database()).await;

        admin_sql(
            &cluster,
            &format!("ALTER DATABASE {maintenance_db} RESET role"),
        )
        .await;
        preflight::check_control_and_maintenance(
            &pool,
            &cluster.maintenance_options(),
            cluster.names(),
        )
        .await
        .expect("without the setting the session acts as itself");
        pool.close().await;
    })
    .await;
}

/// A maintenance role that holds `REPLICATION` or `BYPASSRLS` is refused
/// with the maintenance database's findings, each attribute named; without
/// them the same cluster passes.
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_maintenance_role_with_a_widening_attribute_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let maintenance = quote(cluster.maintenance_role());
        let pool = cluster.control_pool().await;
        for attribute in ["REPLICATION", "BYPASSRLS"] {
            admin_sql(&cluster, &format!("ALTER ROLE {maintenance} {attribute}")).await;
            let error = preflight::check_control_and_maintenance(
                &pool,
                &cluster.maintenance_options(),
                cluster.names(),
            )
            .await
            .unwrap_err();
            assert_refused(
                &cluster,
                &error,
                "maintenance",
                cluster.maintenance_database(),
                &format!("the maintenance role holds {attribute}"),
            );
            wait_for_no_sessions(&cluster, cluster.maintenance_database()).await;
            admin_sql(&cluster, &format!("ALTER ROLE {maintenance} NO{attribute}")).await;
        }
        preflight::check_control_and_maintenance(
            &pool,
            &cluster.maintenance_options(),
            cluster.names(),
        )
        .await
        .expect("without the attributes the maintenance role passes");
        pool.close().await;
    })
    .await;
}
