//! The startup preflight: no tenant role reaches one of the service's own
//! databases.
//!
//! A *protected* database (the control database, the maintenance database,
//! any other the product names) is reached only by its dedicated roles, so a
//! tenant role's credentials open none of them. The product runs the
//! preflight before any migration.
//!
//! A protected database passes when `PUBLIC` holds neither `CONNECT` nor
//! `TEMPORARY` on it, no tenant role owns it or may connect to it, and every other `LOGIN` role
//! that may connect is its owner, an allowed role or a superuser. A
//! *tenant role* is any role whose name has the object-name prefix and,
//! when the maintenance role is known, any role the maintenance role
//! reaches through a membership chain other than the owner, the allowed
//! roles and the maintenance role itself. A superuser is the cluster
//! operator's and not a finding, unless its name is tenant-named.
//!
//! The maintenance session itself passes only when its `CURRENT_USER` is
//! its `session_user`. The crate names the maintenance role by the session
//! user (the reach above, the tenant-role checks) while its statements act
//! as the current user, and the two differ only when the session switched
//! role at startup (a `role` startup option, `ALTER ROLE … SET role`,
//! `ALTER DATABASE … SET role`). The rule belongs to the session, not to a
//! database: it is read once on the maintenance connection and reported
//! with the findings of the database that connection's check covers (the
//! maintenance database, or the shared one). Other pools are not held to
//! it: their session role is only the allowed role of their own database.
//!
//! The reads take a fixed number of statements per database however many
//! roles the cluster holds; the rules ([`protected_facts`], [`findings`])
//! are pure. Nothing is repaired: a finding is `FAILED_PRECONDITION` naming
//! the label, the database and the roles at fault, never a URL or a
//! credential.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use sekvent_db::to_app_error;
use sekvent_error::AppError;
use sqlx::postgres::{PgConnectOptions, PgPool};
use sqlx::{Connection, PgConnection};
use tokio::time::error::Elapsed;

use crate::ObjectNames;
use crate::cluster::privileges::{self, DatabaseGrant, DatabasePrivilege, Grantee};

/// The longest a preflight of one pool may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The oldest server the crate accepts, as `server_version_num`.
const MINIMUM_SERVER_VERSION: i32 = 160_000;

/// A database only its dedicated roles may reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protected {
    /// For the message: `control`, `maintenance`, …
    pub label: String,
    /// The database name.
    pub database: String,
    /// Roles that may connect besides the owner and superusers.
    pub allowed: Vec<String>,
}

/// A role with an effective `CONNECT` on a protected database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectingRole {
    /// The role name.
    pub name: String,
    /// `LOGIN`.
    pub login: bool,
    /// `SUPERUSER`.
    pub superuser: bool,
    /// Whether the maintenance role reaches it through a membership chain.
    pub reached: bool,
}

/// What the rules of the preflight look at for one protected database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedFacts {
    /// The owner's role name.
    pub owner: String,
    /// Its ACL, one privilege per entry.
    pub grants: Vec<DatabaseGrant>,
    /// Every `LOGIN` role that may connect, and whether it is a superuser.
    pub connecting: Vec<(String, bool)>,
    /// Every tenant role that may connect, sorted.
    pub tenants_connecting: Vec<String>,
}

/// What is wrong with a protected database.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Finding {
    /// There is no such database.
    Missing,
    /// `PUBLIC` holds this privilege.
    Public(DatabasePrivilege),
    /// This tenant role owns it.
    TenantOwner(String),
    /// This tenant role may connect.
    TenantMayConnect(String),
    /// This `LOGIN` role, neither the owner, an allowed role nor a
    /// superuser, may connect.
    RoleMayConnect(String),
    /// The maintenance session acts as `current_user`, not as the
    /// `session_user` it signed in as.
    RoleSwitched {
        /// The role it signed in as.
        session_user: String,
        /// The role its statements act as.
        current_user: String,
    },
}

/// Every role with an effective `CONNECT` on the database (`LOGIN` or not),
/// with its attributes and whether the maintenance role (`$2`, NULL when
/// unknown) reaches it. One statement, however many roles there are.
const CONNECTING_ROLES: &str = "\
WITH RECURSIVE reached(oid) AS (
    SELECT m.roleid
    FROM pg_catalog.pg_auth_members m
    JOIN pg_catalog.pg_roles r ON r.oid = m.member
    WHERE r.rolname = $2
    UNION
    SELECT m.roleid
    FROM pg_catalog.pg_auth_members m
    JOIN reached x ON m.member = x.oid
)
SELECT r.rolname::text, r.rolcanlogin, r.rolsuper,
       EXISTS (SELECT 1 FROM reached x WHERE x.oid = r.oid)
FROM pg_catalog.pg_database d
JOIN pg_catalog.pg_roles r
    ON pg_catalog.has_database_privilege(r.oid, d.oid, 'CONNECT')
WHERE d.datname = $1";

/// The server's version as `server_version_num` (`170002` for 17.2).
const SERVER_VERSION: &str = "SELECT pg_catalog.current_setting('server_version_num')::int4";

/// The session's role, the role its statements act as, its database and
/// what identifies that database across connections: its oid and the
/// server's start time in microseconds. Two sessions with the same three
/// are on one database.
const SESSION: &str = "\
SELECT session_user::text, current_user::text, d.datname::text, d.oid::int8,
       (extract(epoch FROM pg_catalog.pg_postmaster_start_time()) * 1000000)::int8
FROM pg_catalog.pg_database d
WHERE d.datname = pg_catalog.current_database()";

/// Pure: the facts of one protected database from its owner, its ACL and
/// the roles that may connect to it. `connecting` keeps the `LOGIN` roles;
/// `tenants_connecting` keeps the tenant roles, `LOGIN` or not.
pub fn protected_facts(
    owner: String,
    grants: Vec<DatabaseGrant>,
    roles: &[ConnectingRole],
    allowed: &[String],
    maintenance: Option<&str>,
    names: &ObjectNames,
) -> ProtectedFacts {
    let mut connecting: Vec<(String, bool)> = roles
        .iter()
        .filter(|role| role.login)
        .map(|role| (role.name.clone(), role.superuser))
        .collect();
    connecting.sort();
    connecting.dedup();
    let mut tenants_connecting: Vec<String> = roles
        .iter()
        .filter(|role| is_tenant_role(role, &owner, allowed, maintenance, names))
        .map(|role| role.name.clone())
        .collect();
    tenants_connecting.sort();
    tenants_connecting.dedup();
    ProtectedFacts {
        owner,
        grants,
        connecting,
        tenants_connecting,
    }
}

/// Pure: the rule of the maintenance session: its statements act as the
/// role it signed in as. A [`Finding::RoleSwitched`] when they do not.
pub fn session_findings(session_user: &str, current_user: &str) -> Vec<Finding> {
    if session_user == current_user {
        return Vec::new();
    }
    vec![Finding::RoleSwitched {
        session_user: session_user.to_owned(),
        current_user: current_user.to_owned(),
    }]
}

/// Pure: every rule the facts break, in order: `PUBLIC`'s `CONNECT` and
/// `TEMPORARY` (its `CREATE` alone opens no session and is no finding), a
/// tenant-named owner, each tenant role that may connect (the
/// owner already named as tenant owner is not repeated), then each other
/// `LOGIN` role that may connect and is neither the owner, an allowed role
/// nor a superuser.
pub fn findings(facts: &ProtectedFacts, allowed: &[String], names: &ObjectNames) -> Vec<Finding> {
    let mut found = Vec::new();
    let mut public: Vec<DatabasePrivilege> = facts
        .grants
        .iter()
        .filter(|grant| grant.grantee == Grantee::Public)
        .map(|grant| grant.privilege)
        .filter(|privilege| {
            matches!(
                privilege,
                DatabasePrivilege::Connect | DatabasePrivilege::Temporary
            )
        })
        .collect();
    public.sort();
    public.dedup();
    found.extend(public.into_iter().map(Finding::Public));

    let tenant_owner = names.has_prefix(&facts.owner);
    if tenant_owner {
        found.push(Finding::TenantOwner(facts.owner.clone()));
    }
    found.extend(
        facts
            .tenants_connecting
            .iter()
            .filter(|role| !(tenant_owner && **role == facts.owner))
            .cloned()
            .map(Finding::TenantMayConnect),
    );
    found.extend(
        facts
            .connecting
            .iter()
            .filter(|(role, superuser)| {
                !superuser
                    && *role != facts.owner
                    && !allowed.contains(role)
                    && !facts.tenants_connecting.contains(role)
            })
            .map(|(role, _)| Finding::RoleMayConnect(role.clone())),
    );
    found
}

/// The facts of `database`, read in two statements however many roles the
/// cluster holds; `None` when there is no such database.
pub async fn facts(
    conn: &mut PgConnection,
    database: &str,
    allowed: &[String],
    maintenance: Option<&str>,
    names: &ObjectNames,
) -> Result<Option<ProtectedFacts>, AppError> {
    let Some(database_facts) = privileges::database_facts(conn, database).await? else {
        return Ok(None);
    };
    let rows: Vec<(String, bool, bool, bool)> = sqlx::query_as(CONNECTING_ROLES)
        .bind(database)
        .bind(maintenance)
        .fetch_all(&mut *conn)
        .await
        .map_err(to_app_error)?;
    let roles: Vec<ConnectingRole> = rows
        .into_iter()
        .map(|(name, login, superuser, reached)| ConnectingRole {
            name,
            login,
            superuser,
            reached,
        })
        .collect();
    Ok(Some(protected_facts(
        database_facts.owner,
        database_facts.grants,
        &roles,
        allowed,
        maintenance,
        names,
    )))
}

/// `FAILED_PRECONDITION` naming the label, the database and the roles at
/// fault when `protected` breaks a rule, or when the server is older than
/// Postgres 16. Three statements, within [`TIMEOUT`]; past it
/// `UNAVAILABLE`.
pub async fn check(
    conn: &mut PgConnection,
    protected: &Protected,
    maintenance: Option<&str>,
    names: &ObjectNames,
) -> Result<(), AppError> {
    bounded(
        &protected.label,
        tokio::time::timeout(
            TIMEOUT,
            run_check(conn, protected, maintenance, names, Vec::new()),
        )
        .await,
    )
}

/// The pool's own database, reached by the pool's session role: the
/// session role is the one allowed role. Within [`TIMEOUT`].
pub async fn check_session_database(
    pool: &PgPool,
    label: &str,
    maintenance: Option<&str>,
    names: &ObjectNames,
) -> Result<(), AppError> {
    let work = async {
        let mut conn = pool.acquire().await.map_err(to_app_error)?;
        let session = read_session(&mut conn).await?;
        let protected = Protected {
            label: label.to_owned(),
            database: session.database,
            allowed: vec![session.role],
        };
        run_check(&mut conn, &protected, maintenance, names, Vec::new()).await
    };
    bounded(label, tokio::time::timeout(TIMEOUT, work).await)
}

/// The control database on its pool, then the maintenance database on a
/// connection of `maintenance` (it may be another server), closed after.
///
/// The control database allows the control pool's session role, the
/// maintenance database the maintenance role. When both are one database,
/// one check runs and allows both roles. A maintenance session whose
/// `CURRENT_USER` is not its `session_user` is a finding of the database
/// its check covers. Within one [`TIMEOUT`] for all of
/// it; the check's error wins over a failed close.
pub async fn check_control_and_maintenance(
    control: &PgPool,
    maintenance: &PgConnectOptions,
    names: &ObjectNames,
) -> Result<(), AppError> {
    let stage = Stage::new(CONTROL);
    let work = async {
        let mut control_conn = control.acquire().await.map_err(to_app_error)?;
        let control_session = read_session(&mut control_conn).await?;
        stage.set(MAINTENANCE);
        let mut maintenance_conn = PgConnection::connect_with(maintenance)
            .await
            .map_err(to_app_error)?;
        let outcome = check_both(
            &mut control_conn,
            control_session,
            &mut maintenance_conn,
            names,
            &stage,
        )
        .await;
        stage.set(MAINTENANCE);
        let closed = maintenance_conn.close().await.map_err(to_app_error);
        outcome.and(closed)
    };
    enveloped(&stage, work).await
}

const CONTROL: &str = "control";
const MAINTENANCE: &str = "maintenance";
const SHARED: &str = "control and maintenance";

/// The label of the database a preflight is working on, for the error past
/// its envelope. Shared by reference with the work, read after it is gone.
struct Stage(Mutex<&'static str>);

impl Stage {
    fn new(label: &'static str) -> Self {
        Self(Mutex::new(label))
    }

    fn set(&self, label: &'static str) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = label;
    }

    fn get(&self) -> &'static str {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `work` within [`TIMEOUT`]; past it `UNAVAILABLE` naming the database
/// `stage` last named.
async fn enveloped<T>(
    stage: &Stage,
    work: impl Future<Output = Result<T, AppError>>,
) -> Result<T, AppError> {
    let outcome = tokio::time::timeout(TIMEOUT, work).await;
    bounded(stage.get(), outcome)
}

/// One session's role, the role it acts as, its database, and the
/// database's identity.
struct Session {
    role: String,
    current_role: String,
    database: String,
    identity: (i64, i64),
}

async fn read_session(conn: &mut PgConnection) -> Result<Session, AppError> {
    let (role, current_role, database, oid, started): (String, String, String, i64, i64) =
        sqlx::query_as(SESSION)
            .fetch_one(&mut *conn)
            .await
            .map_err(to_app_error)?;
    Ok(Session {
        role,
        current_role,
        database,
        identity: (oid, started),
    })
}

/// The two checks of [`check_control_and_maintenance`], or the one check of
/// a shared database.
async fn check_both(
    control: &mut PgConnection,
    control_session: Session,
    maintenance: &mut PgConnection,
    names: &ObjectNames,
    stage: &Stage,
) -> Result<(), AppError> {
    let maintenance_session = read_session(maintenance).await?;
    let maintenance_role = maintenance_session.role.clone();
    let switched = session_findings(&maintenance_session.role, &maintenance_session.current_role);
    let shared = control_session.database == maintenance_session.database
        && control_session.identity == maintenance_session.identity;
    if shared {
        stage.set(SHARED);
        let protected = Protected {
            label: SHARED.to_owned(),
            database: control_session.database,
            allowed: vec![control_session.role, maintenance_session.role],
        };
        return run_check(
            control,
            &protected,
            Some(maintenance_role.as_str()),
            names,
            switched,
        )
        .await;
    }
    stage.set(CONTROL);
    let protected = Protected {
        label: CONTROL.to_owned(),
        database: control_session.database,
        allowed: vec![control_session.role],
    };
    run_check(
        control,
        &protected,
        Some(maintenance_role.as_str()),
        names,
        Vec::new(),
    )
    .await?;
    stage.set(MAINTENANCE);
    let protected = Protected {
        label: MAINTENANCE.to_owned(),
        database: maintenance_session.database,
        allowed: vec![maintenance_session.role],
    };
    run_check(
        maintenance,
        &protected,
        Some(maintenance_role.as_str()),
        names,
        switched,
    )
    .await
}

/// [`check`] without its envelope; `session` are the findings of the
/// maintenance session the check covers, reported first.
async fn run_check(
    conn: &mut PgConnection,
    protected: &Protected,
    maintenance: Option<&str>,
    names: &ObjectNames,
    session: Vec<Finding>,
) -> Result<(), AppError> {
    let version: i32 = sqlx::query_scalar(SERVER_VERSION)
        .fetch_one(&mut *conn)
        .await
        .map_err(to_app_error)?;
    supported_server(&protected.label, version)?;
    let mut found = session;
    found.extend(
        match facts(
            conn,
            &protected.database,
            &protected.allowed,
            maintenance,
            names,
        )
        .await?
        {
            Some(facts) => findings(&facts, &protected.allowed, names),
            None => vec![Finding::Missing],
        },
    );
    verdict(protected, &found)
}

/// Whether `role` counts as a tenant role on this database.
fn is_tenant_role(
    role: &ConnectingRole,
    owner: &str,
    allowed: &[String],
    maintenance: Option<&str>,
    names: &ObjectNames,
) -> bool {
    if names.has_prefix(&role.name) {
        return true;
    }
    maintenance.is_some_and(|maintenance| {
        role.reached
            && role.name != owner
            && role.name != maintenance
            && !allowed.contains(&role.name)
    })
}

/// Postgres 16 or later (`server_version_num` 160000 and up).
fn supported_server(label: &str, version: i32) -> Result<(), AppError> {
    if version >= MINIMUM_SERVER_VERSION {
        return Ok(());
    }
    Err(AppError::failed_precondition(format!(
        "preflight of the {label} database: the server reports version {version} \
         (server_version_num); Postgres 16 or later is required"
    )))
}

/// The envelope's outcome: past [`TIMEOUT`] is `UNAVAILABLE`.
fn bounded<T>(label: &str, outcome: Result<Result<T, AppError>, Elapsed>) -> Result<T, AppError> {
    outcome.unwrap_or_else(|_| {
        Err(AppError::unavailable(format!(
            "the preflight of the {label} database did not finish within {} s",
            TIMEOUT.as_secs()
        )))
    })
}

/// `FAILED_PRECONDITION` naming the label, the database and every finding.
fn verdict(protected: &Protected, found: &[Finding]) -> Result<(), AppError> {
    if found.is_empty() {
        return Ok(());
    }
    let described: Vec<String> = found.iter().map(describe).collect();
    Err(AppError::failed_precondition(format!(
        "preflight of the {} database {}: {}",
        protected.label,
        protected.database,
        described.join("; ")
    )))
}

pub(crate) fn describe(finding: &Finding) -> String {
    match finding {
        Finding::Missing => "it does not exist".to_owned(),
        Finding::Public(privilege) => format!("PUBLIC holds {}", privilege.sql_name()),
        Finding::TenantOwner(role) => format!("tenant role {role} owns it"),
        Finding::TenantMayConnect(role) => format!("tenant role {role} may connect"),
        Finding::RoleMayConnect(role) => format!(
            "role {role} may connect and is neither its owner, an allowed role nor a superuser"
        ),
        Finding::RoleSwitched {
            session_user,
            current_user,
        } => format!(
            "the maintenance session runs as role {current_user}, not as its session role \
             {session_user}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    const OWNER: &str = "orders_owner";
    const MAINTENANCE_ROLE: &str = "orders_maintenance";

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("a valid prefix")
    }

    fn tenant(suffix: &str) -> String {
        format!("app_t_{suffix}")
    }

    fn connecting(name: &str, login: bool, superuser: bool, reached: bool) -> ConnectingRole {
        ConnectingRole {
            name: name.to_owned(),
            login,
            superuser,
            reached,
        }
    }

    fn public(privilege: DatabasePrivilege) -> DatabaseGrant {
        DatabaseGrant {
            grantee: Grantee::Public,
            privilege,
            grantable: false,
        }
    }

    fn to_role(role: &str, privilege: DatabasePrivilege) -> DatabaseGrant {
        DatabaseGrant {
            grantee: Grantee::Role(role.to_owned()),
            privilege,
            grantable: false,
        }
    }

    fn facts_of(owner: &str, roles: &[ConnectingRole], allowed: &[String]) -> ProtectedFacts {
        protected_facts(
            owner.to_owned(),
            vec![to_role(owner, DatabasePrivilege::Connect)],
            roles,
            allowed,
            Some(MAINTENANCE_ROLE),
            &names(),
        )
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn connecting_keeps_the_login_roles_sorted_with_their_superuser_flag() {
        let roles = [
            connecting("orders_reader", true, false, false),
            connecting("postgres", true, true, false),
            connecting("pg_monitor", false, false, false),
            connecting(OWNER, true, false, false),
        ];
        let facts = facts_of(OWNER, &roles, &[]);
        assert_eq!(
            facts.connecting,
            [
                ("orders_owner".to_owned(), false),
                ("orders_reader".to_owned(), false),
                ("postgres".to_owned(), true),
            ]
        );
        assert_eq!(facts.tenants_connecting, [] as [String; 0]);
        assert_eq!(facts.owner, OWNER);
        assert_eq!(facts.grants, [to_role(OWNER, DatabasePrivilege::Connect)]);
    }

    #[test]
    fn a_prefixed_role_is_a_tenant_role_with_or_without_login() {
        let nologin = tenant("0123");
        let login = tenant("4567");
        let roles = [
            connecting(&login, true, false, false),
            connecting(&nologin, false, false, false),
        ];
        let facts = facts_of(OWNER, &roles, &[]);
        assert_eq!(facts.tenants_connecting, [nologin.clone(), login.clone()]);
        assert_eq!(facts.connecting, [(login, false)]);
    }

    #[test]
    fn a_prefixed_role_stays_a_tenant_role_when_allowed_or_owning() {
        let role = tenant("0123");
        let roles = [connecting(&role, true, false, false)];
        let facts = facts_of(&role, &roles, std::slice::from_ref(&role));
        assert_eq!(facts.tenants_connecting, [role]);
    }

    #[test]
    fn a_role_the_maintenance_role_reaches_is_a_tenant_role() {
        let roles = [connecting("orders_tenant_x", false, false, true)];
        let facts = facts_of(OWNER, &roles, &[]);
        assert_eq!(facts.tenants_connecting, ["orders_tenant_x"]);
    }

    #[test]
    fn reach_counts_only_when_the_maintenance_role_is_known() {
        let roles = [connecting("orders_tenant_x", true, false, true)];
        let facts = protected_facts(OWNER.to_owned(), Vec::new(), &roles, &[], None, &names());
        assert_eq!(facts.tenants_connecting, [] as [String; 0]);
        assert_eq!(facts.connecting, [("orders_tenant_x".to_owned(), false)]);
    }

    #[test]
    fn reach_spares_the_owner_the_allowed_roles_and_the_maintenance_role() {
        let roles = [
            connecting(OWNER, true, false, true),
            connecting("orders_service", true, false, true),
            connecting(MAINTENANCE_ROLE, true, false, true),
        ];
        let facts = facts_of(OWNER, &roles, &strings(&["orders_service"]));
        assert_eq!(facts.tenants_connecting, [] as [String; 0]);
    }

    #[test]
    fn a_clean_database_has_no_finding() {
        let roles = [
            connecting(OWNER, true, false, false),
            connecting("orders_service", true, false, false),
            connecting("postgres", true, true, false),
            connecting("pg_database_owner", false, false, false),
        ];
        let allowed = strings(&["orders_service"]);
        let facts = facts_of(OWNER, &roles, &allowed);
        assert_eq!(findings(&facts, &allowed, &names()), [] as [Finding; 0]);
    }

    #[test]
    fn public_connect_and_temporary_are_findings_once_each() {
        let mut facts = facts_of(OWNER, &[], &[]);
        facts.grants.extend([
            public(DatabasePrivilege::Temporary),
            public(DatabasePrivilege::Connect),
            DatabaseGrant {
                grantable: true,
                ..public(DatabasePrivilege::Connect)
            },
            public(DatabasePrivilege::Create),
        ]);
        assert_eq!(
            findings(&facts, &[], &names()),
            [
                Finding::Public(DatabasePrivilege::Connect),
                Finding::Public(DatabasePrivilege::Temporary),
            ]
        );
    }

    #[test]
    fn of_public_privileges_only_connect_and_temporary_are_findings() {
        for (privilege, expected) in [
            (
                DatabasePrivilege::Connect,
                vec![Finding::Public(DatabasePrivilege::Connect)],
            ),
            (
                DatabasePrivilege::Temporary,
                vec![Finding::Public(DatabasePrivilege::Temporary)],
            ),
            (DatabasePrivilege::Create, Vec::new()),
        ] {
            let mut facts = facts_of(OWNER, &[], &[]);
            facts.grants.push(public(privilege));
            assert_eq!(findings(&facts, &[], &names()), expected, "{privilege:?}");
        }
    }

    #[test]
    fn public_temporary_alone_is_a_finding() {
        let mut facts = facts_of(OWNER, &[], &[]);
        facts.grants.push(public(DatabasePrivilege::Temporary));
        assert_eq!(
            findings(&facts, &[], &names()),
            [Finding::Public(DatabasePrivilege::Temporary)]
        );
    }

    #[test]
    fn a_grant_to_a_role_is_judged_by_who_connects_not_by_the_acl() {
        let mut facts = facts_of(OWNER, &[], &[]);
        facts
            .grants
            .push(to_role("orders_service", DatabasePrivilege::Create));
        assert_eq!(findings(&facts, &[], &names()), [] as [Finding; 0]);
    }

    #[test]
    fn a_tenant_owner_is_one_finding_even_though_it_connects() {
        let owner = tenant("0123");
        let roles = [connecting(&owner, true, false, false)];
        let facts = facts_of(&owner, &roles, &[]);
        assert_eq!(
            findings(&facts, &[], &names()),
            [Finding::TenantOwner(owner)]
        );
    }

    #[test]
    fn a_tenant_role_that_may_connect_is_a_finding() {
        let role = tenant("0123");
        let roles = [
            connecting(OWNER, true, false, false),
            connecting(&role, false, false, false),
        ];
        let facts = facts_of(OWNER, &roles, &[]);
        assert_eq!(
            findings(&facts, &[], &names()),
            [Finding::TenantMayConnect(role)]
        );
    }

    #[test]
    fn an_allowed_tenant_named_role_is_still_a_finding() {
        let role = tenant("0123");
        let allowed = vec![role.clone()];
        let roles = [connecting(&role, true, false, false)];
        let facts = facts_of(OWNER, &roles, &allowed);
        assert_eq!(
            findings(&facts, &allowed, &names()),
            [Finding::TenantMayConnect(role)]
        );
    }

    #[test]
    fn a_reached_login_role_is_named_once_as_a_tenant_role() {
        let roles = [connecting("orders_tenant_x", true, false, true)];
        let facts = facts_of(OWNER, &roles, &[]);
        assert_eq!(
            findings(&facts, &[], &names()),
            [Finding::TenantMayConnect("orders_tenant_x".to_owned())]
        );
    }

    #[test]
    fn another_login_role_that_may_connect_is_a_finding() {
        let roles = [
            connecting(OWNER, true, false, false),
            connecting("billing_reader", true, false, false),
        ];
        let facts = facts_of(OWNER, &roles, &[]);
        assert_eq!(
            findings(&facts, &[], &names()),
            [Finding::RoleMayConnect("billing_reader".to_owned())]
        );
    }

    #[test]
    fn the_owner_an_allowed_role_and_a_superuser_may_connect() {
        let roles = [
            connecting(OWNER, true, false, false),
            connecting("orders_service", true, false, false),
            connecting("cluster_admin", true, true, false),
        ];
        let allowed = strings(&["orders_service"]);
        let facts = facts_of(OWNER, &roles, &allowed);
        assert_eq!(findings(&facts, &allowed, &names()), [] as [Finding; 0]);
        // The allowed list passed to the rules decides, too.
        assert_eq!(
            findings(&facts, &[], &names()),
            [Finding::RoleMayConnect("orders_service".to_owned())]
        );
    }

    #[test]
    fn a_tenant_named_superuser_is_a_finding() {
        let role = tenant("0123");
        let roles = [
            connecting(OWNER, true, false, false),
            connecting(&role, true, true, false),
        ];
        let facts = facts_of(OWNER, &roles, &[]);
        assert_eq!(
            findings(&facts, &[], &names()),
            [Finding::TenantMayConnect(role)]
        );
    }

    #[test]
    fn findings_come_in_a_fixed_order() {
        let owner = tenant("aaaa");
        let other = tenant("bbbb");
        let roles = [
            connecting("billing_reader", true, false, false),
            connecting(&other, true, false, false),
            connecting(&owner, true, false, false),
        ];
        let mut facts = facts_of(&owner, &roles, &[]);
        facts.grants.push(public(DatabasePrivilege::Connect));
        assert_eq!(
            findings(&facts, &[], &names()),
            [
                Finding::Public(DatabasePrivilege::Connect),
                Finding::TenantOwner(owner),
                Finding::TenantMayConnect(other),
                Finding::RoleMayConnect("billing_reader".to_owned()),
            ]
        );
    }

    #[test]
    fn a_maintenance_session_acting_as_another_role_is_a_finding() {
        assert_eq!(
            session_findings(MAINTENANCE_ROLE, MAINTENANCE_ROLE),
            [] as [Finding; 0]
        );
        let found = session_findings(MAINTENANCE_ROLE, "orders_reader");
        assert_eq!(
            found,
            [Finding::RoleSwitched {
                session_user: MAINTENANCE_ROLE.to_owned(),
                current_user: "orders_reader".to_owned(),
            }]
        );
        let protected = Protected {
            label: MAINTENANCE.to_owned(),
            database: "orders_upkeep".to_owned(),
            allowed: vec![MAINTENANCE_ROLE.to_owned()],
        };
        let error = verdict(&protected, &found).unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), None);
        assert_eq!(
            error.message(),
            "preflight of the maintenance database orders_upkeep: the maintenance session \
             runs as role orders_reader, not as its session role orders_maintenance"
        );
    }

    #[test]
    fn postgres_older_than_16_is_refused() {
        let error = supported_server(CONTROL, 150_008).unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), None);
        assert!(error.message().contains("150008"), "{}", error.message());
        assert!(error.message().contains("control"), "{}", error.message());
        assert!(supported_server(CONTROL, 159_999).is_err());
        assert!(supported_server(CONTROL, 160_000).is_ok());
        assert!(supported_server(CONTROL, 170_002).is_ok());
    }

    #[tokio::test]
    async fn an_elapsed_envelope_is_unavailable() {
        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        let error = bounded::<()>(MAINTENANCE, Err(elapsed)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), None);
        assert!(
            error.message().contains("maintenance"),
            "{}",
            error.message()
        );
        assert!(error.message().contains("30 s"), "{}", error.message());
        assert_eq!(bounded(CONTROL, Ok(Ok(7))).unwrap(), 7);
        let inner = bounded::<()>(CONTROL, Ok(Err(AppError::unavailable("busy")))).unwrap_err();
        assert_eq!(inner.message(), "busy");
    }

    #[tokio::test(start_paused = true)]
    async fn an_elapsed_envelope_names_the_database_being_worked_on() {
        let stage = Stage::new(CONTROL);
        let error = enveloped::<()>(&stage, async {
            stage.set(MAINTENANCE);
            std::future::pending().await
        })
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(
            error.message(),
            "the preflight of the maintenance database did not finish within 30 s"
        );

        let stage = Stage::new(CONTROL);
        let error = enveloped::<()>(&stage, std::future::pending())
            .await
            .unwrap_err();
        assert_eq!(
            error.message(),
            "the preflight of the control database did not finish within 30 s"
        );

        let stage = Stage::new(CONTROL);
        let error = enveloped::<()>(&stage, async {
            stage.set(SHARED);
            std::future::pending().await
        })
        .await
        .unwrap_err();
        assert!(
            error.message().contains("control and maintenance database"),
            "{}",
            error.message()
        );
        let finished = enveloped(&stage, async { Ok(5) }).await.unwrap();
        assert_eq!(finished, 5);
    }

    #[test]
    fn the_verdict_names_the_label_the_database_and_every_finding() {
        let protected = Protected {
            label: CONTROL.to_owned(),
            database: "orders_control".to_owned(),
            allowed: Vec::new(),
        };
        assert!(verdict(&protected, &[]).is_ok());
        let error = verdict(
            &protected,
            &[
                Finding::Public(DatabasePrivilege::Temporary),
                Finding::TenantOwner(tenant("0123")),
                Finding::TenantMayConnect(tenant("4567")),
                Finding::RoleMayConnect("billing_reader".to_owned()),
            ],
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), None);
        let message = error.message();
        assert!(
            message.contains("control database orders_control"),
            "{message}"
        );
        assert!(message.contains("PUBLIC holds TEMPORARY"), "{message}");
        assert!(
            message.contains("tenant role app_t_0123 owns it"),
            "{message}"
        );
        assert!(
            message.contains("tenant role app_t_4567 may connect"),
            "{message}"
        );
        assert!(
            message.contains("role billing_reader may connect"),
            "{message}"
        );
    }

    #[test]
    fn a_missing_database_is_named_as_missing() {
        let protected = Protected {
            label: MAINTENANCE.to_owned(),
            database: "orders_maintenance".to_owned(),
            allowed: Vec::new(),
        };
        let error = verdict(&protected, &[Finding::Missing]).unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert!(
            error.message().contains("does not exist"),
            "{}",
            error.message()
        );
        assert!(
            error.message().contains("maintenance"),
            "{}",
            error.message()
        );
    }
}
