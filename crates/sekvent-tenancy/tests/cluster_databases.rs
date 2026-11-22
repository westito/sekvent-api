//! Tenant databases on a real server: the CONNECT restriction, convergence, drift refused, size, the drop decision, poisoning.
#![cfg(feature = "lifecycle")]

mod common;

use std::borrow::Cow;
use std::error::Error as _;

use common::Cluster;
use sekvent_config::Secret;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::cluster::databases::{self, DatabaseAccess};
use sekvent_tenancy::cluster::roles;
use sekvent_tenancy::{DatabaseName, Maintenance, MaintenanceConn, ScramVerifier, TenantRole};
use sqlx::{Connection, PgConnection};

/// A database's owner, template flag, `ALLOW_CONNECTIONS` and ACL.
const DATABASE_STATE: &str = "SELECT concat_ws('|', pg_catalog.pg_get_userbyid(datdba), \
     datistemplate, datallowconn, coalesce(datacl::text, 'default')) \
     FROM pg_catalog.pg_database WHERE datname = $1";

/// Sessions of a role that wait on a lock.
const LOCK_WAITERS: &str =
    "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND wait_event_type = 'Lock'";

/// The crate's maintenance sessions of a role on a database.
const MAINTENANCE_SESSIONS: &str = "SELECT count(*) FROM pg_stat_activity \
     WHERE usename = $1 AND datname = $2 AND application_name = 'sekvent-tenancy-maintenance'";

/// The sessions of a role under an application name.
const NAMED_SESSIONS: &str =
    "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND application_name = $2";

/// The state and the last statement of every maintenance session of a role.
const MAINTENANCE_ACTIVITY: &str = "SELECT coalesce(state, ''), coalesce(query, '') \
     FROM pg_stat_activity \
     WHERE usename = $1 AND application_name = 'sekvent-tenancy-maintenance'";

/// A tenant role and its database name, and the role's password.
struct Tenant {
    role: TenantRole,
    database: DatabaseName,
    password: Secret,
}

fn source(error: &AppError) -> String {
    error.source().map(ToString::to_string).unwrap_or_default()
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    sekvent_db::sqlstate(error).map(Cow::into_owned)
}

fn name(text: &str) -> DatabaseName {
    DatabaseName::parse(text).expect("a valid database name")
}

/// A tenant whose role the crate created; its database is not created yet.
async fn tenant_role(cluster: &Cluster, conn: &mut MaintenanceConn) -> Tenant {
    let tenant = common::new_tenant();
    let password = common::new_password();
    let verifier = ScramVerifier::new(&password).expect("the verifier was computed");
    let role = cluster.names().role(tenant);
    roles::create(conn, &role, &verifier, 5)
        .await
        .expect("the role was created");
    Tenant {
        role,
        database: cluster.names().database(tenant),
        password,
    }
}

/// A tenant with its role and database created by the crate.
async fn tenant(cluster: &Cluster, conn: &mut MaintenanceConn) -> Tenant {
    let tenant = tenant_role(cluster, conn).await;
    databases::create(conn, &tenant.database, &tenant.role)
        .await
        .expect("the database was created");
    tenant
}

async fn admin_count(
    cluster: &Cluster,
    sql: &'static str,
    first: &str,
    second: Option<&str>,
) -> i64 {
    let mut admin = cluster.admin().await;
    let mut query = sqlx::query_scalar(sql).bind(first.to_owned());
    if let Some(second) = second {
        query = query.bind(second.to_owned());
    }
    let count: i64 = query
        .fetch_one(&mut admin)
        .await
        .expect("the count was read");
    admin.close().await.expect("the admin session closed");
    count
}

async fn database_state(cluster: &Cluster, database: &str) -> Option<String> {
    let mut admin = cluster.admin().await;
    let state: Option<String> = sqlx::query_scalar(DATABASE_STATE)
        .bind(database)
        .fetch_optional(&mut admin)
        .await
        .expect("the database state was read");
    admin.close().await.expect("the admin session closed");
    state
}

/// The state and the last statement of the one maintenance session of
/// `role`.
async fn maintenance_activity(cluster: &Cluster, role: &str) -> (String, String) {
    let mut admin = cluster.admin().await;
    let rows: Vec<(String, String)> = sqlx::query_as(MAINTENANCE_ACTIVITY)
        .bind(role)
        .fetch_all(&mut admin)
        .await
        .expect("pg_stat_activity is readable");
    admin.close().await.expect("the admin session closed");
    assert_eq!(rows.len(), 1, "one maintenance session of {role}");
    rows.into_iter().next().expect("one row")
}

/// The maintenance session of `role` is idle, outside any transaction.
async fn assert_idle(cluster: &Cluster, role: &str) {
    let (state, query) = maintenance_activity(cluster, role).await;
    assert_eq!(state, "idle", "after: {query}");
}

/// A repeated `create` over drift is refused and changes nothing.
async fn assert_refused_and_preserved(
    cluster: &Cluster,
    conn: &mut MaintenanceConn,
    tenant: &Tenant,
) {
    let before = database_state(cluster, tenant.database.as_str()).await;
    assert!(before.is_some());
    let error = databases::create(conn, &tenant.database, &tenant.role)
        .await
        .expect_err("drift is refused");
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), None);
    assert!(!conn.is_poisoned());
    assert_idle(cluster, cluster.maintenance_role()).await;
    assert_eq!(
        database_state(cluster, tenant.database.as_str()).await,
        before
    );
}

/// A drop the crate itself refused, by the decision row `why`, with
/// `INTERNAL`: the session of `session_role` sent no `SET ROLE` (its last
/// statement is the drop's catalog read) and the database is in place.
async fn assert_drop_refused(
    cluster: &Cluster,
    conn: &mut MaintenanceConn,
    session_role: &str,
    database: &DatabaseName,
    owner: &TenantRole,
    why: &str,
) {
    let before = database_state(cluster, database.as_str()).await;
    assert!(before.is_some());
    let error = databases::drop(conn, database, owner)
        .await
        .expect_err("the drop is refused");
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), None);
    assert_eq!(source(&error), why);
    assert!(!conn.is_poisoned());
    let (state, last) = maintenance_activity(cluster, session_role).await;
    assert_eq!(state, "idle");
    assert!(last.contains("d.datistemplate"), "{last}");
    assert!(!last.contains("SET ROLE"), "{last}");
    assert!(!last.contains("RESET ROLE"), "{last}");
    assert_eq!(database_state(cluster, database.as_str()).await, before);
}

/// A database the maintenance role creates and therefore owns.
async fn maintenance_owned_database(cluster: &Cluster) -> String {
    let name = cluster.unique_name("upkeep");
    let mut own = PgConnection::connect_with(&cluster.maintenance_options())
        .await
        .expect("the maintenance role connected");
    common::execute(
        &mut own,
        &format!("CREATE DATABASE {}", common::quote(&name)),
    )
    .await;
    own.close().await.expect("closed");
    name
}

/// A connection poisoned by a drop cancelled between its `SET ROLE` and its
/// `RESET ROLE`: the drop waits on a lock of `pg_database` the admin holds.
async fn poisoned_connection(cluster: &Cluster) -> MaintenanceConn {
    let mut conn = cluster.maintenance().connect().await.expect("connected");
    let victim = tenant(cluster, &mut conn).await;
    let mut lock = cluster.admin().await;
    common::execute(&mut lock, "BEGIN").await;
    common::execute(&mut lock, "LOCK TABLE pg_catalog.pg_database IN SHARE MODE").await;
    {
        let dropping = databases::drop(&mut conn, &victim.database, &victim.role);
        tokio::pin!(dropping);
        tokio::select! {
            outcome = &mut dropping => {
                panic!("the drop finished under the lock: {:?}", outcome.map_err(|e| e.code()));
            }
            () = async {
                sekvent_testing::await_until!(
                    admin_count(cluster, LOCK_WAITERS, cluster.maintenance_role(), None).await > 0
                );
            } => {}
        }
    }
    common::execute(&mut lock, "ROLLBACK").await;
    lock.close().await.expect("the admin session closed");
    assert!(conn.is_poisoned());
    conn
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_created_database_opens_only_to_its_owner() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = tenant(&cluster, &mut conn).await;
        assert_eq!(
            databases::access(&mut conn, tenant.database.as_str())
                .await
                .expect("read"),
            Some(DatabaseAccess {
                owner: tenant.role.as_str().to_owned(),
                public_connect: false,
            })
        );

        let session = cluster
            .connect_as(
                tenant.role.as_str(),
                &tenant.password,
                tenant.database.as_str(),
            )
            .await
            .expect("the owner connects");
        session.close().await.expect("closed");

        let refused = cluster
            .connect_as(
                cluster.maintenance_role(),
                cluster.maintenance_password(),
                tenant.database.as_str(),
            )
            .await
            .expect_err("the maintenance role does not connect");
        assert_eq!(sqlstate(&refused).as_deref(), Some("42501"));

        let (third, password) = cluster.create_login_role("third").await;
        let refused = cluster
            .connect_as(&third, &password, tenant.database.as_str())
            .await
            .expect_err("another LOGIN role does not connect");
        assert_eq!(sqlstate(&refused).as_deref(), Some("42501"));

        let before = database_state(&cluster, tenant.database.as_str()).await;
        databases::create(&mut conn, &tenant.database, &tenant.role)
            .await
            .expect("a repeated create converges");
        assert_eq!(
            database_state(&cluster, tenant.database.as_str()).await,
            before
        );
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_create_interrupted_after_create_database_converges() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = tenant_role(&cluster, &mut conn).await;
        let mut own = PgConnection::connect_with(&cluster.maintenance_options())
            .await
            .expect("the maintenance role connected");
        common::execute(
            &mut own,
            &format!(
                "CREATE DATABASE {} OWNER {} ALLOW_CONNECTIONS false",
                common::quote(tenant.database.as_str()),
                common::quote(tenant.role.as_str())
            ),
        )
        .await;
        own.close().await.expect("closed");
        cluster
            .connect_as(
                tenant.role.as_str(),
                &tenant.password,
                tenant.database.as_str(),
            )
            .await
            .expect_err("the closed database does not open yet");

        databases::create(&mut conn, &tenant.database, &tenant.role)
            .await
            .expect("the create converged");
        let access = databases::access(&mut conn, tenant.database.as_str())
            .await
            .expect("read")
            .expect("the database exists");
        assert!(!access.public_connect);
        let session = cluster
            .connect_as(
                tenant.role.as_str(),
                &tenant.password,
                tenant.database.as_str(),
            )
            .await
            .expect("the owner connects");
        session.close().await.expect("closed");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn another_owner_or_a_template_is_refused_and_preserved() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");

        let taken = tenant_role(&cluster, &mut conn).await;
        let other = cluster.create_role("other", "").await;
        cluster
            .create_database_named(taken.database.as_str(), &other)
            .await;
        assert_refused_and_preserved(&cluster, &mut conn, &taken).await;

        let template = tenant(&cluster, &mut conn).await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "ALTER DATABASE {} IS_TEMPLATE true",
                    common::quote(template.database.as_str())
                ),
            )
            .await;
        assert_refused_and_preserved(&cluster, &mut conn, &template).await;
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_extra_grant_is_refused_and_preserved() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let third = cluster.create_role("third", "").await;
        for (privilege, grantee) in [
            ("CONNECT", common::quote(&third)),
            ("CONNECT", common::quote(cluster.maintenance_role())),
            ("CREATE", "PUBLIC".to_owned()),
        ] {
            let tenant = tenant(&cluster, &mut conn).await;
            let database = common::quote(tenant.database.as_str());
            cluster
                .admin_execute(
                    "postgres",
                    &format!("GRANT {privilege} ON DATABASE {database} TO {grantee}"),
                )
                .await;
            assert_refused_and_preserved(&cluster, &mut conn, &tenant).await;
        }
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn size_is_read_without_a_superuser() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = tenant(&cluster, &mut conn).await;
        let bytes = databases::size(&mut conn, &tenant.database)
            .await
            .expect("measured as the owner");
        assert!(bytes > 0);

        let upkeep = maintenance_owned_database(&cluster).await;
        let bytes = databases::size(&mut conn, &name(&upkeep))
            .await
            .expect("measured as the session's role");
        assert!(bytes > 0);

        let third = cluster.create_role("third", "").await;
        let foreign = cluster.create_database("foreign", &third).await;
        let error = databases::size(&mut conn, &name(&foreign))
            .await
            .expect_err("a third owner is refused");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        // The refusal inside the size's transaction rolled it back.
        assert_idle(&cluster, cluster.maintenance_role()).await;
        assert!(!conn.is_poisoned());

        let missing = cluster.unique_name("missing");
        let error = databases::size(&mut conn, &name(&missing))
            .await
            .expect_err("a missing database is refused");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_idle(&cluster, cluster.maintenance_role()).await;
        assert!(!conn.is_poisoned());
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn access_reads_the_owner_and_public_connect() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let third = cluster.create_role("third", "").await;
        let open = cluster.create_database("open", &third).await;
        assert_eq!(
            databases::access(&mut conn, &open).await.expect("read"),
            Some(DatabaseAccess {
                owner: third.clone(),
                public_connect: true,
            })
        );

        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "REVOKE CONNECT ON DATABASE {} FROM PUBLIC",
                    common::quote(&open)
                ),
            )
            .await;
        assert_eq!(
            databases::access(&mut conn, &open).await.expect("read"),
            Some(DatabaseAccess {
                owner: third.clone(),
                public_connect: false,
            })
        );
        // `TEMPORARY` alone is not `CONNECT`.
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT TEMPORARY ON DATABASE {} TO PUBLIC",
                    common::quote(&open)
                ),
            )
            .await;
        assert_eq!(
            databases::access(&mut conn, &open)
                .await
                .expect("read")
                .map(|access| access.public_connect),
            Some(false)
        );
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT CONNECT ON DATABASE {} TO PUBLIC",
                    common::quote(&open)
                ),
            )
            .await;
        assert_eq!(
            databases::access(&mut conn, &open).await.expect("read"),
            Some(DatabaseAccess {
                owner: third,
                public_connect: true,
            })
        );
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn access_is_one_statement_for_any_name() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let missing = cluster.unique_name("missing");
        assert_eq!(
            databases::access(&mut conn, &missing).await.expect("read"),
            None
        );
        // The session's last statement read the owner and the ACL together.
        let (state, query) = maintenance_activity(&cluster, cluster.maintenance_role()).await;
        assert_eq!(state, "idle", "after: {query}");
        assert!(
            query.contains("datdba") && query.contains("aclexplode"),
            "{query}"
        );
        assert!(query.contains("$1"), "the name is bound: {query}");
        assert!(!query.contains(&missing), "{query}");

        let postgres = databases::access(&mut conn, "postgres")
            .await
            .expect("read")
            .expect("postgres exists");
        assert!(postgres.public_connect, "postgres keeps its default ACL");
        assert_ne!(postgres.owner, "");

        let quoted = "orders'; DROP DATABASE postgres; --";
        assert_eq!(
            databases::access(&mut conn, quoted).await.expect("read"),
            None
        );
        assert!(!conn.is_poisoned());
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_custom_application_name_shows_on_every_session() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance().with_application_name("orders-upkeep");
        assert_eq!(maintenance.application_name(), "orders-upkeep");
        let conn = maintenance.connect().await.expect("connected");
        let sessions = admin_count(
            &cluster,
            NAMED_SESSIONS,
            cluster.maintenance_role(),
            Some("orders-upkeep"),
        )
        .await;
        assert_eq!(sessions, 1);
        assert_eq!(
            admin_count(
                &cluster,
                MAINTENANCE_SESSIONS,
                cluster.maintenance_role(),
                Some(cluster.maintenance_database()),
            )
            .await,
            0,
            "the default name is replaced"
        );
        conn.close().await.expect("closed");

        let pool = maintenance
            .pool_on(&name(cluster.maintenance_database()))
            .await
            .expect("the pool holds the permit");
        let shown: String = sqlx::query_scalar("SELECT current_setting('application_name')")
            .fetch_one(pool.pool())
            .await
            .expect("the setting was read");
        assert_eq!(shown, "orders-upkeep");
        pool.close().await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn drop_ends_the_sessions_and_removes_the_database() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = tenant(&cluster, &mut conn).await;
        let mut session = cluster
            .connect_as(
                tenant.role.as_str(),
                &tenant.password,
                tenant.database.as_str(),
            )
            .await
            .expect("the owner connects");

        databases::drop(&mut conn, &tenant.database, &tenant.role)
            .await
            .expect("dropped with a session connected");
        assert!(!conn.is_poisoned());
        assert_eq!(
            database_state(&cluster, tenant.database.as_str()).await,
            None
        );
        assert_eq!(cluster.sessions(tenant.database.as_str()).await, 0);
        common::try_execute(&mut session, "SELECT 1")
            .await
            .expect_err("the owner's session was ended");

        databases::drop(&mut conn, &tenant.database, &tenant.role)
            .await
            .expect("a missing database drops Ok");
        roles::drop(&mut conn, &tenant.role)
            .await
            .expect("the role follows");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn drop_refuses_what_is_not_the_tenants() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = tenant(&cluster, &mut conn).await;

        let maintenance = cluster.maintenance_role();
        let foreign = "the database is owned by another role while its tenant role exists";

        let third = cluster.create_role("third", "").await;
        let unrelated = name(&cluster.create_database("unrelated", &third).await);
        assert_drop_refused(
            &cluster,
            &mut conn,
            maintenance,
            &unrelated,
            &tenant.role,
            foreign,
        )
        .await;

        let upkeep = name(&maintenance_owned_database(&cluster).await);
        assert_drop_refused(
            &cluster,
            &mut conn,
            maintenance,
            &upkeep,
            &tenant.role,
            foreign,
        )
        .await;

        // The maintenance database is the bootstrap admin's: another owner.
        let maintenance_database = name(cluster.maintenance_database());
        assert_drop_refused(
            &cluster,
            &mut conn,
            maintenance,
            &maintenance_database,
            &tenant.role,
            foreign,
        )
        .await;

        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "ALTER DATABASE {} IS_TEMPLATE true",
                    common::quote(tenant.database.as_str())
                ),
            )
            .await;
        assert_drop_refused(
            &cluster,
            &mut conn,
            maintenance,
            &tenant.database,
            &tenant.role,
            "the database is a template",
        )
        .await;
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn drop_refuses_the_sessions_own_database() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let mut conn = maintenance.connect().await.expect("connected");
        let tenant = tenant_role(&cluster, &mut conn).await;
        conn.close().await.expect("closed");
        // The tenant role owns it and `PUBLIC` may still connect, so the
        // maintenance role can open a session on it.
        let own = name(&cluster.unique_name("own"));
        cluster
            .create_database_named(own.as_str(), tenant.role.as_str())
            .await;
        let mut conn = maintenance.connect_to(&own).await.expect("connected");
        assert_drop_refused(
            &cluster,
            &mut conn,
            cluster.maintenance_role(),
            &own,
            &tenant.role,
            "the database is the maintenance session's own",
        )
        .await;
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn drop_refuses_a_database_whose_owner_is_the_sessions_role() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = tenant(&cluster, &mut conn).await;
        conn.close().await.expect("closed");
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT CONNECT ON DATABASE {} TO {}",
                    common::quote(cluster.maintenance_database()),
                    common::quote(tenant.role.as_str())
                ),
            )
            .await;
        // A maintenance connection that signs in as the tenant role itself.
        let as_owner = Maintenance::new(
            cluster.options_for(
                tenant.role.as_str(),
                &tenant.password,
                cluster.maintenance_database(),
            ),
            cluster.names().clone(),
        );
        let mut conn = as_owner.connect().await.expect("connected");
        assert_drop_refused(
            &cluster,
            &mut conn,
            tenant.role.as_str(),
            &tenant.database,
            &tenant.role,
            "the database's owner is the session's role",
        )
        .await;
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_database_of_another_owner_is_left_alone_once_the_role_is_gone() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = tenant(&cluster, &mut conn).await;
        databases::drop(&mut conn, &tenant.database, &tenant.role)
            .await
            .expect("dropped");
        roles::drop(&mut conn, &tenant.role).await.expect("dropped");

        let other = cluster.create_role("other", "").await;
        cluster
            .create_database_named(tenant.database.as_str(), &other)
            .await;
        let before = database_state(&cluster, tenant.database.as_str()).await;
        databases::drop(&mut conn, &tenant.database, &tenant.role)
            .await
            .expect("someone else's database is left alone");
        assert!(before.is_some());
        assert_eq!(
            database_state(&cluster, tenant.database.as_str()).await,
            before
        );
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_failed_drop_returns_to_the_maintenance_role() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = tenant(&cluster, &mut conn).await;
        // The tenant role may not end a superuser's session, so the forced
        // drop fails after its SET ROLE.
        let blocker = cluster.admin_on(tenant.database.as_str()).await;

        let error = databases::drop(&mut conn, &tenant.database, &tenant.role)
            .await
            .expect_err("the drop cannot end the superuser's session");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert!(!conn.is_poisoned(), "the RESET ROLE succeeded");
        assert!(
            database_state(&cluster, tenant.database.as_str())
                .await
                .is_some()
        );

        // Only the maintenance role can create a role; the tenant role cannot.
        let next = tenant_role(&cluster, &mut conn).await;
        assert!(roles::exists(&mut conn, &next.role).await.expect("probed"));
        blocker.close().await.expect("closed");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_poisoned_connection_refuses_every_operation() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = poisoned_connection(&cluster).await;
        let tenant = common::new_tenant();
        let role = cluster.names().role(tenant);
        let database = cluster.names().database(tenant);
        let verifier =
            ScramVerifier::new(&common::new_password()).expect("the verifier was computed");
        for outcome in [
            databases::create(&mut conn, &database, &role).await,
            databases::drop(&mut conn, &database, &role).await,
            databases::size(&mut conn, &database).await.map(|_| ()),
            databases::access(&mut conn, database.as_str())
                .await
                .map(|_| ()),
            roles::create(&mut conn, &role, &verifier, 5).await,
            roles::exists(&mut conn, &role).await.map(|_| ()),
            roles::drop(&mut conn, &role).await,
        ] {
            let error = outcome.expect_err("the connection is poisoned");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), None);
            assert!(source(&error).contains("poisoned"), "{}", source(&error));
        }
        assert!(conn.is_poisoned());
        conn.close()
            .await
            .expect("a poisoned connection still closes");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn maintenance_sessions_carry_their_application_name() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let conn = maintenance.connect().await.expect("connected");
        let sessions = admin_count(
            &cluster,
            MAINTENANCE_SESSIONS,
            cluster.maintenance_role(),
            Some(cluster.maintenance_database()),
        )
        .await;
        assert_eq!(sessions, 1);
        conn.close().await.expect("closed");

        let upkeep = maintenance_owned_database(&cluster).await;
        let conn = maintenance
            .connect_to(&name(&upkeep))
            .await
            .expect("connected to another database");
        let sessions = admin_count(
            &cluster,
            MAINTENANCE_SESSIONS,
            cluster.maintenance_role(),
            Some(&upkeep),
        )
        .await;
        assert_eq!(sessions, 1);
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_pool_on_session_is_read_only() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let pool = maintenance
            .pool_on(&name(cluster.maintenance_database()))
            .await
            .expect("the pool holds the permit");
        let read_only: String =
            sqlx::query_scalar("SELECT current_setting('default_transaction_read_only')")
                .fetch_one(pool.pool())
                .await
                .expect("the setting was read");
        assert_eq!(read_only, "on");
        let search_path: String = sqlx::query_scalar("SELECT current_setting('search_path')")
            .fetch_one(pool.pool())
            .await
            .expect("the setting was read");
        assert!(
            !search_path.contains("pg_temp"),
            "the database's own search_path"
        );
        let refused = sqlx::raw_sql("CREATE TABLE orders (id bigint)")
            .execute(pool.pool())
            .await
            .expect_err("a write is refused");
        assert_eq!(sqlstate(&refused).as_deref(), Some("25006"));
        pool.close().await;

        let conn = maintenance
            .connect()
            .await
            .expect("the permit returned with the pool");
        conn.close().await.expect("closed");
    }))
    .await;
}
