//! Tenant roles on a real server: creation, convergence, drift refused and left alone, the connection limit, drop.
#![cfg(feature = "lifecycle")]

mod common;

use std::borrow::Cow;
use std::error::Error as _;

use common::Cluster;
use sekvent_config::Secret;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::cluster::{databases, roles};
use sekvent_tenancy::{MaintenanceConn, ObjectNames, ScramVerifier, TenantRole};
use sqlx::Connection;

/// A verifier Postgres stores as given: 16 bytes of salt, two 32-byte keys.
const FIXED_VERIFIER: &str = "SCRAM-SHA-256$4096:b3JkZXJzLXNhbHQtMDAxNg==$\
                              AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=:\
                              ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=";

/// A role's attributes, limit and memberships, with its verifier hashed so
/// that nothing secret is printed when two states differ.
const ROLE_STATE: &str = "SELECT concat_ws('|', a.rolsuper, a.rolinherit, a.rolcreaterole, \
     a.rolcreatedb, a.rolcanlogin, a.rolreplication, a.rolbypassrls, a.rolconnlimit, \
     md5(coalesce(a.rolpassword, '')), \
     (SELECT string_agg(concat_ws(':', g.rolname, m.rolname, e.admin_option, e.inherit_option, \
             e.set_option), ',' ORDER BY g.rolname, m.rolname, e.admin_option, \
             e.inherit_option, e.set_option) \
        FROM pg_auth_members e \
        JOIN pg_roles g ON g.oid = e.roleid \
        JOIN pg_roles m ON m.oid = e.member \
       WHERE e.roleid = a.oid OR e.member = a.oid)) \
     FROM pg_authid a WHERE a.rolname = $1";

/// Sessions of a role that wait on a lock.
const LOCK_WAITERS: &str =
    "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND wait_event_type = 'Lock'";

/// The state of every maintenance session of a role.
const MAINTENANCE_STATES: &str = "SELECT coalesce(state, '') FROM pg_stat_activity \
     WHERE usename = $1 AND application_name = 'sekvent-tenancy-maintenance'";

/// Whether a role of the name exists, read by the admin.
const ROLE_EXISTS: &str = "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)";

fn source(error: &AppError) -> String {
    error.source().map(ToString::to_string).unwrap_or_default()
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    sekvent_db::sqlstate(error).map(Cow::into_owned)
}

fn verifier(password: &Secret) -> ScramVerifier {
    ScramVerifier::new(password).expect("the verifier was computed")
}

/// A new tenant role created by the crate, and its password.
async fn new_role(
    cluster: &Cluster,
    conn: &mut MaintenanceConn,
    limit: u32,
) -> (TenantRole, Secret) {
    let role = cluster.names().role(common::new_tenant());
    let password = common::new_password();
    roles::create(conn, &role, &verifier(&password), limit)
        .await
        .expect("the role was created");
    (role, password)
}

async fn role_state(cluster: &Cluster, role: &str) -> String {
    let mut admin = cluster.admin().await;
    let state: String = sqlx::query_scalar(ROLE_STATE)
        .bind(role)
        .fetch_one(&mut admin)
        .await
        .expect("the role state was read");
    admin.close().await.expect("the admin session closed");
    state
}

/// The states of the maintenance sessions, read by the admin.
async fn maintenance_states(cluster: &Cluster) -> Vec<String> {
    let mut admin = cluster.admin().await;
    let states: Vec<String> = sqlx::query_scalar(MAINTENANCE_STATES)
        .bind(cluster.maintenance_role())
        .fetch_all(&mut admin)
        .await
        .expect("pg_stat_activity is readable");
    admin.close().await.expect("the admin session closed");
    states
}

async fn role_exists(cluster: &Cluster, role: &str) -> bool {
    let mut admin = cluster.admin().await;
    let exists: bool = sqlx::query_scalar(ROLE_EXISTS)
        .bind(role)
        .fetch_one(&mut admin)
        .await
        .expect("the role was looked up");
    admin.close().await.expect("the admin session closed");
    exists
}

/// A repeated `create` over drift is refused, changes nothing and leaves
/// the session idle outside a transaction. The refusal's source is
/// returned.
async fn assert_refused_and_preserved(
    cluster: &Cluster,
    conn: &mut MaintenanceConn,
    role: &TenantRole,
) -> String {
    let before = role_state(cluster, role.as_str()).await;
    let error = roles::create(conn, role, &verifier(&common::new_password()), 9)
        .await
        .expect_err("drift is refused");
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), None);
    assert!(!conn.is_poisoned());
    assert_eq!(maintenance_states(cluster).await, ["idle"]);
    assert_eq!(role_state(cluster, role.as_str()).await, before);
    source(&error)
}

async fn lock_waiters(cluster: &Cluster) -> i64 {
    let mut admin = cluster.admin().await;
    let waiting: i64 = sqlx::query_scalar(LOCK_WAITERS)
        .bind(cluster.maintenance_role())
        .fetch_one(&mut admin)
        .await
        .expect("pg_stat_activity is readable");
    admin.close().await.expect("the admin session closed");
    waiting
}

/// A connection poisoned by a drop cancelled between its `SET ROLE` and its
/// `RESET ROLE`: the drop waits on a lock of `pg_database` the admin holds.
async fn poisoned_connection(cluster: &Cluster) -> MaintenanceConn {
    let mut conn = cluster.maintenance().connect().await.expect("connected");
    let tenant = common::new_tenant();
    let role = cluster.names().role(tenant);
    let database = cluster.names().database(tenant);
    roles::create(&mut conn, &role, &verifier(&common::new_password()), 2)
        .await
        .expect("the role was created");
    databases::create(&mut conn, &database, &role)
        .await
        .expect("the database was created");

    let mut lock = cluster.admin().await;
    common::execute(&mut lock, "BEGIN").await;
    common::execute(&mut lock, "LOCK TABLE pg_catalog.pg_database IN SHARE MODE").await;
    {
        let dropping = databases::drop(&mut conn, &database, &role);
        tokio::pin!(dropping);
        tokio::select! {
            outcome = &mut dropping => {
                panic!("the drop finished under the lock: {:?}", outcome.map_err(|e| e.code()));
            }
            () = async { sekvent_testing::await_until!(lock_waiters(cluster).await > 0) } => {}
        }
    }
    common::execute(&mut lock, "ROLLBACK").await;
    lock.close().await.expect("the admin session closed");
    assert!(conn.is_poisoned());
    conn
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_created_role_has_exactly_its_attributes() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let role = cluster.names().role(common::new_tenant());
        assert!(!roles::exists(&mut conn, &role).await.expect("probed"));

        let fixed = ScramVerifier::parse(Secret::new(FIXED_VERIFIER)).expect("a valid verifier");
        roles::create(&mut conn, &role, &fixed, 4)
            .await
            .expect("created");
        assert!(roles::exists(&mut conn, &role).await.expect("probed"));

        let mut admin = cluster.admin().await;
        let attributes: (bool, bool, bool, bool, bool, bool, bool, i32, bool) = sqlx::query_as(
            "SELECT rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, \
             rolreplication, rolbypassrls, rolconnlimit, rolpassword = $2 \
             FROM pg_authid WHERE rolname = $1",
        )
        .bind(role.as_str())
        .bind(FIXED_VERIFIER)
        .fetch_one(&mut admin)
        .await
        .expect("the role was read");
        assert_eq!(
            attributes,
            (false, false, false, false, true, false, false, 4, true),
            "LOGIN NOINHERIT NOCREATEDB NOCREATEROLE, the limit, the verifier as given"
        );

        let members: Vec<(String, bool, bool)> = sqlx::query_as(
            "SELECT m.rolname::text, e.inherit_option, e.set_option FROM pg_auth_members e \
             JOIN pg_roles g ON g.oid = e.roleid JOIN pg_roles m ON m.oid = e.member \
             WHERE g.rolname = $1",
        )
        .bind(role.as_str())
        .fetch_all(&mut admin)
        .await
        .expect("the members were read");
        assert_ne!(members, [] as [(String, bool, bool); 0]);
        assert!(
            members
                .iter()
                .all(|(member, _, _)| member == cluster.maintenance_role())
        );
        assert!(members.iter().all(|(_, inherit, _)| !inherit), "no INHERIT");
        assert!(members.iter().any(|(_, _, set)| *set), "SET TRUE");

        let memberships: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_auth_members e JOIN pg_roles m ON m.oid = e.member \
             WHERE m.rolname = $1",
        )
        .bind(role.as_str())
        .fetch_one(&mut admin)
        .await
        .expect("the memberships were read");
        assert_eq!(memberships, 0, "the role is a member of nothing");
        admin.close().await.expect("the admin session closed");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_repeated_create_moves_the_verifier_and_the_limit() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let (role, first) = new_role(&cluster, &mut conn, 1).await;
        let second = common::new_password();
        roles::create(&mut conn, &role, &verifier(&second), 3)
            .await
            .expect("the role converged");

        let mut admin = cluster.admin().await;
        let limit: i32 = sqlx::query_scalar("SELECT rolconnlimit FROM pg_roles WHERE rolname = $1")
            .bind(role.as_str())
            .fetch_one(&mut admin)
            .await
            .expect("the limit was read");
        assert_eq!(limit, 3);
        admin.close().await.expect("the admin session closed");

        let session = cluster
            .connect_as(role.as_str(), &second, "postgres")
            .await
            .expect("the new password signs in");
        session.close().await.expect("closed");
        let refused = cluster
            .connect_as(role.as_str(), &first, "postgres")
            .await
            .expect_err("the old password no longer signs in");
        assert_eq!(sqlstate(&refused).as_deref(), Some("28P01"));
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_widening_attribute_is_refused_and_preserved() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        for attribute in [
            "SUPERUSER",
            "INHERIT",
            "CREATEROLE",
            "CREATEDB",
            "REPLICATION",
            "BYPASSRLS",
        ] {
            let (role, _) = new_role(&cluster, &mut conn, 2).await;
            cluster
                .admin_execute(
                    "postgres",
                    &format!("ALTER ROLE {} {attribute}", common::quote(role.as_str())),
                )
                .await;
            assert_refused_and_preserved(&cluster, &mut conn, &role).await;
        }
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn memberships_out_of_and_into_the_role_are_refused_and_preserved() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");

        let (out_of, _) = new_role(&cluster, &mut conn, 2).await;
        let target = cluster.create_role("target", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT {} TO {}",
                    common::quote(&target),
                    common::quote(out_of.as_str())
                ),
            )
            .await;
        assert_refused_and_preserved(&cluster, &mut conn, &out_of).await;

        let (into, _) = new_role(&cluster, &mut conn, 2).await;
        let member = cluster.create_role("member", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT {} TO {}",
                    common::quote(into.as_str()),
                    common::quote(&member)
                ),
            )
            .await;
        assert_refused_and_preserved(&cluster, &mut conn, &into).await;

        // A chain into the role whose only direct edge is the maintenance
        // role's own, which is allowed: link is a member of the maintenance
        // role and end a member of link, so both reach the role through two
        // and three edges, and no direct edge can be the finding.
        let (chained, _) = new_role(&cluster, &mut conn, 2).await;
        let link = cluster.create_role("link", "").await;
        let end = cluster.create_role("end", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT {} TO {}; GRANT {} TO {}",
                    common::quote(cluster.maintenance_role()),
                    common::quote(&link),
                    common::quote(&link),
                    common::quote(&end)
                ),
            )
            .await;
        let found = assert_refused_and_preserved(&cluster, &mut conn, &chained).await;
        assert!(found.contains(&link), "{found}");
        assert!(found.contains(&end), "{found}");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_chain_out_of_the_role_is_refused_and_preserved() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        // The role is a member of near, and near of far: far is reached
        // only through two edges.
        let (role, _) = new_role(&cluster, &mut conn, 2).await;
        let near = cluster.create_role("near", "").await;
        let far = cluster.create_role("far", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT {} TO {}; GRANT {} TO {}",
                    common::quote(&near),
                    common::quote(role.as_str()),
                    common::quote(&far),
                    common::quote(&near)
                ),
            )
            .await;
        let found = assert_refused_and_preserved(&cluster, &mut conn, &role).await;
        assert!(found.contains(&far), "{found}");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_new_role_refused_after_its_create_is_rolled_back() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        // A member of the maintenance role reaches every role it holds, so
        // the check after CREATE ROLE and the grant refuses the new role.
        let member = cluster.create_role("upkeep", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT {} TO {}",
                    common::quote(cluster.maintenance_role()),
                    common::quote(&member)
                ),
            )
            .await;
        let role = cluster.names().role(common::new_tenant());
        let error = roles::create(&mut conn, &role, &verifier(&common::new_password()), 2)
            .await
            .expect_err("the created role is reached by a third role");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert!(source(&error).contains(&member), "{}", source(&error));
        assert!(!conn.is_poisoned());
        // Rolled back before the refusal returned: no open transaction holds
        // the new role.
        assert_eq!(maintenance_states(&cluster).await, ["idle"]);
        assert!(!role_exists(&cluster, role.as_str()).await);
        assert!(!roles::exists(&mut conn, &role).await.expect("probed"));
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_create_dropped_in_flight_poisons_the_connection() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let role = cluster.names().role(common::new_tenant());
        // CREATE ROLE waits on this lock, inside the create's transaction.
        let mut lock = cluster.admin().await;
        common::execute(&mut lock, "BEGIN").await;
        common::execute(&mut lock, "LOCK TABLE pg_catalog.pg_authid IN SHARE MODE").await;
        {
            let fresh = verifier(&common::new_password());
            let creating = roles::create(&mut conn, &role, &fresh, 2);
            tokio::pin!(creating);
            tokio::select! {
                outcome = &mut creating => {
                    panic!("the create finished under the lock: {:?}", outcome.map_err(|e| e.code()));
                }
                () = async { sekvent_testing::await_until!(lock_waiters(&cluster).await > 0) } => {}
            }
        }
        common::execute(&mut lock, "ROLLBACK").await;
        lock.close().await.expect("the admin session closed");

        assert!(conn.is_poisoned(), "the create did not run to its end");
        for outcome in [
            roles::exists(&mut conn, &role).await.map(|_| ()),
            roles::drop(&mut conn, &role).await,
        ] {
            let error = outcome.expect_err("the connection is poisoned");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(source(&error).contains("poisoned"), "{}", source(&error));
        }
        assert!(conn.is_poisoned(), "a refused operation does not lift it");
        conn.close()
            .await
            .expect("a poisoned connection still closes");
        assert!(!role_exists(&cluster, role.as_str()).await);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_member_of_the_maintenance_role_reaches_the_role_and_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let (role, _) = new_role(&cluster, &mut conn, 2).await;
        let member = cluster.create_role("upkeep", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT {} TO {}",
                    common::quote(cluster.maintenance_role()),
                    common::quote(&member)
                ),
            )
            .await;
        assert_refused_and_preserved(&cluster, &mut conn, &role).await;
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_maintenance_membership_with_inherit_is_refused_and_preserved() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let (role, _) = new_role(&cluster, &mut conn, 2).await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT {} TO {} WITH INHERIT TRUE",
                    common::quote(role.as_str()),
                    common::quote(cluster.maintenance_role())
                ),
            )
            .await;
        assert_refused_and_preserved(&cluster, &mut conn, &role).await;
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_connection_limit_holds() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let (role, password) = new_role(&cluster, &mut conn, 1).await;
        let first = cluster
            .connect_as(role.as_str(), &password, "postgres")
            .await
            .expect("the first session signs in");
        let refused = cluster
            .connect_as(role.as_str(), &password, "postgres")
            .await
            .expect_err("the limit is one session");
        assert_eq!(sqlstate(&refused).as_deref(), Some("53300"));
        first.close().await.expect("closed");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn drop_removes_the_role_and_repeats_cleanly() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let (role, _) = new_role(&cluster, &mut conn, 2).await;
        assert!(roles::exists(&mut conn, &role).await.expect("probed"));
        roles::drop(&mut conn, &role).await.expect("dropped");
        assert!(!roles::exists(&mut conn, &role).await.expect("probed"));
        roles::drop(&mut conn, &role)
            .await
            .expect("a missing role drops Ok");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_role_that_owns_its_database_is_not_dropped() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = cluster.maintenance().connect().await.expect("connected");
        let tenant = common::new_tenant();
        let role = cluster.names().role(tenant);
        let database = cluster.names().database(tenant);
        roles::create(&mut conn, &role, &verifier(&common::new_password()), 2)
            .await
            .expect("created");
        databases::create(&mut conn, &database, &role)
            .await
            .expect("created");
        roles::drop(&mut conn, &role)
            .await
            .expect_err("a role that owns a database cannot be dropped");
        assert!(roles::exists(&mut conn, &role).await.expect("probed"));
        databases::drop(&mut conn, &database, &role)
            .await
            .expect("dropped");
        roles::drop(&mut conn, &role)
            .await
            .expect("dropped after its database");
        conn.close().await.expect("closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_refused_argument_is_answered_before_the_poisoned_check() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let mut conn = poisoned_connection(&cluster).await;
        let role = cluster.names().role(common::new_tenant());
        let foreign = ObjectNames::new("billing_t_")
            .expect("the prefix is valid")
            .role(common::new_tenant());
        let fresh = verifier(&common::new_password());

        for limit in [0, 2_147_483_648] {
            let error = roles::create(&mut conn, &role, &fresh, limit)
                .await
                .expect_err("the limit is refused");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(
                source(&error).contains("connection limit"),
                "{}",
                source(&error)
            );
        }
        for error in [
            roles::create(&mut conn, &foreign, &fresh, 5).await,
            roles::exists(&mut conn, &foreign).await.map(|_| ()),
            roles::drop(&mut conn, &foreign).await,
        ] {
            let error = error.expect_err("another prefix is refused");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(
                source(&error).contains("not an object name"),
                "{}",
                source(&error)
            );
        }
        for error in [
            roles::create(&mut conn, &role, &fresh, 5).await,
            roles::exists(&mut conn, &role).await.map(|_| ()),
            roles::drop(&mut conn, &role).await,
        ] {
            let error = error.expect_err("the connection is poisoned");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(source(&error).contains("poisoned"), "{}", source(&error));
        }
        conn.close()
            .await
            .expect("a poisoned connection still closes");
    }))
    .await;
}
