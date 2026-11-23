//! The ownership handover on a real server: what moves, each refusal with its check, convergence, the restrict path.
#![cfg(feature = "lifecycle")]

mod common;

use std::borrow::Cow;
use std::error::Error as _;

use common::{Cluster, quote};
use sekvent_config::Secret;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::cluster::handover::{self, Adoption};
use sekvent_tenancy::cluster::roles;
use sekvent_tenancy::{DatabaseName, Maintenance, ScramVerifier, TenantRole, reasons};
use sqlx::{Connection, PgConnection};

/// What a handover may change, read from the shared catalogs on `postgres`,
/// so that reading it opens no session on the database itself. Booleans are
/// spelt `true` and `false` (`format` alone would print `t` and `f`).
#[derive(Debug, PartialEq, Eq)]
struct State {
    owner: String,
    allow_connections: bool,
    public_connect: bool,
    /// `grantee:privilege:grantable` of the ACL with its defaults made
    /// explicit, `PUBLIC`'s `CONNECT` left out.
    grants: Vec<String>,
    /// `catalog:object:owner` of every recorded ownership in the database.
    owned: Vec<String>,
    /// `inherit:set` of every membership of the maintenance role in the
    /// tenant role.
    memberships: Vec<String>,
}

async fn state(cluster: &Cluster, database: &str, role: &str) -> State {
    let mut admin = cluster.admin().await;
    let (owner, allow_connections, public_connect) = sqlx::query_as::<_, (String, bool, bool)>(
        "SELECT pg_get_userbyid(datdba)::text, datallowconn,
                has_database_privilege('public', oid, 'CONNECT')
           FROM pg_database WHERE datname = $1",
    )
    .bind(database)
    .fetch_one(&mut admin)
    .await
    .expect("the database exists");
    let grants = sqlx::query_scalar::<_, String>(
        "SELECT format('%s:%s:%s',
                       CASE WHEN a.grantee = 0 THEN 'PUBLIC'
                            ELSE pg_get_userbyid(a.grantee)::text END,
                       a.privilege_type, a.is_grantable::text)
           FROM pg_database d,
                aclexplode(coalesce(d.datacl, acldefault('d', d.datdba))) a
          WHERE d.datname = $1
          ORDER BY 1",
    )
    .bind(database)
    .fetch_all(&mut admin)
    .await
    .expect("the ACL is readable")
    .into_iter()
    .filter(|grant| grant != "PUBLIC:CONNECT:false")
    .collect();
    let ownerships = sqlx::query_scalar::<_, String>(
        "SELECT format('%s:%s:%s', s.classid::regclass, s.objid, pg_get_userbyid(s.refobjid))
           FROM pg_shdepend s JOIN pg_database d ON d.oid = s.dbid
          WHERE d.datname = $1 AND s.deptype = 'o'
          ORDER BY 1",
    )
    .bind(database)
    .fetch_all(&mut admin)
    .await
    .expect("the dependencies are readable");
    let memberships = sqlx::query_scalar::<_, String>(
        "SELECT format('%s:%s', m.inherit_option::text, m.set_option::text)
           FROM pg_auth_members m
          WHERE m.roleid = (SELECT oid FROM pg_roles WHERE rolname = $1)
            AND m.member = (SELECT oid FROM pg_roles WHERE rolname = $2)
          ORDER BY 1",
    )
    .bind(role)
    .bind(cluster.maintenance_role())
    .fetch_all(&mut admin)
    .await
    .expect("the memberships are readable");
    admin.close().await.expect("the admin session closed");
    State {
        owner,
        allow_connections,
        public_connect,
        grants,
        owned: ownerships,
        memberships,
    }
}

/// A database the maintenance role creates, and so owns, with the default
/// ACL (`PUBLIC` may connect).
async fn candidate(cluster: &Cluster) -> DatabaseName {
    let name = cluster.unique_name("candidate");
    let mut conn = PgConnection::connect_with(&cluster.maintenance_options())
        .await
        .expect("the maintenance role connected");
    common::execute(
        &mut conn,
        &format!("CREATE DATABASE {}", common::quote(&name)),
    )
    .await;
    conn.close().await.expect("the session closed");
    DatabaseName::parse(&name).expect("a fixture name is a database name")
}

/// A tenant role made by the crate, held by the maintenance role, and the
/// password it signs in with.
async fn tenant_role(cluster: &Cluster, maintenance: &Maintenance) -> (TenantRole, Secret) {
    let role = cluster.names().role(common::new_tenant());
    let password = common::new_password();
    let verifier = ScramVerifier::new(&password).expect("the verifier was made");
    let mut conn = maintenance.connect().await.expect("a maintenance session");
    roles::create(&mut conn, &role, &verifier, 5)
        .await
        .expect("the role was created");
    conn.close().await.expect("the session closed");
    (role, password)
}

/// Runs `sql` as the maintenance role on `database`.
async fn as_maintenance(cluster: &Cluster, database: &str, sql: &str) {
    let mut conn = cluster
        .connect_as(
            cluster.maintenance_role(),
            cluster.maintenance_password(),
            database,
        )
        .await
        .expect("the maintenance role connected");
    common::execute(&mut conn, sql).await;
    conn.close().await.expect("the session closed");
}

/// Sessions of the maintenance role that wait on a lock.
const LOCK_WAITERS: &str =
    "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND wait_event_type = 'Lock'";

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

/// Waits until no session is left on `database`: a closed session leaves
/// `pg_stat_activity` a moment after its client is gone.
async fn no_sessions(cluster: &Cluster, database: &str) {
    sekvent_testing::await_until!(cluster.sessions(database).await == 0);
}

async fn run_prepare(
    maintenance: &Maintenance,
    database: &DatabaseName,
    role: &TenantRole,
) -> Result<Adoption, AppError> {
    Box::pin(handover::prepare(maintenance, database, role)).await
}

async fn run_adopt(
    maintenance: &Maintenance,
    database: &DatabaseName,
    role: &TenantRole,
) -> Result<(), AppError> {
    Box::pin(handover::adopt(maintenance, database, role)).await
}

/// The check a refusal names, after its shape is asserted.
fn refused_check(err: &AppError) -> String {
    assert_eq!(err.code(), ErrorCode::FailedPrecondition);
    assert_eq!(err.reason(), Some(reasons::ADOPTION_REFUSED));
    assert_eq!(err.message(), "the database cannot be adopted");
    assert_eq!(err.metadata().len(), 1);
    err.metadata()
        .get(handover::CHECK_KEY)
        .cloned()
        .expect("the refusal names its check")
}

fn sqlstate(err: &sqlx::Error) -> Option<String> {
    sekvent_db::sqlstate(err).map(Cow::into_owned)
}

fn source(err: &AppError) -> String {
    err.source().map(ToString::to_string).unwrap_or_default()
}

/// The refusal of a tenant role that fails its check: `INTERNAL` without a
/// reason, the generic message, the role and `finding` in the source.
fn assert_role_refused(err: &AppError, role: &TenantRole, finding: &str) {
    assert_eq!(err.code(), ErrorCode::Internal, "{err:?}");
    assert_eq!(err.reason(), None);
    assert_eq!(err.message(), "internal error");
    let text = source(err);
    assert!(
        text.contains(&format!("tenant role {} is not as expected", role.as_str())),
        "{text}"
    );
    assert!(text.contains(finding), "{text}");
}

/// The maintenance role is refused on `database` for lack of `CONNECT`.
async fn maintenance_cannot_connect(cluster: &Cluster, database: &str) {
    let err = cluster
        .connect_as(
            cluster.maintenance_role(),
            cluster.maintenance_password(),
            database,
        )
        .await
        .expect_err("the maintenance role may not connect");
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"));
}

/// `role` signs in on `database` with `password` and is the session's user.
async fn tenant_signs_in(cluster: &Cluster, role: &TenantRole, password: &Secret, database: &str) {
    let mut conn = cluster
        .connect_as(role.as_str(), password, database)
        .await
        .expect("the tenant role connected");
    let user = sqlx::query_scalar::<_, String>("SELECT current_user::text")
        .fetch_one(&mut conn)
        .await
        .expect("the session answered");
    assert_eq!(user, role.as_str());
    conn.close().await.expect("the session closed");
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_handover_moves_the_database_and_everything_in_it() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, password) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            "CREATE SCHEMA inventory;
             CREATE TABLE inventory.items (id bigint PRIMARY KEY, label text NOT NULL);
             CREATE SEQUENCE inventory.item_numbers;
             CREATE TYPE public.item_state AS ENUM ('open', 'closed');
             CREATE FUNCTION public.item_count() RETURNS bigint
                 LANGUAGE sql AS 'SELECT count(*) FROM inventory.items';",
        )
        .await;

        assert_eq!(
            run_prepare(&maintenance, &database, &role)
                .await
                .expect("prepared"),
            Adoption::Reassign
        );
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("the handover succeeded");

        let after = state(&cluster, database.as_str(), role.as_str()).await;
        assert_eq!(after.owner, role.as_str());
        assert!(after.allow_connections);
        assert!(!after.public_connect);
        assert!(!after.owned.is_empty(), "the objects are recorded");
        for owned in &after.owned {
            assert!(
                owned.ends_with(&format!(":{}", role.as_str())),
                "{owned} is the tenant role's"
            );
        }
        assert!(
            after
                .memberships
                .iter()
                .all(|edge| edge.starts_with("false:")),
            "no membership inherits: {:?}",
            after.memberships
        );
        assert!(
            after.memberships.iter().any(|edge| edge == "false:true"),
            "the maintenance role may still switch: {:?}",
            after.memberships
        );

        let mut admin = cluster.admin_on(database.as_str()).await;
        let owners = sqlx::query_as::<_, (String, String)>(
            "SELECT 'schema', pg_get_userbyid(nspowner)::text
               FROM pg_namespace WHERE nspname = 'inventory'
             UNION ALL
             SELECT 'table', pg_get_userbyid(relowner)::text
               FROM pg_class WHERE oid = 'inventory.items'::regclass
             UNION ALL
             SELECT 'sequence', pg_get_userbyid(relowner)::text
               FROM pg_class WHERE oid = 'inventory.item_numbers'::regclass
             UNION ALL
             SELECT 'type', pg_get_userbyid(typowner)::text
               FROM pg_type WHERE oid = 'public.item_state'::regtype
             UNION ALL
             SELECT 'function', pg_get_userbyid(proowner)::text
               FROM pg_proc WHERE oid = 'public.item_count()'::regprocedure
             ORDER BY 1",
        )
        .fetch_all(&mut admin)
        .await
        .expect("the objects are readable");
        admin.close().await.expect("the admin session closed");
        let expected: Vec<(String, String)> = ["function", "schema", "sequence", "table", "type"]
            .into_iter()
            .map(|kind| (kind.to_owned(), role.as_str().to_owned()))
            .collect();
        assert_eq!(owners, expected);

        tenant_signs_in(&cluster, &role, &password, database.as_str()).await;
        maintenance_cannot_connect(&cluster, database.as_str()).await;
        assert_eq!(
            run_prepare(&maintenance, &database, &role)
                .await
                .expect("prepared"),
            Adoption::Restrict
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_missing_database_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let absent_role = cluster.names().role(common::new_tenant());
        let database =
            DatabaseName::parse(&cluster.unique_name("absent")).expect("a database name");
        let prepared = run_prepare(&maintenance, &database, &absent_role)
            .await
            .expect_err("nothing to adopt");
        assert_eq!(refused_check(&prepared), "missing");

        // A role that does not exist yet is no finding of step 1.
        let adopted = run_adopt(&maintenance, &database, &absent_role)
            .await
            .expect_err("nothing to adopt");
        assert_eq!(refused_check(&adopted), "missing");

        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let adopted = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("nothing to adopt");
        assert_eq!(refused_check(&adopted), "missing");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_template_is_refused_and_left_alone() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "ALTER DATABASE {} IS_TEMPLATE true",
                    common::quote(database.as_str())
                ),
            )
            .await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("a template is refused");
        assert_eq!(refused_check(&err), "template");
        assert_eq!(
            state(&cluster, database.as_str(), role.as_str()).await,
            before
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_database_of_a_third_role_is_refused_and_left_alone() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let third = cluster.create_role("owner", "").await;
        let name = cluster.create_database("foreign", &third).await;
        let database = DatabaseName::parse(&name).expect("a database name");
        let before = state(&cluster, &name, role.as_str()).await;
        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("another owner is refused");
        assert_eq!(refused_check(&err), "owner");
        assert_eq!(state(&cluster, &name, role.as_str()).await, before);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_grant_to_a_third_role_is_refused_and_left_alone() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        let third = cluster.create_role("reader", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT CONNECT ON DATABASE {} TO {}",
                    common::quote(database.as_str()),
                    common::quote(&third)
                ),
            )
            .await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("a third grantee is refused");
        assert_eq!(refused_check(&err), "grantee");
        assert_eq!(
            state(&cluster, database.as_str(), role.as_str()).await,
            before
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_maintenance_role_owning_another_database_is_refused() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        let other = candidate(&cluster).await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        let other_before = state(&cluster, other.as_str(), role.as_str()).await;
        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("REASSIGN OWNED would move the other database too");
        assert_eq!(refused_check(&err), "owns_more");
        assert_eq!(
            state(&cluster, database.as_str(), role.as_str()).await,
            before
        );
        assert_eq!(
            state(&cluster, other.as_str(), role.as_str()).await,
            other_before
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_object_of_a_third_role_is_refused_and_left_alone() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        let third = cluster.create_role("stray", "").await;
        cluster
            .admin_execute(
                database.as_str(),
                &format!(
                    "CREATE TABLE public.stray (id bigint); ALTER TABLE public.stray OWNER TO {}",
                    common::quote(&third)
                ),
            )
            .await;
        no_sessions(&cluster, database.as_str()).await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("a third role's table is refused");
        assert_eq!(refused_check(&err), "foreign_object");
        let after = state(&cluster, database.as_str(), role.as_str()).await;
        assert!(after.public_connect, "the check comes before the revoke");
        assert_eq!(after, before);
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_unrecorded_object_of_the_superuser_is_refused_before_the_role_exists() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let role = cluster.names().role(common::new_tenant());
        let database = candidate(&cluster).await;
        cluster
            .admin_execute(database.as_str(), "CREATE TABLE public.stray (id bigint)")
            .await;
        no_sessions(&cluster, database.as_str()).await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        let err = run_prepare(&maintenance, &database, &role)
            .await
            .expect_err("the superuser's table is refused");
        assert_eq!(refused_check(&err), "foreign_object");
        assert_eq!(
            state(&cluster, database.as_str(), role.as_str()).await,
            before
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_session_of_another_role_is_refused_with_only_public_connect_revoked() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, password) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            "CREATE TABLE public.orders (id bigint)",
        )
        .await;
        let (visitor, visitor_password) = cluster.create_login_role("visitor").await;
        let session = cluster
            .connect_as(&visitor, &visitor_password, database.as_str())
            .await
            .expect("PUBLIC may connect");
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        assert!(before.public_connect);

        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("a connected session is refused");
        assert_eq!(refused_check(&err), "session");
        let after = state(&cluster, database.as_str(), role.as_str()).await;
        assert!(!after.public_connect, "the revoke stays");
        assert_eq!(
            State {
                public_connect: true,
                ..after
            },
            before,
            "nothing else changed"
        );
        let late = cluster
            .connect_as(&visitor, &visitor_password, database.as_str())
            .await
            .expect_err("no new session may start");
        assert_eq!(sqlstate(&late).as_deref(), Some("42501"));

        session.close().await.expect("the visitor left");
        no_sessions(&cluster, database.as_str()).await;
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("the next call converges");
        let adopted = state(&cluster, database.as_str(), role.as_str()).await;
        assert_eq!(adopted.owner, role.as_str());
        assert!(!adopted.public_connect);
        tenant_signs_in(&cluster, &role, &password, database.as_str()).await;
        maintenance_cannot_connect(&cluster, database.as_str()).await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_repeated_call_converges() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, password) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            "CREATE TABLE public.orders (id bigint)",
        )
        .await;
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("the first call succeeded");
        let first = state(&cluster, database.as_str(), role.as_str()).await;
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("the second call succeeded");
        assert_eq!(
            state(&cluster, database.as_str(), role.as_str()).await,
            first
        );
        assert_eq!(first.owner, role.as_str());
        assert_eq!(
            run_prepare(&maintenance, &database, &role)
                .await
                .expect("prepared"),
            Adoption::Restrict
        );
        tenant_signs_in(&cluster, &role, &password, database.as_str()).await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_database_the_role_owns_takes_the_restrict_path() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, password) = tenant_role(&cluster, &maintenance).await;
        let name = cluster.create_database("owned", role.as_str()).await;
        let database = DatabaseName::parse(&name).expect("a database name");
        let before = state(&cluster, &name, role.as_str()).await;
        assert!(before.public_connect);

        assert_eq!(
            run_prepare(&maintenance, &database, &role)
                .await
                .expect("prepared"),
            Adoption::Restrict
        );
        assert_eq!(
            state(&cluster, &name, role.as_str()).await,
            before,
            "prepare changes nothing on this path"
        );
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("the restriction was applied");
        let after = state(&cluster, &name, role.as_str()).await;
        assert_eq!(after.owner, role.as_str());
        assert!(after.allow_connections);
        assert!(!after.public_connect);
        assert_eq!(after.memberships, before.memberships);
        tenant_signs_in(&cluster, &role, &password, &name).await;
        maintenance_cannot_connect(&cluster, &name).await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_recorded_ownership_alone_refuses() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        // A temporary table of a third role: the owner-column scan skips
        // temporary schemas, so only the recorded ownership names it. Its
        // session stays connected, which the session check would refuse, but
        // the object check comes first.
        let (visitor, visitor_password) = cluster.create_login_role("visitor").await;
        let mut session = cluster
            .connect_as(&visitor, &visitor_password, database.as_str())
            .await
            .expect("PUBLIC may connect");
        common::execute(&mut session, "CREATE TEMPORARY TABLE scratch (id bigint)").await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        assert!(
            before
                .owned
                .iter()
                .any(|owned| owned.ends_with(&format!(":{visitor}"))),
            "the temporary table's owner is recorded: {:?}",
            before.owned
        );

        let err = run_prepare(&maintenance, &database, &role)
            .await
            .expect_err("the recorded ownership refuses");
        assert_eq!(refused_check(&err), "foreign_object");
        assert_eq!(
            state(&cluster, database.as_str(), role.as_str()).await,
            before
        );
        session.close().await.expect("the visitor left");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_object_scan_skips_a_temporary_schema() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        // The bootstrap superuser's objects are not recorded, so only the
        // owner-column scan could see this temporary table; it skips it,
        // and the refusal is the later session check, not the object check.
        let mut session = cluster.admin_on(database.as_str()).await;
        common::execute(&mut session, "CREATE TEMPORARY TABLE scratch (id bigint)").await;

        let err = run_prepare(&maintenance, &database, &role)
            .await
            .expect_err("the superuser's session is refused");
        assert_eq!(refused_check(&err), "session");
        session.close().await.expect("the admin session closed");
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_object_scan_skips_the_members_of_an_extension() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        // A trusted extension the maintenance role installs: the extension
        // is the maintenance role's, its members the bootstrap superuser's.
        as_maintenance(&cluster, database.as_str(), "CREATE EXTENSION citext").await;
        let mut admin = cluster.admin_on(database.as_str()).await;
        let member_owners: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT pg_get_userbyid(t.typowner)::text
               FROM pg_type t
               JOIN pg_depend d ON d.classid = 'pg_type'::regclass AND d.objid = t.oid
                AND d.deptype = 'e'
               JOIN pg_extension e ON e.oid = d.refobjid AND e.extname = 'citext'",
        )
        .fetch_all(&mut admin)
        .await
        .expect("the members are readable");
        admin.close().await.expect("the admin session closed");
        assert_eq!(member_owners.len(), 1, "{member_owners:?}");
        assert_ne!(member_owners[0], cluster.maintenance_role());
        no_sessions(&cluster, database.as_str()).await;

        assert_eq!(
            run_prepare(&maintenance, &database, &role)
                .await
                .expect("the members are skipped"),
            Adoption::Reassign
        );
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("the handover succeeded");
        let mut admin = cluster.admin_on(database.as_str()).await;
        let owner: String = sqlx::query_scalar(
            "SELECT pg_get_userbyid(extowner)::text FROM pg_extension WHERE extname = 'citext'",
        )
        .fetch_one(&mut admin)
        .await
        .expect("the extension is readable");
        admin.close().await.expect("the admin session closed");
        assert_eq!(
            owner,
            role.as_str(),
            "the extension moved with the database"
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_session_that_appears_before_the_transfer_is_refused_in_its_transaction() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, password) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            "CREATE TABLE public.orders (id bigint)",
        )
        .await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;

        // Step 2's revoke waits on this lock while its session holds the
        // maintenance permit.
        let mut lock = cluster.admin().await;
        common::execute(&mut lock, "BEGIN").await;
        common::execute(&mut lock, "LOCK TABLE pg_catalog.pg_database IN SHARE MODE").await;
        let adopting = run_adopt(&maintenance, &database, &role);
        tokio::pin!(adopting);
        tokio::select! {
            outcome = &mut adopting => {
                panic!("the handover finished under the lock: {:?}", outcome.map_err(|e| e.code()));
            }
            () = async { sekvent_testing::await_until!(lock_waiters(&cluster).await > 0) } => {}
        }
        // Queued for the permit behind step 2, so ahead of step 3: the
        // permit is fair.
        let queued = maintenance.connect();
        tokio::pin!(queued);
        tokio::select! {
            biased;
            _ = &mut queued => panic!("the permit is held by step 2"),
            () = std::future::ready(()) => {}
        }
        common::execute(&mut lock, "ROLLBACK").await;
        lock.close().await.expect("the admin session closed");
        let held = tokio::select! {
            outcome = &mut adopting => {
                panic!("the handover went on past a queued permit: {:?}", outcome.map_err(|e| e.code()));
            }
            conn = &mut queued => conn.expect("the permit came after step 2"),
        };

        // Step 2 passed. A superuser session appears before step 3 runs.
        let intruder = cluster.admin_on(database.as_str()).await;
        held.close().await.expect("the permit went on to step 3");
        let err = adopting.await.expect_err("step 3's session check refuses");
        assert_eq!(refused_check(&err), "session");
        let after = state(&cluster, database.as_str(), role.as_str()).await;
        assert!(!after.public_connect, "step 2's revoke stays");
        assert_eq!(
            State {
                public_connect: true,
                ..after
            },
            before,
            "step 3 rolled back: owner, objects and memberships unchanged"
        );

        intruder.close().await.expect("the admin session closed");
        no_sessions(&cluster, database.as_str()).await;
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("the next call converges");
        tenant_signs_in(&cluster, &role, &password, database.as_str()).await;
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_default_privileges_entry_of_the_maintenance_role_stays_after_a_handover() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            "CREATE TABLE public.orders (id bigint);
             ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC;",
        )
        .await;
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("the handover succeeded");

        let mut admin = cluster.admin_on(database.as_str()).await;
        let entries: Vec<String> =
            sqlx::query_scalar("SELECT pg_get_userbyid(defaclrole)::text FROM pg_default_acl")
                .fetch_all(&mut admin)
                .await
                .expect("the default privileges are readable");
        admin.close().await.expect("the admin session closed");
        // Current behaviour, pending an owner decision: REASSIGN OWNED does
        // not move default privileges, so the entry stays the maintenance
        // role's after the handover.
        assert_eq!(entries, [cluster.maintenance_role()]);
        let after = state(&cluster, database.as_str(), role.as_str()).await;
        assert_eq!(after.owner, role.as_str());
    }))
    .await;
}

/// Objects of the candidate that an unqualified or type-inexact statement
/// would reach on a `search_path` that puts `public` first, each function
/// raising: `public."="(oid, regclass)` and `public."~"(name, name)`, the
/// comparisons the object scan once made without a `pg_catalog` operator
/// of exactly their types, and the `public.current_setting(text)` and type
/// `public.text` an unqualified read of a session's settings would reach.
/// The maintenance role creates them, so the object scan allows them as its
/// own.
const HOSTILE: &str = "
CREATE FUNCTION public.hostile_eq(oid, pg_catalog.regclass) RETURNS boolean
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'a hostile object was called'; END $$;
CREATE OPERATOR public.= (
    LEFTARG = oid, RIGHTARG = pg_catalog.regclass, FUNCTION = public.hostile_eq);
CREATE FUNCTION public.hostile_match(name, name) RETURNS boolean
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'a hostile object was called'; END $$;
CREATE OPERATOR public.~ (LEFTARG = name, RIGHTARG = name, FUNCTION = public.hostile_match);
CREATE FUNCTION public.current_setting(text) RETURNS text
    LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'a hostile object was called'; END $$;
CREATE TYPE public.text AS (hostile integer);
CREATE TABLE public.orders (id bigint)";

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn no_operator_or_function_of_the_candidate_is_called() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        as_maintenance(&cluster, database.as_str(), HOSTILE).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            &format!(
                "ALTER DATABASE {} SET search_path = public, pg_catalog",
                quote(database.as_str())
            ),
        )
        .await;
        // A session without the crate's startup options gets that path.
        let mut raw = cluster
            .connect_as(
                cluster.maintenance_role(),
                cluster.maintenance_password(),
                database.as_str(),
            )
            .await
            .expect("the maintenance role connected");
        let search_path: String =
            sqlx::query_scalar("SELECT pg_catalog.current_setting('search_path')")
                .fetch_one(&mut raw)
                .await
                .expect("the setting was read");
        assert!(search_path.starts_with("public"), "{search_path}");
        raw.close().await.expect("the session closed");
        no_sessions(&cluster, database.as_str()).await;

        assert_eq!(
            run_prepare(&maintenance, &database, &role)
                .await
                .expect("prepared without calling an object of the candidate"),
            Adoption::Reassign
        );
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("adopted without calling an object of the candidate");
        let after = state(&cluster, database.as_str(), role.as_str()).await;
        assert_eq!(after.owner, role.as_str());
        assert!(!after.public_connect);
    }))
    .await;
}

/// Every membership of the maintenance role in the tenant role is without
/// `INHERIT`, and one of them lets it switch (`SET`). The role's creation
/// may leave more than one row: the grant a role's creator receives and the
/// crate's own.
fn assert_no_inheriting_membership(state: &State) {
    assert!(
        state
            .memberships
            .iter()
            .all(|edge| edge.starts_with("false:")),
        "no membership inherits: {:?}",
        state.memberships
    );
    assert!(
        state.memberships.iter().any(|edge| edge == "false:true"),
        "the maintenance role may switch: {:?}",
        state.memberships
    );
}

/// An adoption resumed at its database step, after the tenant role made by
/// the role step gained a membership, is refused before anything changes,
/// by `prepare` and `adopt` alike: the owner, `PUBLIC`'s `CONNECT` and the
/// maintenance role's membership (`INHERIT FALSE`) stay as they were.
/// Without the membership it goes through.
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_adoption_whose_role_gained_a_membership_is_refused_and_changes_nothing() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            "CREATE TABLE public.orders (id bigint)",
        )
        .await;
        let third = cluster.create_role("third", "").await;
        let (role_quoted, third_quoted) = (quote(role.as_str()), quote(&third));
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        assert!(before.public_connect);
        assert_no_inheriting_membership(&before);
        for (grant, revoke, finding) in [
            (
                format!("GRANT {third_quoted} TO {role_quoted}"),
                format!("REVOKE {third_quoted} FROM {role_quoted}"),
                format!("it reaches role {third}"),
            ),
            (
                format!("GRANT {role_quoted} TO {third_quoted}"),
                format!("REVOKE {role_quoted} FROM {third_quoted}"),
                format!("role {third} reaches it"),
            ),
        ] {
            cluster.admin_execute("postgres", &grant).await;
            let err = run_prepare(&maintenance, &database, &role)
                .await
                .expect_err("prepare checks the role before anything else");
            assert_role_refused(&err, &role, &finding);
            let err = run_adopt(&maintenance, &database, &role)
                .await
                .expect_err("the role is checked before anything else");
            assert_role_refused(&err, &role, &finding);
            assert_eq!(
                state(&cluster, database.as_str(), role.as_str()).await,
                before,
                "nothing changed: no revoke of step 2"
            );
            cluster.admin_execute("postgres", &revoke).await;
        }
        run_adopt(&maintenance, &database, &role)
            .await
            .expect("without the membership the adoption goes through");
        let after = state(&cluster, database.as_str(), role.as_str()).await;
        assert_eq!(after.owner, role.as_str());
        assert_no_inheriting_membership(&after);
    }))
    .await;
}

/// A handover to a role that does not exist passes step 1, where a missing
/// role is no finding, and step 2, which revokes `PUBLIC`'s `CONNECT`; step
/// 3's transaction refuses it before anything else and rolls back, so the
/// maintenance role still owns the database.
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_handover_to_a_missing_role_is_refused_at_the_transfer() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let absent = cluster.names().role(common::new_tenant());
        let database = candidate(&cluster).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            "CREATE TABLE public.orders (id bigint)",
        )
        .await;
        let before = state(&cluster, database.as_str(), absent.as_str()).await;
        assert_eq!(before.owner, cluster.maintenance_role());
        assert!(before.public_connect);
        assert_eq!(before.memberships, [] as [String; 0]);

        let err = run_adopt(&maintenance, &database, &absent)
            .await
            .expect_err("step 3 refuses a missing role");
        assert_eq!(err.code(), ErrorCode::Internal, "{err:?}");
        assert_eq!(err.reason(), None);
        assert_eq!(err.message(), "internal error");
        assert_eq!(
            source(&err),
            format!("tenant role {} does not exist", absent.as_str())
        );
        let after = state(&cluster, database.as_str(), absent.as_str()).await;
        assert_eq!(
            after.owner,
            cluster.maintenance_role(),
            "the owner is unchanged"
        );
        assert!(!after.public_connect, "step 2's revoke stays");
        assert_eq!(
            State {
                public_connect: true,
                ..after
            },
            before,
            "step 3 rolled back: owner, objects and connections unchanged"
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_restrict_path_is_refused_before_the_restriction_when_the_role_changed() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let name = cluster.create_database("owned", role.as_str()).await;
        let database = DatabaseName::parse(&name).expect("a database name");
        let third = cluster.create_role("third", "").await;
        cluster
            .admin_execute(
                "postgres",
                &format!("GRANT {} TO {}", quote(&third), quote(role.as_str())),
            )
            .await;
        let before = state(&cluster, &name, role.as_str()).await;
        assert!(before.public_connect);
        let err = run_prepare(&maintenance, &database, &role)
            .await
            .expect_err("prepare checks an existing role first");
        assert_role_refused(&err, &role, &format!("it reaches role {third}"));
        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("the role is checked before the restriction");
        assert_role_refused(&err, &role, &format!("it reaches role {third}"));
        assert_eq!(
            state(&cluster, &name, role.as_str()).await,
            before,
            "nothing was restricted"
        );
        cluster
            .admin_execute(
                "postgres",
                &format!("REVOKE {} FROM {}", quote(&third), quote(role.as_str())),
            )
            .await;
        assert_eq!(
            run_prepare(&maintenance, &database, &role)
                .await
                .expect("without the membership the role passes"),
            Adoption::Restrict
        );
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_membership_that_appears_before_the_transfer_is_refused_in_its_transaction() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        as_maintenance(
            &cluster,
            database.as_str(),
            "CREATE TABLE public.orders (id bigint)",
        )
        .await;
        let third = cluster.create_role("third", "").await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;

        // Step 2's revoke waits on this lock while its session holds the
        // maintenance permit.
        let mut lock = cluster.admin().await;
        common::execute(&mut lock, "BEGIN").await;
        common::execute(&mut lock, "LOCK TABLE pg_catalog.pg_database IN SHARE MODE").await;
        let adopting = run_adopt(&maintenance, &database, &role);
        tokio::pin!(adopting);
        tokio::select! {
            outcome = &mut adopting => {
                panic!("the handover finished under the lock: {:?}", outcome.map_err(|e| e.code()));
            }
            () = async { sekvent_testing::await_until!(lock_waiters(&cluster).await > 0) } => {}
        }
        // Queued for the permit behind step 2, so ahead of step 3: the
        // permit is fair.
        let queued = maintenance.connect();
        tokio::pin!(queued);
        tokio::select! {
            biased;
            _ = &mut queued => panic!("the permit is held by step 2"),
            () = std::future::ready(()) => {}
        }
        common::execute(&mut lock, "ROLLBACK").await;
        lock.close().await.expect("the admin session closed");
        let held = tokio::select! {
            outcome = &mut adopting => {
                panic!("the handover went on past a queued permit: {:?}", outcome.map_err(|e| e.code()));
            }
            conn = &mut queued => conn.expect("the permit came after step 2"),
        };

        // Step 2 passed. The tenant role gains a membership before step 3.
        cluster
            .admin_execute(
                "postgres",
                &format!("GRANT {} TO {}", quote(&third), quote(role.as_str())),
            )
            .await;
        held.close().await.expect("the permit went on to step 3");
        let err = adopting
            .await
            .expect_err("step 3 checks the role in its transaction");
        assert_role_refused(&err, &role, &format!("it reaches role {third}"));
        let after = state(&cluster, database.as_str(), role.as_str()).await;
        assert!(!after.public_connect, "step 2's revoke stays");
        assert_eq!(
            State {
                public_connect: true,
                ..after
            },
            before,
            "step 3 rolled back: owner, objects and the membership's INHERIT FALSE unchanged"
        );
    }))
    .await;
}

/// A `CREATE` of `PUBLIC`, which the read after the restriction would
/// refuse once the database had changed hands, is refused by step 1 before
/// anything changes.
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_public_create_is_refused_before_the_handover() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT CREATE ON DATABASE {} TO PUBLIC",
                    quote(database.as_str())
                ),
            )
            .await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("PUBLIC may create schemas");
        assert_eq!(refused_check(&err), "grantee");
        assert_eq!(
            state(&cluster, database.as_str(), role.as_str()).await,
            before,
            "nothing changed"
        );
    }))
    .await;
}

/// A `PUBLIC` `CONNECT` that the owner's revoke would leave in place is one
/// another grantor made, which needs a grant option of its own; step 1
/// refuses that grantee first, so the read after the restriction never sees
/// such an entry on this path.
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_public_connect_of_another_grantor_is_refused_before_the_handover() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(Box::pin(async {
        let maintenance = cluster.maintenance();
        let (role, _) = tenant_role(&cluster, &maintenance).await;
        let database = candidate(&cluster).await;
        let grantor = quote(&cluster.create_role("grantor", "").await);
        let quoted = quote(database.as_str());
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "GRANT CONNECT ON DATABASE {quoted} TO {grantor} WITH GRANT OPTION; \
                     SET ROLE {grantor}; \
                     GRANT CONNECT ON DATABASE {quoted} TO PUBLIC; \
                     RESET ROLE"
                ),
            )
            .await;
        let before = state(&cluster, database.as_str(), role.as_str()).await;
        let err = run_adopt(&maintenance, &database, &role)
            .await
            .expect_err("the grantor is a third grantee");
        assert_eq!(refused_check(&err), "grantee");
        assert_eq!(
            state(&cluster, database.as_str(), role.as_str()).await,
            before
        );
    }))
    .await;
}
