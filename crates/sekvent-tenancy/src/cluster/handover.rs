//! The ownership handover: the cluster half of adopting an existing
//! database. A database the maintenance role owns becomes the tenant role's,
//! with everything in it.
//!
//! [`prepare`] runs the checks and isolates the database; [`adopt`] runs
//! [`prepare`], the transfer and the CONNECT restriction. Each step runs on a
//! maintenance session of its own, one at a time, closed after:
//!
//! | Step | Session on | What |
//! |---|---|---|
//! | 1 | the maintenance database | for [`adopt`] first, an existing tenant role passes `privileges::check_tenant_role`; the database exists (`missing`), is no template (`template`), is owned by the maintenance role or already by the tenant role (`owner`), its ACL names no grantee but its owner and `PUBLIC`, and `PUBLIC` only with `CONNECT` or `TEMPORARY` without grant option (`grantee`); when the maintenance role owns it, that role owns no other database and no tablespace, which `REASSIGN OWNED` would move too (`owns_more`) |
//! | 2, reassign only | the database | no object of a role other than the maintenance role and the tenant role (`foreign_object`); `CONNECT` revoked from `PUBLIC`, on its own; no session of a role other than the maintenance role (`session`) |
//! | 3, reassign only, [`adopt`] | the database | one transaction: the tenant-role check, then the checks of steps 1 and 2 again, the revoke included; the membership in the tenant role `WITH INHERIT TRUE, SET TRUE`; the database's owner changed; `REASSIGN OWNED BY CURRENT_USER`; the membership back to `INHERIT FALSE, SET TRUE` |
//! | 4, [`adopt`] | the maintenance database | the tenant-role check; as the new owner: `CONNECT` revoked from `PUBLIC` and granted to the owner, connections allowed; then one read of the database: no finding of `privileges::tenant_database_findings` for the owner and no `CONNECT` of `PUBLIC` |
//!
//! A refusal is `FAILED_PRECONDITION` with the reason `ADOPTION_REFUSED`, the
//! message `the database cannot be adopted` and the metadata key `check`
//! naming the check; it carries no domain (the admin guard adds one). The
//! refusals of step 1 and `foreign_object` change nothing. `session`, and a
//! refusal of step 3's repeated checks, leave `PUBLIC`'s `CONNECT` revoked:
//! step 2 revokes it before the session check so that no new session of a
//! role without a grant of its own can start. Nothing else changes, because
//! step 3 is one transaction.
//!
//! A tenant role that fails its check, and a database that is not as
//! expected after the restriction, are `INTERNAL` without a reason, the
//! findings in the error's source, and nothing is repaired. An existing
//! role is checked in step 1 before anything changes; the role is checked
//! again first in step 3's transaction, before the inheriting membership
//! exists, where a missing role is refused too; and in step 4 before the
//! restriction. A database refused by step 4's read keeps its
//! new owner and its restriction, and every later call refuses it again.
//!
//! A call made again after an interruption converges: a database the tenant
//! role already owns takes the [`Adoption::Restrict`] path, and so does step
//! 3 when another call finished the transfer after its session opened. A call
//! that overlaps one that has just transferred the ownership can fail once
//! with a permission error, because the maintenance role can no longer
//! connect to the database; the next call takes the `Restrict` path and
//! converges. Afterwards the maintenance role holds the tenant role `WITH
//! INHERIT FALSE, SET TRUE` and can no longer connect to the database.
//!
//! One deadline of 150 s covers the SQL of a whole public call across its
//! sessions; past it the call is `UNAVAILABLE`. Waiting for the maintenance
//! permit, connecting and closing keep their own bounds.

use sekvent_db::to_app_error;
use sekvent_error::AppError;
use sqlx::PgConnection;
use tokio::time::{Instant, timeout_at};

use crate::cluster::databases::{check_restricted, restrict_connect};
use crate::cluster::maintenance::{
    Failure, OPERATION_TIMEOUT, begin, end_transaction, own_role, roll_back,
};
use crate::cluster::privileges::{self, DatabaseFacts, DatabasePrivilege, Grantee};
use crate::cluster::{Maintenance, MaintenanceConn, sql};
use crate::{DatabaseName, TenantRole, reasons};

/// The message of every refusal.
const REFUSED: &str = "the database cannot be adopted";

/// The metadata key of a refusal, `check`: its value names the refused
/// check (`missing`, `template`, `owner`, `grantee`, `owns_more`,
/// `foreign_object`, `session`).
pub const CHECK_KEY: &str = "check";

/// Other databases and tablespaces owned by the maintenance role (`$1`),
/// the candidate (`$2`) aside. Like every statement of the handover it is
/// type-exact: each comparison has an operator of exactly its two types in
/// `pg_catalog` (`oid`, `name` with `text`), which the session's
/// `search_path` searches first; casts and functions are qualified.
const OWNED_ELSEWHERE: &str = "
SELECT
    (SELECT pg_catalog.count(*)
       FROM pg_catalog.pg_database d
       JOIN pg_catalog.pg_roles r ON r.oid = d.datdba
      WHERE r.rolname = $1 AND d.datname <> $2),
    (SELECT pg_catalog.count(*)
       FROM pg_catalog.pg_tablespace t
       JOIN pg_catalog.pg_roles r ON r.oid = t.spcowner
      WHERE r.rolname = $1)";

/// Whether an object of the current database has an owner other than the
/// roles `$1` and `$2`: first from the recorded ownership dependencies, with
/// no exclusion; then from the owner column of every catalog that has one,
/// skipping system objects, temporary schemas and what is in them, extension
/// members and internally dependent objects (an extension's own row is
/// inspected).
///
/// Catalog classes are compared as `oid` with `oid` (a `regclass` cast on to
/// `oid`), and the temporary schemas are matched with `pg_catalog`'s `~` on
/// `text`, named: no operator of the database's own can stand in for one of
/// these comparisons.
const FOREIGN_OBJECTS: &str = "
WITH here AS (
    SELECT oid FROM pg_catalog.pg_database
     WHERE datname = pg_catalog.current_database()
),
allowed AS (
    SELECT oid FROM pg_catalog.pg_roles WHERE rolname IN ($1, $2)
),
owned (class_id, object_id, owner_id, namespace_id) AS (
    SELECT 'pg_catalog.pg_namespace'::pg_catalog.regclass::pg_catalog.oid, oid, nspowner, oid
      FROM pg_catalog.pg_namespace
    UNION ALL SELECT 'pg_catalog.pg_class'::pg_catalog.regclass::pg_catalog.oid,
           oid, relowner, relnamespace
      FROM pg_catalog.pg_class
    UNION ALL SELECT 'pg_catalog.pg_proc'::pg_catalog.regclass::pg_catalog.oid,
           oid, proowner, pronamespace
      FROM pg_catalog.pg_proc
    UNION ALL SELECT 'pg_catalog.pg_type'::pg_catalog.regclass::pg_catalog.oid,
           oid, typowner, typnamespace
      FROM pg_catalog.pg_type
    UNION ALL SELECT 'pg_catalog.pg_collation'::pg_catalog.regclass::pg_catalog.oid,
           oid, collowner, collnamespace
      FROM pg_catalog.pg_collation
    UNION ALL SELECT 'pg_catalog.pg_conversion'::pg_catalog.regclass::pg_catalog.oid,
           oid, conowner, connamespace
      FROM pg_catalog.pg_conversion
    UNION ALL SELECT 'pg_catalog.pg_operator'::pg_catalog.regclass::pg_catalog.oid,
           oid, oprowner, oprnamespace
      FROM pg_catalog.pg_operator
    UNION ALL SELECT 'pg_catalog.pg_opclass'::pg_catalog.regclass::pg_catalog.oid,
           oid, opcowner, opcnamespace
      FROM pg_catalog.pg_opclass
    UNION ALL SELECT 'pg_catalog.pg_opfamily'::pg_catalog.regclass::pg_catalog.oid,
           oid, opfowner, opfnamespace
      FROM pg_catalog.pg_opfamily
    UNION ALL SELECT 'pg_catalog.pg_statistic_ext'::pg_catalog.regclass::pg_catalog.oid,
           oid, stxowner, stxnamespace
      FROM pg_catalog.pg_statistic_ext
    UNION ALL SELECT 'pg_catalog.pg_ts_config'::pg_catalog.regclass::pg_catalog.oid,
           oid, cfgowner, cfgnamespace
      FROM pg_catalog.pg_ts_config
    UNION ALL SELECT 'pg_catalog.pg_ts_dict'::pg_catalog.regclass::pg_catalog.oid,
           oid, dictowner, dictnamespace
      FROM pg_catalog.pg_ts_dict
    UNION ALL SELECT 'pg_catalog.pg_default_acl'::pg_catalog.regclass::pg_catalog.oid,
           oid, defaclrole, defaclnamespace
      FROM pg_catalog.pg_default_acl
    UNION ALL SELECT 'pg_catalog.pg_language'::pg_catalog.regclass::pg_catalog.oid,
           oid, lanowner, 0::pg_catalog.oid
      FROM pg_catalog.pg_language
    UNION ALL SELECT 'pg_catalog.pg_largeobject_metadata'::pg_catalog.regclass::pg_catalog.oid,
           oid, lomowner, 0::pg_catalog.oid
      FROM pg_catalog.pg_largeobject_metadata
    UNION ALL SELECT 'pg_catalog.pg_foreign_data_wrapper'::pg_catalog.regclass::pg_catalog.oid,
           oid, fdwowner, 0::pg_catalog.oid
      FROM pg_catalog.pg_foreign_data_wrapper
    UNION ALL SELECT 'pg_catalog.pg_foreign_server'::pg_catalog.regclass::pg_catalog.oid,
           oid, srvowner, 0::pg_catalog.oid
      FROM pg_catalog.pg_foreign_server
    UNION ALL SELECT 'pg_catalog.pg_event_trigger'::pg_catalog.regclass::pg_catalog.oid,
           oid, evtowner, 0::pg_catalog.oid
      FROM pg_catalog.pg_event_trigger
    UNION ALL SELECT 'pg_catalog.pg_extension'::pg_catalog.regclass::pg_catalog.oid,
           oid, extowner, 0::pg_catalog.oid
      FROM pg_catalog.pg_extension
    UNION ALL SELECT 'pg_catalog.pg_publication'::pg_catalog.regclass::pg_catalog.oid,
           oid, pubowner, 0::pg_catalog.oid
      FROM pg_catalog.pg_publication
    UNION ALL SELECT 'pg_catalog.pg_subscription'::pg_catalog.regclass::pg_catalog.oid,
           oid, subowner, 0::pg_catalog.oid
      FROM pg_catalog.pg_subscription
     WHERE subdbid = (SELECT oid FROM here)
)
SELECT
    EXISTS (
        SELECT 1 FROM pg_catalog.pg_shdepend s
         WHERE s.dbid = (SELECT oid FROM here)
           AND s.deptype = 'o'
           AND s.refclassid = 'pg_catalog.pg_authid'::pg_catalog.regclass::pg_catalog.oid
           AND s.refobjid NOT IN (SELECT oid FROM allowed)
    ),
    EXISTS (
        SELECT 1 FROM owned o
         WHERE o.object_id >= 16384::pg_catalog.oid
           AND o.owner_id NOT IN (SELECT oid FROM allowed)
           AND NOT EXISTS (
               SELECT 1 FROM pg_catalog.pg_namespace n
                WHERE n.oid = o.namespace_id
                  AND n.nspname::pg_catalog.text
                      OPERATOR(pg_catalog.~) '^pg_(toast_)?temp_[0-9]+$'::pg_catalog.text
           )
           AND NOT EXISTS (
               SELECT 1 FROM pg_catalog.pg_depend d
                WHERE d.classid = o.class_id
                  AND d.objid = o.object_id
                  AND d.deptype IN ('e', 'i')
           )
    )";

/// Sessions on the current database whose role is not `$1`; backends
/// without a role (background workers) are not counted.
const OTHER_SESSIONS: &str = "
SELECT pg_catalog.count(*)
  FROM pg_catalog.pg_stat_activity a
 WHERE a.datid = (SELECT oid FROM pg_catalog.pg_database
                   WHERE datname = pg_catalog.current_database())
   AND a.usesysid IS NOT NULL
   AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
                    WHERE r.oid = a.usesysid AND r.rolname = $1)";

/// Every catalog statement of this module, by name, for the scan of
/// `cluster::tests`.
#[cfg(test)]
pub(super) const STATEMENTS: [(&str, &str); 3] = [
    ("OWNED_ELSEWHERE", OWNED_ELSEWHERE),
    ("FOREIGN_OBJECTS", FOREIGN_OBJECTS),
    ("OTHER_SESSIONS", OTHER_SESSIONS),
];

/// What a handover still has to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Adoption {
    /// The maintenance role still owns the database.
    Reassign,
    /// The tenant role owns it already; only the CONNECT restriction is left.
    Restrict,
}

/// The checks before the database changes hands.
///
/// Step 1 on the maintenance database; when the maintenance role owns the
/// database, step 2 on the database itself, which revokes `PUBLIC`'s
/// `CONNECT` before its session check. `role` need not exist yet. A role of
/// another object-name prefix, and the maintenance database itself, are
/// `INTERNAL` before any session opens.
pub async fn prepare(
    maintenance: &Maintenance,
    database: &DatabaseName,
    role: &TenantRole,
) -> Result<Adoption, AppError> {
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    prepare_until(maintenance, database, role, deadline, RoleCheck::Skipped).await
}

/// [`prepare`] with the tenant-role check first, the handover, the CONNECT
/// restriction and the read of the restricted database.
///
/// `role` must exist, pass the tenant-role check and be held by the
/// maintenance role (as `roles::create` leaves it). An existing role that
/// fails the check is `INTERNAL` before anything changes; a missing role is
/// `INTERNAL` at the transfer, after step 1's refusals and step 2. A call
/// that failed at any point converges when it is made again.
pub async fn adopt(
    maintenance: &Maintenance,
    database: &DatabaseName,
    role: &TenantRole,
) -> Result<(), AppError> {
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    let adoption = prepare_until(maintenance, database, role, deadline, RoleCheck::First).await?;
    if adoption == Adoption::Reassign {
        let mut conn = maintenance.connect_to(database).await?;
        let result = transfer(&mut conn, database, role, deadline).await;
        finish(conn, result).await?;
    }
    let mut conn = maintenance.connect().await?;
    let result = restrict(&mut conn, database, role, deadline).await;
    finish(conn, result).await
}

/// Whether step 1 checks the tenant role before anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoleCheck {
    /// [`adopt`]: an existing role is checked first.
    First,
    /// [`prepare`]: the role need not exist yet.
    Skipped,
}

/// Steps 1 and 2 under `deadline`.
async fn prepare_until(
    maintenance: &Maintenance,
    database: &DatabaseName,
    role: &TenantRole,
    deadline: Instant,
    role_check: RoleCheck,
) -> Result<Adoption, AppError> {
    check_arguments(maintenance, database, role)?;
    let mut conn = maintenance.connect().await?;
    let result = inspect(&mut conn, database, role, deadline, role_check).await;
    let adoption = finish(conn, result).await?;
    if adoption == Adoption::Reassign {
        let mut conn = maintenance.connect_to(database).await?;
        let result = isolate(&mut conn, database, role, deadline).await;
        finish(conn, result).await?;
    }
    Ok(adoption)
}

/// `INTERNAL` (no reason) for a role of another prefix and for the
/// maintenance database, which the maintenance role must keep reaching.
fn check_arguments(
    maintenance: &Maintenance,
    database: &DatabaseName,
    role: &TenantRole,
) -> Result<(), AppError> {
    own_role(maintenance.names(), role)?;
    if database.as_str() == maintenance.database() {
        return Err(AppError::internal(
            "the maintenance database cannot be handed over",
        ));
    }
    Ok(())
}

/// Closes `conn` after a step; the step's error wins over a close error.
async fn finish<T>(conn: MaintenanceConn, result: Result<T, AppError>) -> Result<T, AppError> {
    let closed = conn.close().await;
    let value = result?;
    closed.map(|()| value)
}

/// Step 1, on the maintenance database.
async fn inspect(
    conn: &mut MaintenanceConn,
    database: &DatabaseName,
    role: &TenantRole,
    deadline: Instant,
    role_check: RoleCheck,
) -> Result<Adoption, AppError> {
    let maintenance = conn.maintenance_role().to_owned();
    let session = conn.live()?;
    let outcome = timeout_at(
        deadline,
        inspect_on(session, database, role, &maintenance, role_check),
    )
    .await;
    conn.bounded(outcome)
}

/// Step 2, on the database.
async fn isolate(
    conn: &mut MaintenanceConn,
    database: &DatabaseName,
    role: &TenantRole,
    deadline: Instant,
) -> Result<(), AppError> {
    let maintenance = conn.maintenance_role().to_owned();
    let session = conn.live()?;
    let outcome = timeout_at(deadline, isolate_on(session, database, role, &maintenance)).await;
    conn.bounded(outcome)
}

/// Step 3, on the database.
async fn transfer(
    conn: &mut MaintenanceConn,
    database: &DatabaseName,
    role: &TenantRole,
    deadline: Instant,
) -> Result<(), AppError> {
    let maintenance = conn.maintenance_role().to_owned();
    let session = conn.live()?;
    let outcome = timeout_at(deadline, transfer_on(session, database, role, &maintenance)).await;
    conn.bounded(outcome)
}

/// Step 4, on the maintenance database.
async fn restrict(
    conn: &mut MaintenanceConn,
    database: &DatabaseName,
    role: &TenantRole,
    deadline: Instant,
) -> Result<(), AppError> {
    let session = conn.live()?;
    let outcome = timeout_at(deadline, restrict_on(session, database, role)).await;
    conn.bounded(outcome)
}

/// The checks of step 1, the tenant role's first when `role_check` asks.
async fn inspect_on(
    conn: &mut PgConnection,
    database: &DatabaseName,
    role: &TenantRole,
    maintenance: &str,
    role_check: RoleCheck,
) -> Result<Adoption, AppError> {
    if role_check == RoleCheck::First {
        privileges::check_existing_tenant_role(&mut *conn, role).await?;
    }
    let facts = privileges::database_facts(&mut *conn, database.as_str()).await?;
    let adoption =
        check_database(facts.as_ref(), maintenance, role.as_str()).map_err(Check::refuse)?;
    if adoption == Adoption::Reassign {
        let (databases, tablespaces) = owned_elsewhere(&mut *conn, database, maintenance).await?;
        check_owns_nothing_else(databases, tablespaces).map_err(Check::refuse)?;
    }
    Ok(adoption)
}

/// The checks and the revoke of step 2, also repeated inside step 3.
async fn isolate_on(
    conn: &mut PgConnection,
    database: &DatabaseName,
    role: &TenantRole,
    maintenance: &str,
) -> Result<(), AppError> {
    let (recorded, scanned) = sqlx::query_as::<_, (bool, bool)>(FOREIGN_OBJECTS)
        .bind(maintenance)
        .bind(role.as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(to_app_error)?;
    check_foreign_objects(recorded, scanned).map_err(Check::refuse)?;
    sql::run(&mut *conn, &sql::revoke_connect_from_public(database))
        .await
        .map_err(to_app_error)?;
    let sessions = sqlx::query_scalar::<_, i64>(OTHER_SESSIONS)
        .bind(maintenance)
        .fetch_one(&mut *conn)
        .await
        .map_err(to_app_error)?;
    check_sessions(sessions).map_err(Check::refuse)
}

/// The transaction of step 3: committed after the transfer, rolled back
/// (and the rollback awaited) on any refusal or error.
async fn transfer_on(
    conn: &mut PgConnection,
    database: &DatabaseName,
    role: &TenantRole,
    maintenance: &str,
) -> Result<(), Failure> {
    let mut tx = begin(conn).await?;
    match transfer_steps(&mut tx, database, role, maintenance).await {
        // Another call finished the transfer since step 1: nothing changed.
        Ok(Adoption::Restrict) => roll_back(tx).await,
        result => end_transaction(tx, result.map(|_| ())).await,
    }
}

/// The checks and the statements of step 3, inside its transaction. The
/// path the recheck found: `Restrict` when the tenant role owns the
/// database already, and nothing ran. The tenant role is checked first,
/// while the maintenance role's membership in it does not inherit.
async fn transfer_steps(
    tx: &mut PgConnection,
    database: &DatabaseName,
    role: &TenantRole,
    maintenance: &str,
) -> Result<Adoption, AppError> {
    privileges::check_tenant_role(&mut *tx, role).await?;
    let facts = privileges::database_facts(&mut *tx, database.as_str()).await?;
    let adoption =
        check_database(facts.as_ref(), maintenance, role.as_str()).map_err(Check::refuse)?;
    if adoption == Adoption::Restrict {
        return Ok(adoption);
    }
    let (databases, tablespaces) = owned_elsewhere(&mut *tx, database, maintenance).await?;
    check_owns_nothing_else(databases, tablespaces).map_err(Check::refuse)?;
    isolate_on(&mut *tx, database, role, maintenance).await?;
    for statement in [
        sql::grant_to_current_user_inheriting(role),
        sql::alter_database_owner(database, role),
        sql::reassign_owned_to(role),
        sql::grant_to_current_user(role),
    ] {
        sql::run(&mut *tx, &statement).await.map_err(to_app_error)?;
    }
    Ok(adoption)
}

/// The statements of step 4: the tenant-role check, the restriction, the
/// read of the restricted database.
async fn restrict_on(
    conn: &mut PgConnection,
    database: &DatabaseName,
    role: &TenantRole,
) -> Result<(), Failure> {
    privileges::check_tenant_role(&mut *conn, role).await?;
    restrict_connect(&mut *conn, database, role).await?;
    check_restricted(conn, database, role).await?;
    Ok(())
}

/// The number of other databases and of tablespaces `maintenance` owns.
async fn owned_elsewhere(
    conn: &mut PgConnection,
    database: &DatabaseName,
    maintenance: &str,
) -> Result<(i64, i64), AppError> {
    sqlx::query_as::<_, (i64, i64)>(OWNED_ELSEWHERE)
        .bind(maintenance)
        .bind(database.as_str())
        .fetch_one(conn)
        .await
        .map_err(to_app_error)
}

/// A precondition of the handover; its name is the `check` metadata value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    Missing,
    Template,
    Owner,
    Grantee,
    OwnsMore,
    ForeignObject,
    Session,
}

impl Check {
    /// The value of the `check` metadata key.
    const fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Template => "template",
            Self::Owner => "owner",
            Self::Grantee => "grantee",
            Self::OwnsMore => "owns_more",
            Self::ForeignObject => "foreign_object",
            Self::Session => "session",
        }
    }

    /// The refusal: `FAILED_PRECONDITION`, `ADOPTION_REFUSED`, the generic
    /// message and the check's name, no domain.
    fn refuse(self) -> AppError {
        AppError::failed_precondition(REFUSED)
            .with_reason(reasons::ADOPTION_REFUSED)
            .with_metadata(CHECK_KEY, self.name())
    }
}

/// Step 1's checks of the database itself, in their order: `missing`,
/// `template`, `owner`, `grantee`. The owner decides the path. `PUBLIC` may
/// hold `CONNECT` and `TEMPORARY` without grant option, what a tenant
/// database may keep; any other entry of `PUBLIC` is a `grantee` too, so
/// that the read after the restriction does not refuse a database that has
/// already changed hands.
fn check_database(
    facts: Option<&DatabaseFacts>,
    maintenance: &str,
    role: &str,
) -> Result<Adoption, Check> {
    let facts = facts.ok_or(Check::Missing)?;
    if facts.template {
        return Err(Check::Template);
    }
    let adoption = if facts.owner == maintenance {
        Adoption::Reassign
    } else if facts.owner == role {
        Adoption::Restrict
    } else {
        return Err(Check::Owner);
    };
    let foreign_grantee = facts.grants.iter().any(|grant| match &grant.grantee {
        Grantee::Public => {
            grant.grantable
                || !matches!(
                    grant.privilege,
                    DatabasePrivilege::Connect | DatabasePrivilege::Temporary
                )
        }
        Grantee::Role(name) => *name != facts.owner,
    });
    if foreign_grantee {
        return Err(Check::Grantee);
    }
    Ok(adoption)
}

/// `owns_more`: `REASSIGN OWNED` would also move every other database and
/// tablespace of the maintenance role.
fn check_owns_nothing_else(other_databases: i64, tablespaces: i64) -> Result<(), Check> {
    if other_databases == 0 && tablespaces == 0 {
        Ok(())
    } else {
        Err(Check::OwnsMore)
    }
}

/// `foreign_object`: either the recorded dependencies or the owner columns
/// found an object of a third role.
fn check_foreign_objects(recorded: bool, scanned: bool) -> Result<(), Check> {
    if recorded || scanned {
        Err(Check::ForeignObject)
    } else {
        Ok(())
    }
}

/// `session`: a session of a role other than the maintenance role.
fn check_sessions(sessions: i64) -> Result<(), Check> {
    if sessions == 0 {
        Ok(())
    } else {
        Err(Check::Session)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sekvent_error::ErrorCode;
    use sqlx::postgres::PgConnectOptions;
    use tokio::time::Instant;
    use uuid::Uuid;

    use sqlx::{Connection, PgConnection};

    use super::{
        Adoption, CHECK_KEY, Check, FOREIGN_OBJECTS, RoleCheck, adopt, check_database,
        check_foreign_objects, check_owns_nothing_else, check_sessions, inspect, isolate, prepare,
        restrict, transfer,
    };
    use crate::cluster::maintenance::OPERATION_TIMEOUT;
    use crate::cluster::privileges::{DatabaseFacts, DatabaseGrant, DatabasePrivilege, Grantee};
    use crate::cluster::testing;
    use crate::cluster::{Maintenance, MaintenanceConn};
    use crate::{DatabaseName, ObjectNames, TenantId, TenantRole, reasons};

    const MAINTENANCE: &str = "billing_maintenance";
    const THIRD: &str = "billing_reader";

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("the prefix is valid")
    }

    fn role() -> TenantRole {
        names().role(TenantId::from_uuid(Uuid::from_u128(7)))
    }

    fn orders() -> DatabaseName {
        DatabaseName::parse("orders").expect("the name is valid")
    }

    /// A maintenance connection nobody listens on; none of these tests
    /// connects.
    fn maintenance() -> Maintenance {
        let options = PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(1)
            .username(MAINTENANCE)
            .database("billing");
        Maintenance::new(options, names())
    }

    fn grant(grantee: Grantee, privilege: DatabasePrivilege) -> DatabaseGrant {
        DatabaseGrant {
            grantee,
            privilege,
            grantable: false,
        }
    }

    /// The default ACL of a database owned by `owner`.
    fn defaults(owner: &str) -> Vec<DatabaseGrant> {
        vec![
            grant(Grantee::Role(owner.to_owned()), DatabasePrivilege::Connect),
            grant(
                Grantee::Role(owner.to_owned()),
                DatabasePrivilege::Temporary,
            ),
            grant(Grantee::Role(owner.to_owned()), DatabasePrivilege::Create),
            grant(Grantee::Public, DatabasePrivilege::Connect),
            grant(Grantee::Public, DatabasePrivilege::Temporary),
        ]
    }

    fn facts(owner: &str, template: bool, grants: Vec<DatabaseGrant>) -> DatabaseFacts {
        DatabaseFacts {
            name: "orders".to_owned(),
            owner: owner.to_owned(),
            template,
            grants,
        }
    }

    fn decide(facts: &DatabaseFacts) -> Result<Adoption, Check> {
        check_database(Some(facts), MAINTENANCE, role().as_str())
    }

    #[test]
    fn every_check_has_its_metadata_name() {
        let all = [
            (Check::Missing, "missing"),
            (Check::Template, "template"),
            (Check::Owner, "owner"),
            (Check::Grantee, "grantee"),
            (Check::OwnsMore, "owns_more"),
            (Check::ForeignObject, "foreign_object"),
            (Check::Session, "session"),
        ];
        for (check, name) in all {
            assert_eq!(check.name(), name);
        }
    }

    #[test]
    fn a_refusal_carries_the_reason_the_generic_message_and_the_check_only() {
        for check in [
            Check::Missing,
            Check::Template,
            Check::Owner,
            Check::Grantee,
            Check::OwnsMore,
            Check::ForeignObject,
            Check::Session,
        ] {
            let err = check.refuse();
            assert_eq!(err.code(), ErrorCode::FailedPrecondition);
            assert_eq!(err.reason(), Some(reasons::ADOPTION_REFUSED));
            assert_eq!(err.domain(), None);
            assert_eq!(err.message(), "the database cannot be adopted");
            let expected = BTreeMap::from([(CHECK_KEY.to_owned(), check.name().to_owned())]);
            assert_eq!(CHECK_KEY, "check");
            assert_eq!(err.metadata(), &expected);
        }
    }

    #[test]
    fn a_missing_database_is_refused_first() {
        assert_eq!(
            check_database(None, MAINTENANCE, role().as_str()),
            Err(Check::Missing)
        );
    }

    #[test]
    fn a_template_is_refused_before_its_owner_is_looked_at() {
        assert_eq!(
            decide(&facts(MAINTENANCE, true, defaults(MAINTENANCE))),
            Err(Check::Template)
        );
        assert_eq!(
            decide(&facts(THIRD, true, defaults(THIRD))),
            Err(Check::Template)
        );
    }

    #[test]
    fn a_database_of_a_third_role_is_refused_before_its_grantees() {
        assert_eq!(
            decide(&facts(THIRD, false, defaults(THIRD))),
            Err(Check::Owner)
        );
        let mut grants = defaults(THIRD);
        grants.push(grant(
            Grantee::Role(MAINTENANCE.to_owned()),
            DatabasePrivilege::Connect,
        ));
        assert_eq!(decide(&facts(THIRD, false, grants)), Err(Check::Owner));
    }

    #[test]
    fn the_owner_decides_the_path() {
        assert_eq!(
            decide(&facts(MAINTENANCE, false, defaults(MAINTENANCE))),
            Ok(Adoption::Reassign)
        );
        assert_eq!(
            decide(&facts(MAINTENANCE, false, Vec::new())),
            Ok(Adoption::Reassign)
        );
        let owner = role();
        assert_eq!(
            decide(&facts(owner.as_str(), false, defaults(owner.as_str()))),
            Ok(Adoption::Restrict)
        );
    }

    #[test]
    fn a_grant_to_any_role_but_the_owner_is_refused() {
        let mut grants = defaults(MAINTENANCE);
        grants.push(grant(
            Grantee::Role(THIRD.to_owned()),
            DatabasePrivilege::Connect,
        ));
        assert_eq!(
            decide(&facts(MAINTENANCE, false, grants)),
            Err(Check::Grantee)
        );

        let owner = role();
        let mut grants = defaults(owner.as_str());
        grants.push(grant(
            Grantee::Role(MAINTENANCE.to_owned()),
            DatabasePrivilege::Temporary,
        ));
        assert_eq!(
            decide(&facts(owner.as_str(), false, grants)),
            Err(Check::Grantee)
        );
    }

    #[test]
    fn public_may_keep_only_connect_and_temporary_without_grant_option() {
        let mut grants = defaults(MAINTENANCE);
        grants.push(grant(Grantee::Public, DatabasePrivilege::Create));
        assert_eq!(
            decide(&facts(MAINTENANCE, false, grants)),
            Err(Check::Grantee)
        );

        let owner = role();
        let mut grants = defaults(owner.as_str());
        grants.push(DatabaseGrant {
            grantable: true,
            ..grant(Grantee::Public, DatabasePrivilege::Connect)
        });
        assert_eq!(
            decide(&facts(owner.as_str(), false, grants)),
            Err(Check::Grantee)
        );

        let only_temporary = vec![grant(Grantee::Public, DatabasePrivilege::Temporary)];
        assert_eq!(
            decide(&facts(MAINTENANCE, false, only_temporary)),
            Ok(Adoption::Reassign)
        );
    }

    #[test]
    fn the_maintenance_role_may_own_nothing_else() {
        assert_eq!(check_owns_nothing_else(0, 0), Ok(()));
        assert_eq!(check_owns_nothing_else(1, 0), Err(Check::OwnsMore));
        assert_eq!(check_owns_nothing_else(0, 1), Err(Check::OwnsMore));
        assert_eq!(check_owns_nothing_else(2, 3), Err(Check::OwnsMore));
    }

    #[test]
    fn either_arm_of_the_object_check_refuses() {
        assert_eq!(check_foreign_objects(false, false), Ok(()));
        assert_eq!(
            check_foreign_objects(true, false),
            Err(Check::ForeignObject)
        );
        assert_eq!(
            check_foreign_objects(false, true),
            Err(Check::ForeignObject)
        );
        assert_eq!(check_foreign_objects(true, true), Err(Check::ForeignObject));
    }

    #[test]
    fn any_other_session_refuses() {
        assert_eq!(check_sessions(0), Ok(()));
        assert_eq!(check_sessions(1), Err(Check::Session));
        assert_eq!(check_sessions(12), Err(Check::Session));
    }

    #[tokio::test]
    async fn a_role_of_another_prefix_is_internal_before_any_session() {
        let other = ObjectNames::new("app_other_")
            .expect("the prefix is valid")
            .role(TenantId::from_uuid(Uuid::from_u128(7)));
        let maintenance = maintenance();
        let errors = [
            prepare(&maintenance, &orders(), &other)
                .await
                .expect_err("a foreign role is refused"),
            adopt(&maintenance, &orders(), &other)
                .await
                .expect_err("a foreign role is refused"),
        ];
        for err in errors {
            assert_eq!(err.code(), ErrorCode::Internal);
            assert_eq!(err.reason(), None);
        }
    }

    #[tokio::test]
    async fn the_maintenance_database_is_internal_before_any_session() {
        let maintenance = maintenance();
        let own = DatabaseName::parse("billing").expect("the name is valid");
        let errors = [
            prepare(&maintenance, &own, &role())
                .await
                .expect_err("the maintenance database is refused"),
            adopt(&maintenance, &own, &role())
                .await
                .expect_err("the maintenance database is refused"),
        ];
        for err in errors {
            assert_eq!(err.code(), ErrorCode::Internal);
            assert_eq!(err.reason(), None);
        }
    }

    #[tokio::test]
    async fn every_step_refuses_a_connection_without_a_session() {
        let deadline = Instant::now() + OPERATION_TIMEOUT;
        let mut conn = MaintenanceConn::detached(names(), MAINTENANCE);
        let codes = [
            inspect(&mut conn, &orders(), &role(), deadline, RoleCheck::Skipped)
                .await
                .expect_err("no session")
                .code(),
            inspect(&mut conn, &orders(), &role(), deadline, RoleCheck::First)
                .await
                .expect_err("no session")
                .code(),
            isolate(&mut conn, &orders(), &role(), deadline)
                .await
                .expect_err("no session")
                .code(),
            transfer(&mut conn, &orders(), &role(), deadline)
                .await
                .expect_err("no session")
                .code(),
            restrict(&mut conn, &orders(), &role(), deadline)
                .await
                .expect_err("no session")
                .code(),
        ];
        assert_eq!(codes, [ErrorCode::Internal; 5]);
    }

    /// `public."="(oid, regclass)` and `public."~"(name, name)`, whose
    /// functions raise, and a table of a third role so that an ownership is
    /// recorded: the comparisons the object scan once made had no
    /// `pg_catalog` operator of exactly their types, so these would win.
    fn hostile_operators(database: &str) -> String {
        format!(
            "CREATE FUNCTION public.hostile_eq(oid, pg_catalog.regclass) RETURNS boolean {raises}; \
             CREATE OPERATOR public.= (LEFTARG = oid, RIGHTARG = pg_catalog.regclass, \
                 FUNCTION = public.hostile_eq); \
             CREATE FUNCTION public.hostile_match(name, name) RETURNS boolean {raises}; \
             CREATE OPERATOR public.~ (LEFTARG = name, RIGHTARG = name, \
                 FUNCTION = public.hostile_match); \
             CREATE ROLE \"{database}_owner\"; \
             CREATE TABLE public.recorded (id bigint); \
             ALTER TABLE public.recorded OWNER TO \"{database}_owner\"",
            raises = testing::RAISES
        )
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn the_object_scan_calls_no_operator_of_the_databases_own() {
        let Some((options, _)) = testing::public_first(hostile_operators).await else {
            return;
        };
        tokio::time::timeout(testing::SCENARIO_TIMEOUT, async {
            // A session without the safe search_path: the database's own,
            // public first.
            let mut conn = PgConnection::connect_with(&options)
                .await
                .expect("the superuser connected");
            let search_path: String =
                sqlx::query_scalar("SELECT pg_catalog.current_setting('search_path')")
                    .fetch_one(&mut conn)
                    .await
                    .expect("the setting was read");
            assert!(search_path.starts_with("public"), "{search_path}");
            let (recorded, scanned) = sqlx::query_as::<_, (bool, bool)>(FOREIGN_OBJECTS)
                .bind("nobody_allowed")
                .bind("nobody_allowed_either")
                .fetch_one(&mut conn)
                .await
                .expect("the scan called no operator of the database");
            assert!(recorded, "the third role's table is recorded");
            assert!(
                scanned,
                "the superuser's functions are found by their owner"
            );
            conn.close().await.expect("the session closed");
        })
        .await
        .expect("the scenario finished within its 30 s guard");
    }
}
