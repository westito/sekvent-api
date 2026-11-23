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
//! `ALTER DATABASE … SET role`). The maintenance role itself (the session
//! user) passes only when it holds none of `SUPERUSER`, `REPLICATION` and
//! `BYPASSRLS`, and when it reaches, through a membership chain of any
//! edges whatever their options, no superuser and none of
//! `pg_execute_server_program`, `pg_read_server_files` and
//! `pg_write_server_files`. These rules belong to the session, not to a
//! database: they are read once on the maintenance connection and
//! reported, the role switch first, then the attributes, then the reached
//! roles, before the findings of the database that connection's check
//! covers (the maintenance database, or the shared one). Other pools are
//! not held to them: their session role is only the allowed role of their
//! own database.
//!
//! The preflight runs on the product's own connections, whose
//! `search_path` is the database's: a `public."="(name, text)` there would
//! outrank `pg_catalog`'s on a path that puts `public` first. Every
//! comparison therefore names `pg_catalog`'s operator
//! (`OPERATOR(pg_catalog.=)`), a list or a subquery is compared with
//! `= ANY`, and every cast and function is qualified, so no object of the
//! database is called. The preflight does not read back the maintenance
//! session's startup settings (the maintenance connection does, on each
//! session it opens).
//!
//! The reads take a fixed number of statements per database however many
//! roles the cluster holds; the rules ([`protected_facts`], [`findings`],
//! [`session_findings`], [`maintenance_role_findings`],
//! [`maintenance_reach_findings`]) are pure. Nothing is repaired: a finding
//! is `FAILED_PRECONDITION` naming the label, the database and the roles at
//! fault, never a URL or a credential.

use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use sekvent_db::to_app_error;
use sekvent_error::AppError;
use sqlx::postgres::{PgConnectOptions, PgPool};
use sqlx::{Connection, PgConnection};
use tokio::time::error::Elapsed;

use crate::ObjectNames;
use crate::cluster::privileges::{self, DatabaseGrant, DatabasePrivilege, Grantee, RoleAttributes};

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
    /// The maintenance role holds this attribute (its SQL name:
    /// `SUPERUSER`, `REPLICATION` or `BYPASSRLS`).
    MaintenanceAttribute(&'static str),
    /// The maintenance role reaches this role, a superuser or one of
    /// `pg_execute_server_program`, `pg_read_server_files` and
    /// `pg_write_server_files`, through a membership chain.
    MaintenanceReaches(String),
}

/// The finding's description, as the preflight's error names it.
impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&describe(self))
    }
}

/// Every role with an effective `CONNECT` on the database (`LOGIN` or not),
/// with its attributes and whether the maintenance role (`$2`, NULL when
/// unknown) reaches it. One statement, however many roles there are.
const CONNECTING_ROLES: &str = "\
WITH RECURSIVE reached(oid) AS (
    SELECT m.roleid
    FROM pg_catalog.pg_auth_members m
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) m.member
    WHERE r.rolname OPERATOR(pg_catalog.=) $2
    UNION
    SELECT m.roleid
    FROM pg_catalog.pg_auth_members m
    JOIN reached x ON m.member OPERATOR(pg_catalog.=) x.oid
)
SELECT r.rolname::pg_catalog.text, r.rolcanlogin, r.rolsuper,
       EXISTS (SELECT 1 FROM reached x WHERE x.oid OPERATOR(pg_catalog.=) r.oid)
FROM pg_catalog.pg_database d
JOIN pg_catalog.pg_roles r
    ON pg_catalog.has_database_privilege(r.oid, d.oid, 'CONNECT')
WHERE d.datname OPERATOR(pg_catalog.=) $1";

/// The server's version as `server_version_num` (`170002` for 17.2).
const SERVER_VERSION: &str =
    "SELECT pg_catalog.current_setting('server_version_num')::pg_catalog.int4";

/// The session's role, the role its statements act as, its database and
/// what identifies that database across connections: its oid and the
/// server's start time in microseconds. Two sessions with the same three
/// are on one database. The product is `numeric` by `numeric` with
/// `pg_catalog`'s operator, named.
const SESSION: &str = "\
SELECT session_user::pg_catalog.text, current_user::pg_catalog.text,
       d.datname::pg_catalog.text, d.oid::pg_catalog.int8,
       (extract(epoch FROM pg_catalog.pg_postmaster_start_time())
           OPERATOR(pg_catalog.*) 1000000::pg_catalog.numeric)::pg_catalog.int8
FROM pg_catalog.pg_database d
WHERE d.datname OPERATOR(pg_catalog.=) pg_catalog.current_database()";

/// The attributes of the session's role, the role it signed in as.
const SESSION_ROLE_ATTRIBUTES: &str = "\
SELECT r.rolsuper, r.rolinherit, r.rolcreaterole, r.rolcreatedb,
       r.rolcanlogin, r.rolreplication, r.rolbypassrls
FROM pg_catalog.pg_roles r
WHERE r.rolname OPERATOR(pg_catalog.=) session_user";

/// Every role the session's role (`session_user`) reaches through a
/// membership chain, any edge whatever its options, that is a superuser or
/// one of the predefined roles that run the server's programs or read or
/// write its files.
const SESSION_ROLE_REACH: &str = "\
WITH RECURSIVE reached(oid) AS (
    SELECT m.roleid
    FROM pg_catalog.pg_auth_members m
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) m.member
    WHERE r.rolname OPERATOR(pg_catalog.=) session_user
    UNION
    SELECT m.roleid
    FROM pg_catalog.pg_auth_members m
    JOIN reached x ON m.member OPERATOR(pg_catalog.=) x.oid
)
SELECT r.rolname::pg_catalog.text
FROM reached x
JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) x.oid
WHERE r.rolsuper
   OR r.rolname OPERATOR(pg_catalog.=) ANY (ARRAY[
       'pg_execute_server_program'::pg_catalog.name,
       'pg_read_server_files'::pg_catalog.name,
       'pg_write_server_files'::pg_catalog.name
   ])";

/// Every catalog statement of this module, by name, for the scan of
/// `cluster::tests`.
#[cfg(test)]
pub(super) const STATEMENTS: [(&str, &str); 5] = [
    ("CONNECTING_ROLES", CONNECTING_ROLES),
    ("SERVER_VERSION", SERVER_VERSION),
    ("SESSION", SESSION),
    ("SESSION_ROLE_ATTRIBUTES", SESSION_ROLE_ATTRIBUTES),
    ("SESSION_ROLE_REACH", SESSION_ROLE_REACH),
];

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

/// Pure: the rule of the maintenance role's own attributes: it holds none
/// of `SUPERUSER`, `REPLICATION` and `BYPASSRLS`. A
/// [`Finding::MaintenanceAttribute`] for each it holds, in that order.
pub fn maintenance_role_findings(attributes: &RoleAttributes) -> Vec<Finding> {
    [
        (attributes.superuser, "SUPERUSER"),
        (attributes.replication, "REPLICATION"),
        (attributes.bypass_rls, "BYPASSRLS"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(Finding::MaintenanceAttribute(name)))
    .collect()
}

/// Pure: the rule of the maintenance role's reach: it reaches no superuser
/// and none of `pg_execute_server_program`, `pg_read_server_files` and
/// `pg_write_server_files`. A [`Finding::MaintenanceReaches`] for each role
/// of `reached` (what [`session_role_reach`] read), in the given order.
pub fn maintenance_reach_findings(reached: &[String]) -> Vec<Finding> {
    reached
        .iter()
        .cloned()
        .map(Finding::MaintenanceReaches)
        .collect()
}

/// The roles the session's role (`session_user`) reaches through a
/// membership chain, any edge whatever its options, that are superusers or
/// one of `pg_execute_server_program`, `pg_read_server_files` and
/// `pg_write_server_files`; sorted by name, each once. One statement.
pub async fn session_role_reach(conn: &mut PgConnection) -> Result<Vec<String>, AppError> {
    let mut reached: Vec<String> = sqlx::query_scalar(SESSION_ROLE_REACH)
        .fetch_all(&mut *conn)
        .await
        .map_err(to_app_error)?;
    reached.sort();
    reached.dedup();
    Ok(reached)
}

/// The attributes of the session's role (`session_user`, the role it signed
/// in as, whatever role it acts as), in one statement.
pub async fn session_role_attributes(conn: &mut PgConnection) -> Result<RoleAttributes, AppError> {
    let (superuser, inherit, create_role, create_db, login, replication, bypass_rls): (
        bool,
        bool,
        bool,
        bool,
        bool,
        bool,
        bool,
    ) = sqlx::query_as(SESSION_ROLE_ATTRIBUTES)
        .fetch_one(&mut *conn)
        .await
        .map_err(to_app_error)?;
    Ok(RoleAttributes {
        superuser,
        inherit,
        create_role,
        create_db,
        login,
        replication,
        bypass_rls,
    })
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
/// `CURRENT_USER` is not its `session_user`, a maintenance role that holds
/// `SUPERUSER`, `REPLICATION` or `BYPASSRLS`, and each role of
/// [`session_role_reach`] are findings of the database its check covers,
/// in that order and before the database's own. The control database is
/// checked first: a reached superuser may connect to it and is a tenant
/// role there, so on separate databases its finding is the control
/// database's. Within one [`TIMEOUT`] for all of it; the check's error wins
/// over a failed close.
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
    let mut switched =
        session_findings(&maintenance_session.role, &maintenance_session.current_role);
    let attributes = session_role_attributes(maintenance).await?;
    switched.extend(maintenance_role_findings(&attributes));
    let reached = session_role_reach(maintenance).await?;
    switched.extend(maintenance_reach_findings(&reached));
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
        Finding::MaintenanceAttribute(attribute) => {
            format!("the maintenance role holds {attribute}")
        }
        Finding::MaintenanceReaches(role) => format!("the maintenance role reaches role {role}"),
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;
    use crate::cluster::testing;

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
    fn a_maintenance_role_without_widening_attributes_has_no_finding() {
        let usual = RoleAttributes {
            inherit: true,
            create_role: true,
            create_db: true,
            login: true,
            ..RoleAttributes::default()
        };
        assert_eq!(maintenance_role_findings(&usual), [] as [Finding; 0]);
        assert_eq!(
            maintenance_role_findings(&RoleAttributes::default()),
            [] as [Finding; 0]
        );
    }

    #[test]
    fn each_widening_attribute_of_the_maintenance_role_is_a_finding_in_order() {
        type Case = (fn(&mut RoleAttributes), &'static str);
        let cases: [Case; 3] = [
            (|a| a.superuser = true, "SUPERUSER"),
            (|a| a.replication = true, "REPLICATION"),
            (|a| a.bypass_rls = true, "BYPASSRLS"),
        ];
        for (set, name) in cases {
            let mut attributes = RoleAttributes::default();
            set(&mut attributes);
            assert_eq!(
                maintenance_role_findings(&attributes),
                [Finding::MaintenanceAttribute(name)],
                "{name}"
            );
        }
        let all = RoleAttributes {
            superuser: true,
            replication: true,
            bypass_rls: true,
            ..RoleAttributes::default()
        };
        assert_eq!(
            maintenance_role_findings(&all),
            [
                Finding::MaintenanceAttribute("SUPERUSER"),
                Finding::MaintenanceAttribute("REPLICATION"),
                Finding::MaintenanceAttribute("BYPASSRLS"),
            ]
        );
    }

    #[test]
    fn a_maintenance_attribute_is_described_and_named_by_the_verdict() {
        let finding = Finding::MaintenanceAttribute("REPLICATION");
        assert_eq!(describe(&finding), "the maintenance role holds REPLICATION");
        let protected = Protected {
            label: MAINTENANCE.to_owned(),
            database: "orders_upkeep".to_owned(),
            allowed: vec![MAINTENANCE_ROLE.to_owned()],
        };
        let found = [
            Finding::RoleSwitched {
                session_user: MAINTENANCE_ROLE.to_owned(),
                current_user: "orders_reader".to_owned(),
            },
            finding,
        ];
        let error = verdict(&protected, &found).unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(
            error.message(),
            "preflight of the maintenance database orders_upkeep: the maintenance session \
             runs as role orders_reader, not as its session role orders_maintenance; the \
             maintenance role holds REPLICATION"
        );
    }

    /// The operand types of the raising `=` operators of [`hostile_equals`]:
    /// every pair the reads compare.
    const HOSTILE_TYPES: [(&str, &str); 4] = [
        ("pg_catalog.name", "pg_catalog.text"),
        ("pg_catalog.name", "pg_catalog.name"),
        ("pg_catalog.oid", "pg_catalog.oid"),
        ("pg_catalog.text", "pg_catalog.text"),
    ];

    /// A raising `public."="` of exactly each pair of [`HOSTILE_TYPES`]: on
    /// a `search_path` that puts `public` first, Postgres resolves an
    /// unnamed `=` of those types to these, not to `pg_catalog`'s.
    fn hostile_equals(_database: &str) -> String {
        HOSTILE_TYPES
            .iter()
            .enumerate()
            .map(|(n, (left, right))| {
                format!(
                    "CREATE FUNCTION public.hostile_eq_{n}({left}, {right}) RETURNS boolean \
                     {raises}; \
                     CREATE OPERATOR public.= (LEFTARG = {left}, RIGHTARG = {right}, \
                     FUNCTION = public.hostile_eq_{n})",
                    raises = testing::RAISES
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    #[allow(clippy::too_many_lines)]
    async fn the_reads_call_no_operator_of_the_database() {
        let Some((options, database)) = testing::public_first(hostile_equals).await else {
            return;
        };
        tokio::time::timeout(testing::SCENARIO_TIMEOUT, async {
            // A raw session: the database's own search_path, public first.
            let mut conn = PgConnection::connect_with(&options)
                .await
                .expect("the superuser connected");
            let search_path: String =
                sqlx::query_scalar("SELECT pg_catalog.current_setting('search_path')")
                    .fetch_one(&mut conn)
                    .await
                    .expect("the setting was read");
            assert!(search_path.starts_with("public"), "{search_path}");
            for (left, right) in HOSTILE_TYPES {
                let unnamed = format!("SELECT '1'::{left} = '1'::{right}");
                let error = sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(unnamed))
                    .fetch_one(&mut conn)
                    .await
                    .expect_err("an unnamed = of these types is public's");
                assert!(
                    error.to_string().contains("a hostile object was called"),
                    "{left} = {right}: {error}"
                );
            }

            let user = options.get_username().to_owned();
            let read = facts(&mut conn, &database, &[], Some(user.as_str()), &names())
                .await
                .expect("the facts were read without calling public's =")
                .expect("the database exists");
            assert!(
                read.connecting.contains(&(user.clone(), true)),
                "{:?}",
                read.connecting
            );
            let attributes = session_role_attributes(&mut conn)
                .await
                .expect("the attributes were read without calling public's =");
            assert!(attributes.superuser);
            let reached = session_role_reach(&mut conn)
                .await
                .expect("the reach was read without calling public's =");
            assert!(!reached.contains(&user), "{reached:?}");
            let session = read_session(&mut conn)
                .await
                .expect("the session was read without calling public's =");
            assert_eq!(session.database, database);
            assert_eq!(session.role, user);

            let role = privileges::role_facts(&mut conn, &user)
                .await
                .expect("the role was read without calling public's =")
                .expect("the session's role exists");
            assert_eq!(role.name, user);
            let database_facts = privileges::database_facts(&mut conn, &database)
                .await
                .expect("the database was read without calling public's =")
                .expect("the database exists");
            assert_eq!(database_facts.name, database);
            assert_eq!(
                privileges::role_may_connect(&mut conn, &user, &database)
                    .await
                    .expect("the CONNECT was read without calling public's ="),
                Some(true)
            );
            let connecting = privileges::roles_that_may_connect(&mut conn, &database)
                .await
                .expect("the roles were read without calling public's =")
                .expect("the database exists");
            assert!(connecting.contains(&user), "{connecting:?}");
            assert_eq!(
                privileges::public_may_connect(&mut conn, &database)
                    .await
                    .expect("PUBLIC's CONNECT was read without calling public's ="),
                Some(true)
            );

            // The whole check runs every read and reports the database's
            // default ACL, not an error of a called object.
            let protected = Protected {
                label: CONTROL.to_owned(),
                database: database.clone(),
                allowed: Vec::new(),
            };
            let error = check(&mut conn, &protected, Some(user.as_str()), &names())
                .await
                .expect_err("PUBLIC may connect to a database of the default ACL");
            assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{error:?}");
            assert!(
                error.message().contains("PUBLIC holds CONNECT"),
                "{}",
                error.message()
            );
            conn.close().await.expect("the session closed");
        })
        .await
        .expect("the scenario finished within its 30 s guard");
    }

    #[test]
    fn each_role_the_maintenance_role_reaches_is_a_finding_in_order() {
        assert_eq!(maintenance_reach_findings(&[]), [] as [Finding; 0]);
        let reached = strings(&["pg_read_server_files", "cluster_root"]);
        assert_eq!(
            maintenance_reach_findings(&reached),
            [
                Finding::MaintenanceReaches("pg_read_server_files".to_owned()),
                Finding::MaintenanceReaches("cluster_root".to_owned()),
            ]
        );
    }

    #[test]
    fn the_session_findings_come_before_the_databases_own() {
        let protected = Protected {
            label: MAINTENANCE.to_owned(),
            database: "orders_upkeep".to_owned(),
            allowed: vec![MAINTENANCE_ROLE.to_owned()],
        };
        let found = [
            Finding::RoleSwitched {
                session_user: MAINTENANCE_ROLE.to_owned(),
                current_user: "orders_reader".to_owned(),
            },
            Finding::MaintenanceAttribute("BYPASSRLS"),
            Finding::MaintenanceReaches("pg_write_server_files".to_owned()),
            Finding::MaintenanceReaches("cluster_root".to_owned()),
            Finding::Public(DatabasePrivilege::Connect),
        ];
        let error = verdict(&protected, &found).unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), None);
        assert_eq!(
            error.message(),
            "preflight of the maintenance database orders_upkeep: the maintenance session \
             runs as role orders_reader, not as its session role orders_maintenance; the \
             maintenance role holds BYPASSRLS; the maintenance role reaches role \
             pg_write_server_files; the maintenance role reaches role cluster_root; PUBLIC \
             holds CONNECT"
        );
    }

    #[test]
    fn every_finding_displays_its_description() {
        let all = [
            (Finding::Missing, "it does not exist"),
            (
                Finding::Public(DatabasePrivilege::Temporary),
                "PUBLIC holds TEMPORARY",
            ),
            (
                Finding::TenantOwner("app_t_0123".to_owned()),
                "tenant role app_t_0123 owns it",
            ),
            (
                Finding::TenantMayConnect("app_t_0123".to_owned()),
                "tenant role app_t_0123 may connect",
            ),
            (
                Finding::RoleMayConnect("billing_reader".to_owned()),
                "role billing_reader may connect and is neither its owner, an allowed role nor \
                 a superuser",
            ),
            (
                Finding::RoleSwitched {
                    session_user: MAINTENANCE_ROLE.to_owned(),
                    current_user: "orders_reader".to_owned(),
                },
                "the maintenance session runs as role orders_reader, not as its session role \
                 orders_maintenance",
            ),
            (
                Finding::MaintenanceAttribute("SUPERUSER"),
                "the maintenance role holds SUPERUSER",
            ),
            (
                Finding::MaintenanceReaches("pg_execute_server_program".to_owned()),
                "the maintenance role reaches role pg_execute_server_program",
            ),
        ];
        for (finding, text) in all {
            assert_eq!(describe(&finding), text);
            assert_eq!(finding.to_string(), text);
        }
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
