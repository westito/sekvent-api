//! Tenant databases: created closed and opened only with their CONNECT
//! restriction, dropped only as the tenant's role, measured.

use sekvent_db::{sqlstate, to_app_error};
use sekvent_error::AppError;
use sqlx::PgConnection;
use tokio::time::timeout;

use crate::cluster::maintenance::{
    Failure, OPERATION_TIMEOUT, begin, end_transaction, own_database, own_role,
};
use crate::cluster::privileges::{
    DatabaseFacts, DatabaseFinding, DatabasePrivilege, Grantee, tenant_database_findings,
};
use crate::cluster::{MaintenanceConn, privileges, sql};
use crate::{DatabaseName, ObjectNames, TenantRole};

/// The owner of a database, no row when there is none.
const DATABASE_OWNER: &str = "SELECT pg_catalog.pg_get_userbyid(datdba)::pg_catalog.text \
     FROM pg_catalog.pg_database WHERE datname = $1";

/// Everything the drop decides from, in one read: the owner, whether it is a
/// template, whether it is the session's own database, the session's role,
/// whether the expected owner (`$2`) exists.
const DROP_FACTS: &str = "SELECT pg_catalog.pg_get_userbyid(d.datdba)::pg_catalog.text, \
     d.datistemplate, d.datname = pg_catalog.current_database(), \
     CURRENT_USER::pg_catalog.text, \
     EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = $2) \
     FROM pg_catalog.pg_database d WHERE d.datname = $1";

/// The owner of a database and the session's role.
const SIZE_FACTS: &str = "SELECT pg_catalog.pg_get_userbyid(datdba)::pg_catalog.text, \
     CURRENT_USER::pg_catalog.text \
     FROM pg_catalog.pg_database WHERE datname = $1";

/// The owner of a database and whether its ACL (or the default one) gives
/// `PUBLIC` `CONNECT`, in one row; no row when there is no such database.
const DATABASE_ACCESS: &str = "SELECT pg_catalog.pg_get_userbyid(d.datdba)::pg_catalog.text, \
     EXISTS (SELECT 1 \
     FROM pg_catalog.aclexplode(COALESCE(d.datacl, pg_catalog.acldefault('d', d.datdba))) a \
     WHERE a.grantee = 0::pg_catalog.oid AND a.privilege_type = 'CONNECT') \
     FROM pg_catalog.pg_database d WHERE d.datname = $1";

/// The size of a database, no row when it is gone.
const DATABASE_SIZE: &str = "SELECT pg_catalog.pg_database_size(oid) \
     FROM pg_catalog.pg_database WHERE datname = $1";

/// Every catalog statement of this module, by name, for the scan of
/// `cluster::tests`.
#[cfg(test)]
pub(super) const STATEMENTS: [(&str, &str); 5] = [
    ("DATABASE_OWNER", DATABASE_OWNER),
    ("DROP_FACTS", DROP_FACTS),
    ("SIZE_FACTS", SIZE_FACTS),
    ("DATABASE_ACCESS", DATABASE_ACCESS),
    ("DATABASE_SIZE", DATABASE_SIZE),
];

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// Creates `database` owned by `owner`, or checks an existing one `owner`
/// owns, and in either case applies the CONNECT restriction and opens it.
/// `database` must be an object name of the connection's names.
///
/// `owner` is checked first (`privileges::check_tenant_role`). A new
/// database is created closed (`ALLOW_CONNECTIONS false`) and opens only
/// together with its restriction, so a call resumed after a failure at any
/// point converges. An existing database with drift is refused and left as
/// it was. After the restriction the database is read once more: a finding
/// of `privileges::tenant_database_findings` for `owner`, or a `CONNECT`
/// of `PUBLIC`, is `INTERNAL` with the findings in the error's source, and
/// nothing is repaired (the restriction stays).
pub async fn create(
    conn: &mut MaintenanceConn,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), AppError> {
    own_database(conn.names(), database)?;
    own_role(conn.names(), owner)?;
    let outcome = timeout(OPERATION_TIMEOUT, async {
        let session = conn.live()?;
        create_on(session, database, owner).await
    })
    .await;
    conn.bounded(outcome)
}

/// Drops `database` as `owner` when `owner` owns it; leaves a database of
/// another owner alone once `owner` is gone; refuses everything else.
///
/// The drop runs as `owner` (`SET ROLE`, `DROP DATABASE IF EXISTS … WITH
/// (FORCE)`, `RESET ROLE`, each alone), never as the maintenance role and
/// never as a discovered other owner. The connection is poisoned from before
/// the `SET ROLE` until a `RESET ROLE` succeeded.
pub async fn drop(
    conn: &mut MaintenanceConn,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), AppError> {
    own_role(conn.names(), owner)?;
    let outcome = timeout(OPERATION_TIMEOUT, drop_on(conn, database, owner)).await;
    conn.bounded(outcome)
}

/// The size of `database` in bytes.
///
/// Read as the owner (`SET LOCAL ROLE`) when a tenant role of the
/// connection's names owns it, as the session's role when that role owns it,
/// and `INTERNAL` for any other owner and for a missing database.
pub async fn size(conn: &mut MaintenanceConn, database: &DatabaseName) -> Result<u64, AppError> {
    let names = conn.names().clone();
    let outcome = timeout(OPERATION_TIMEOUT, async {
        let session = conn.live()?;
        size_on(session, &names, database).await
    })
    .await;
    conn.bounded(outcome)
}

/// Who owns a database and whether `PUBLIC` may connect to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseAccess {
    /// The owner's role name.
    pub owner: String,
    /// Whether `PUBLIC` holds an effective `CONNECT`.
    pub public_connect: bool,
}

/// The owner of `database` and `PUBLIC`'s effective `CONNECT` on it, read in
/// one statement with the name bound as a parameter; `None` when there is no
/// such database.
///
/// Any database name is read, `postgres` and other names [`DatabaseName`]
/// refuses included, and nothing checks it against the connection's object
/// names.
pub async fn access(
    conn: &mut MaintenanceConn,
    database: &str,
) -> Result<Option<DatabaseAccess>, AppError> {
    let outcome = timeout(OPERATION_TIMEOUT, async {
        let session = conn.live()?;
        access_on(session, database).await
    })
    .await;
    conn.bounded(outcome)
}

/// As `owner`, in one transaction (`SET LOCAL ROLE`): `CONNECT` revoked from
/// `PUBLIC` and granted to the owner, then connections allowed. A failed
/// statement rolls the transaction back before the error returns.
pub(crate) async fn restrict_connect(
    conn: &mut PgConnection,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), Failure> {
    let mut tx = begin(conn).await?;
    let result = restrict_steps(&mut tx, database, owner).await;
    end_transaction(tx, result).await
}

/// The statements of [`restrict_connect`], inside its transaction.
async fn restrict_steps(
    tx: &mut PgConnection,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), AppError> {
    for statement in [
        sql::set_local_role(owner),
        sql::revoke_connect_from_public(database),
        sql::grant_connect(database, owner),
        sql::allow_connections(database),
    ] {
        sql::run(tx, &statement).await.map_err(to_app_error)?;
    }
    Ok(())
}

/// After [`restrict_connect`], in one read: `database` has no finding of
/// [`tenant_database_findings`] for `owner` and `PUBLIC` holds no
/// `CONNECT`. Anything else, and a missing database, is `INTERNAL` naming
/// every finding in its source; nothing is repaired.
pub(crate) async fn check_restricted(
    conn: &mut PgConnection,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), AppError> {
    let facts = privileges::database_facts(conn, database.as_str()).await?;
    restricted_verdict(database.as_str(), owner.as_str(), facts.as_ref())
}

/// Pure: what is wrong with a database `owner` owns after its restriction:
/// every finding of [`tenant_database_findings`], then each `CONNECT` of
/// `PUBLIC` that rule lets pass (one without grant option).
fn restricted_findings(facts: &DatabaseFacts, owner: &str) -> Vec<DatabaseFinding> {
    let mut found = tenant_database_findings(facts, owner);
    found.extend(
        facts
            .grants
            .iter()
            .filter(|grant| {
                grant.grantee == Grantee::Public
                    && grant.privilege == DatabasePrivilege::Connect
                    && !grant.grantable
            })
            .cloned()
            .map(DatabaseFinding::Grant),
    );
    found
}

/// The outcome of [`check_restricted`] for what was read.
fn restricted_verdict(
    database: &str,
    owner: &str,
    facts: Option<&DatabaseFacts>,
) -> Result<(), AppError> {
    let Some(facts) = facts else {
        return Err(AppError::internal(format!(
            "tenant database {database} does not exist after its restriction"
        )));
    };
    let found = restricted_findings(facts, owner);
    if found.is_empty() {
        return Ok(());
    }
    let described: Vec<String> = found.iter().map(ToString::to_string).collect();
    Err(AppError::internal(format!(
        "tenant database {database} is not as expected after its restriction: {}",
        described.join("; ")
    )))
}

/// The owner of `database`, `None` when there is no such database.
async fn database_owner(
    session: &mut PgConnection,
    database: &DatabaseName,
) -> Result<Option<String>, AppError> {
    sqlx::query_scalar::<_, String>(DATABASE_OWNER)
        .bind(database.as_str())
        .fetch_optional(session)
        .await
        .map_err(to_app_error)
}

/// The body of [`access`]: its one statement.
async fn access_on(
    session: &mut PgConnection,
    database: &str,
) -> Result<Option<DatabaseAccess>, AppError> {
    let row = sqlx::query_as::<_, (String, bool)>(DATABASE_ACCESS)
        .bind(database)
        .fetch_optional(session)
        .await
        .map_err(to_app_error)?;
    Ok(row.map(|(owner, public_connect)| DatabaseAccess {
        owner,
        public_connect,
    }))
}

/// The body of [`create`].
async fn create_on(
    session: &mut PgConnection,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), Failure> {
    privileges::check_tenant_role(session, owner).await?;
    match database_owner(session, database).await? {
        None => sql::run(session, &sql::create_database(database, owner))
            .await
            .map_err(to_app_error)?,
        Some(found) if found == owner.as_str() => {
            privileges::check_tenant_database(session, database, owner).await?;
        }
        Some(_) => {
            return Err(AppError::internal(format!(
                "the database {} exists with another owner",
                database.as_str()
            ))
            .into());
        }
    }
    restrict_connect(session, database, owner).await?;
    check_restricted(session, database, owner).await?;
    Ok(())
}

/// What the drop's one catalog read found about an existing database.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DropFacts {
    /// The database's owner.
    owner: String,
    /// It is a template.
    template: bool,
    /// It is the database the session is connected to.
    session_database: bool,
    /// The session's current role.
    session_role: String,
    /// The expected owner still exists.
    owner_exists: bool,
}

/// What a drop does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DropDecision {
    /// There is no such database: done.
    Absent,
    /// Owned by the expected owner and nothing protects it: drop it as that
    /// owner.
    DropAsOwner,
    /// Owned by another role and the expected owner is gone: the tenant's
    /// database went before its role and the name is someone else's now.
    LeftAlone,
    /// Nothing is dropped; the text says why.
    Refused(&'static str),
}

/// The decision of a drop for the expected `owner`, from its one catalog read
/// (`None`: there is no such database).
fn drop_decision(found: Option<&DropFacts>, owner: &str) -> DropDecision {
    let Some(facts) = found else {
        return DropDecision::Absent;
    };
    if facts.owner != owner {
        return if facts.owner_exists {
            DropDecision::Refused(
                "the database is owned by another role while its tenant role exists",
            )
        } else {
            DropDecision::LeftAlone
        };
    }
    if facts.template {
        DropDecision::Refused("the database is a template")
    } else if facts.session_database {
        DropDecision::Refused("the database is the maintenance session's own")
    } else if facts.session_role == owner {
        DropDecision::Refused("the database's owner is the session's role")
    } else {
        DropDecision::DropAsOwner
    }
}

/// The body of [`drop`].
async fn drop_on(
    conn: &mut MaintenanceConn,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), AppError> {
    let session = conn.live()?;
    let facts = sqlx::query_as::<_, (String, bool, bool, String, bool)>(DROP_FACTS)
        .bind(database.as_str())
        .bind(owner.as_str())
        .fetch_optional(session)
        .await
        .map_err(to_app_error)?
        .map(
            |(found_owner, template, session_database, session_role, owner_exists)| DropFacts {
                owner: found_owner,
                template,
                session_database,
                session_role,
                owner_exists,
            },
        );
    match drop_decision(facts.as_ref(), owner.as_str()) {
        DropDecision::Absent | DropDecision::LeftAlone => return Ok(()),
        DropDecision::Refused(why) => return Err(AppError::internal(why)),
        DropDecision::DropAsOwner => {}
    }
    conn.mark_role_switched();
    let dropped = drop_as(conn.session()?, database, owner).await;
    let reset = sql::run(conn.session()?, sql::RESET_ROLE).await;
    let (answer, reset_succeeded) = settle(dropped, reset);
    if reset_succeeded {
        conn.mark_role_reset();
    }
    answer
}

/// `SET ROLE <owner>`, then the drop, on a session already marked poisoned.
async fn drop_as(
    session: &mut PgConnection,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), AppError> {
    sql::run(session, &sql::set_role(owner))
        .await
        .map_err(switch_error)?;
    sql::run(session, &sql::drop_database(database))
        .await
        .map_err(switch_error)
}

/// A failed `SET ROLE` or drop: a missing right of the maintenance role on
/// its own cluster is `INTERNAL`; anything else maps as a database error and
/// may be issued again.
fn switch_error(error: sqlx::Error) -> AppError {
    if is_insufficient_privilege(sqlstate(&error).as_deref()) {
        AppError::internal("the maintenance role lacks a right on its own cluster")
    } else {
        to_app_error(error)
    }
}

/// Whether a SQLSTATE is `insufficient_privilege`.
fn is_insufficient_privilege(code: Option<&str>) -> bool {
    code == Some(INSUFFICIENT_PRIVILEGE)
}

/// The drop's answer once its `RESET ROLE` ran, and whether the reset
/// succeeded (which lifts the role-switch poison). The drop's own error wins
/// over the reset's.
fn settle(
    dropped: Result<(), AppError>,
    reset: Result<(), sqlx::Error>,
) -> (Result<(), AppError>, bool) {
    match (dropped, reset) {
        (dropped, Ok(())) => (dropped, true),
        (Ok(()), Err(error)) => (Err(to_app_error(error)), false),
        (Err(error), Err(_)) => (Err(error), false),
    }
}

/// The body of [`size`], in one transaction that a refusal rolls back.
async fn size_on(
    session: &mut PgConnection,
    names: &ObjectNames,
    database: &DatabaseName,
) -> Result<u64, Failure> {
    let mut tx = begin(session).await?;
    let result = measure(&mut tx, names, database).await;
    end_transaction(tx, result).await
}

/// The reads of [`size`], inside its transaction.
async fn measure(
    tx: &mut PgConnection,
    names: &ObjectNames,
    database: &DatabaseName,
) -> Result<u64, AppError> {
    let (owner, session_role) = sqlx::query_as::<_, (String, String)>(SIZE_FACTS)
        .bind(database.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(to_app_error)?
        .ok_or_else(|| AppError::internal("the database to measure does not exist"))?;
    if let Some(role) = size_role(names, &owner, &session_role)? {
        sql::run(tx, &sql::set_local_role(&role))
            .await
            .map_err(to_app_error)?;
    }
    let raw = sqlx::query_scalar::<_, Option<i64>>(DATABASE_SIZE)
        .bind(database.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(to_app_error)?
        .flatten();
    size_bytes(raw)
}

/// Which role measures a database `owner` owns: the session's role itself
/// (`None`) when it is the owner, the owner when it is a tenant role of
/// `names`; `INTERNAL` for any other owner.
fn size_role(
    names: &ObjectNames,
    owner: &str,
    session_role: &str,
) -> Result<Option<TenantRole>, AppError> {
    if owner == session_role {
        return Ok(None);
    }
    if !names.is_object_name(owner) {
        return Err(AppError::internal(
            "the database's owner is neither the session's role nor a tenant role",
        ));
    }
    names.parse_role(owner).map(Some)
}

/// The size read, in bytes: `INTERNAL` when the database disappeared between
/// the reads or the size is negative.
fn size_bytes(raw: Option<i64>) -> Result<u64, AppError> {
    let raw = raw.ok_or_else(|| AppError::internal("the database disappeared while measured"))?;
    u64::try_from(raw).map_err(|_| AppError::internal("the database size is negative"))
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use sekvent_error::ErrorCode;
    use sqlx::Connection;
    use uuid::Uuid;

    use super::*;
    use crate::TenantId;
    use crate::cluster::privileges::DatabaseGrant;
    use crate::cluster::testing;

    const OWNER: &str = "app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d";
    const MAINTENANCE: &str = "orders_maintenance";

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("the prefix is valid")
    }

    fn tenant() -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(0x0b9c_2f4e_51a8_4d3b_9e6f_7c1d_2a3b_4c5d))
    }

    fn source(error: &AppError) -> String {
        error.source().map(ToString::to_string).unwrap_or_default()
    }

    /// A database the tenant role owns and nothing protects.
    fn droppable() -> DropFacts {
        DropFacts {
            owner: OWNER.to_owned(),
            template: false,
            session_database: false,
            session_role: MAINTENANCE.to_owned(),
            owner_exists: true,
        }
    }

    #[test]
    fn the_drop_decision_has_five_outcomes() {
        assert_eq!(drop_decision(None, OWNER), DropDecision::Absent);

        assert_eq!(
            drop_decision(Some(&droppable()), OWNER),
            DropDecision::DropAsOwner
        );

        for protected in [
            DropFacts {
                template: true,
                ..droppable()
            },
            DropFacts {
                session_database: true,
                ..droppable()
            },
            DropFacts {
                session_role: OWNER.to_owned(),
                ..droppable()
            },
        ] {
            assert!(matches!(
                drop_decision(Some(&protected), OWNER),
                DropDecision::Refused(_)
            ));
        }

        let foreign = DropFacts {
            owner: "orders_owner".to_owned(),
            ..droppable()
        };
        assert!(matches!(
            drop_decision(Some(&foreign), OWNER),
            DropDecision::Refused(_)
        ));

        let left = DropFacts {
            owner_exists: false,
            ..foreign
        };
        assert_eq!(drop_decision(Some(&left), OWNER), DropDecision::LeftAlone);
    }

    #[test]
    fn another_owner_is_refused_even_when_it_is_the_session_role() {
        let maintenance_owned = DropFacts {
            owner: MAINTENANCE.to_owned(),
            ..droppable()
        };
        assert!(matches!(
            drop_decision(Some(&maintenance_owned), OWNER),
            DropDecision::Refused(_)
        ));
    }

    #[test]
    fn only_insufficient_privilege_is_reclassified() {
        assert!(is_insufficient_privilege(Some("42501")));
        assert!(!is_insufficient_privilege(Some("55006")));
        assert!(!is_insufficient_privilege(None));
        let error = switch_error(sqlx::Error::Protocol("unexpected message".to_owned()));
        assert_eq!(error.code(), ErrorCode::Internal);
    }

    #[test]
    fn the_drop_error_wins_and_only_a_reset_lifts_the_poison() {
        let reset_failed = || sqlx::Error::Protocol("the reset failed".to_owned());

        let (answer, lifted) = settle(Ok(()), Ok(()));
        answer.expect("dropped and reset");
        assert!(lifted);

        let (answer, lifted) = settle(Err(AppError::unavailable("drop")), Ok(()));
        assert_eq!(
            answer.expect_err("the drop failed").code(),
            ErrorCode::Unavailable
        );
        assert!(lifted);

        let (answer, lifted) = settle(Ok(()), Err(reset_failed()));
        assert_eq!(
            answer.expect_err("the reset failed").code(),
            ErrorCode::Internal
        );
        assert!(!lifted);

        let (answer, lifted) = settle(Err(AppError::unavailable("drop")), Err(reset_failed()));
        assert_eq!(
            answer.expect_err("both failed").code(),
            ErrorCode::Unavailable
        );
        assert!(!lifted);
    }

    #[test]
    fn the_size_is_read_as_the_owner_or_the_session_role() {
        let names = names();
        assert_eq!(
            size_role(&names, MAINTENANCE, MAINTENANCE).expect("own"),
            None
        );
        assert_eq!(
            size_role(&names, OWNER, MAINTENANCE).expect("a tenant role"),
            Some(names.role(tenant()))
        );
        for other in [
            "orders_owner",
            "app_t_short",
            "billing_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d",
        ] {
            let error = size_role(&names, other, MAINTENANCE).expect_err("another owner");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), None);
        }
    }

    #[test]
    fn a_missing_or_negative_size_is_internal() {
        assert_eq!(size_bytes(Some(7_340_032)).expect("a size"), 7_340_032);
        assert_eq!(size_bytes(Some(0)).expect("a size"), 0);
        for raw in [None, Some(-1)] {
            let error = size_bytes(raw).expect_err("not a size");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), None);
        }
    }

    fn entry(grantee: Grantee, privilege: DatabasePrivilege, grantable: bool) -> DatabaseGrant {
        DatabaseGrant {
            grantee,
            privilege,
            grantable,
        }
    }

    fn owner_entry(privilege: DatabasePrivilege) -> DatabaseGrant {
        entry(Grantee::Role(OWNER.to_owned()), privilege, true)
    }

    /// A database `owner` owns with `grants` as its ACL.
    fn owned(owner: &str, grants: Vec<DatabaseGrant>) -> DatabaseFacts {
        DatabaseFacts {
            name: OWNER.to_owned(),
            owner: owner.to_owned(),
            template: false,
            grants,
        }
    }

    /// The ACL the restriction leaves: the owner's privileges and `PUBLIC`'s
    /// `TEMPORARY`.
    fn restricted_acl() -> Vec<DatabaseGrant> {
        vec![
            entry(Grantee::Public, DatabasePrivilege::Temporary, false),
            owner_entry(DatabasePrivilege::Create),
            owner_entry(DatabasePrivilege::Temporary),
            owner_entry(DatabasePrivilege::Connect),
        ]
    }

    #[test]
    fn a_restricted_database_passes_its_read() {
        let facts = owned(OWNER, restricted_acl());
        assert_eq!(
            restricted_findings(&facts, OWNER),
            [] as [DatabaseFinding; 0]
        );
        restricted_verdict(OWNER, OWNER, Some(&facts)).expect("as the restriction left it");
    }

    #[test]
    fn public_connect_after_the_restriction_is_a_finding() {
        let left = entry(Grantee::Public, DatabasePrivilege::Connect, false);
        let mut grants = restricted_acl();
        grants.push(left.clone());
        let facts = owned(OWNER, grants);
        assert_eq!(
            tenant_database_findings(&facts, OWNER),
            [] as [DatabaseFinding; 0],
            "the tenant rules allow it"
        );
        assert_eq!(
            restricted_findings(&facts, OWNER),
            [DatabaseFinding::Grant(left)]
        );
        let error = restricted_verdict(OWNER, OWNER, Some(&facts)).expect_err("PUBLIC may connect");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(error.message(), "internal error");
        assert_eq!(
            source(&error),
            format!(
                "tenant database {OWNER} is not as expected after its restriction: PUBLIC holds \
                 CONNECT"
            )
        );
    }

    #[test]
    fn every_finding_of_the_tenant_rules_counts_after_the_restriction() {
        let mut grants = restricted_acl();
        grants.push(entry(Grantee::Public, DatabasePrivilege::Create, false));
        grants.push(entry(
            Grantee::Role("orders_reader".to_owned()),
            DatabasePrivilege::Connect,
            true,
        ));
        let facts = DatabaseFacts {
            template: true,
            ..owned(MAINTENANCE, grants)
        };
        let error = restricted_verdict(OWNER, OWNER, Some(&facts)).expect_err("drift");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        let why = source(&error);
        for part in [
            "it is owned by orders_maintenance",
            "it is a template",
            "PUBLIC holds CREATE",
            "role orders_reader holds CONNECT with grant option",
        ] {
            assert!(why.contains(part), "{why}");
        }
        assert!(!why.contains("PUBLIC holds CONNECT"), "{why}");
    }

    #[test]
    fn a_public_grant_option_is_named_once() {
        let mut grants = restricted_acl();
        let option = entry(Grantee::Public, DatabasePrivilege::Connect, true);
        grants.push(option.clone());
        assert_eq!(
            restricted_findings(&owned(OWNER, grants), OWNER),
            [DatabaseFinding::Grant(option)]
        );
    }

    #[test]
    fn a_database_gone_after_its_restriction_is_internal() {
        let error = restricted_verdict(OWNER, OWNER, None).expect_err("gone");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert!(
            source(&error).contains("does not exist"),
            "{}",
            source(&error)
        );
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn the_read_after_the_restriction_sees_what_the_owners_revoke_leaves() {
        let tenant = TenantId::from_uuid(Uuid::new_v4());
        let role = names().role(tenant);
        let database = names().database(tenant);
        let grantor = format!("{}_grantor", role.as_str());
        let roles = format!("CREATE ROLE {}; CREATE ROLE \"{grantor}\"", role.quoted());
        let Some((options, _)) = testing::public_first(|_| roles).await else {
            return;
        };
        timeout(testing::SCENARIO_TIMEOUT, async {
            let mut admin = PgConnection::connect_with(&options)
                .await
                .expect("the superuser connected");
            testing::execute(
                &mut admin,
                &format!(
                    "CREATE DATABASE {} OWNER {}",
                    database.quoted(),
                    role.quoted()
                ),
            )
            .await;
            let error = check_restricted(&mut admin, &database, &role)
                .await
                .expect_err("the default ACL gives PUBLIC CONNECT");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(
                source(&error).ends_with(": PUBLIC holds CONNECT"),
                "{}",
                source(&error)
            );

            testing::execute(&mut admin, &sql::revoke_connect_from_public(&database)).await;
            check_restricted(&mut admin, &database, &role)
                .await
                .expect("the owner's revoke restricted it");

            // A role with a grant option grants PUBLIC a CONNECT of its own,
            // which the owner's revoke leaves in place.
            testing::execute(
                &mut admin,
                &format!(
                    "GRANT CONNECT ON DATABASE {} TO \"{grantor}\" WITH GRANT OPTION",
                    database.quoted()
                ),
            )
            .await;
            testing::execute(
                &mut admin,
                &format!(
                    "SET ROLE \"{grantor}\"; GRANT CONNECT ON DATABASE {} TO PUBLIC; RESET ROLE",
                    database.quoted()
                ),
            )
            .await;
            testing::execute(&mut admin, &sql::revoke_connect_from_public(&database)).await;
            let error = check_restricted(&mut admin, &database, &role)
                .await
                .expect_err("PUBLIC keeps the grantor's CONNECT");
            let why = source(&error);
            assert!(
                why.contains(&format!("role {grantor} holds CONNECT with grant option")),
                "{why}"
            );
            assert!(why.ends_with("; PUBLIC holds CONNECT"), "{why}");
            admin.close().await.expect("the session closed");
        })
        .await
        .expect("the scenario finished within its 30 s guard");
    }

    #[tokio::test]
    async fn arguments_are_refused_before_the_connection_is_looked_at() {
        let names = names();
        let mut conn = MaintenanceConn::detached(names.clone(), MAINTENANCE);
        conn.mark_role_switched();
        let role = names.role(tenant());
        let database = names.database(tenant());
        let foreign = ObjectNames::new("billing_t_")
            .expect("the prefix is valid")
            .role(tenant());
        let adopted = DatabaseName::parse("orders").expect("a valid name");

        let error = create(&mut conn, &adopted, &role)
            .await
            .expect_err("not a generated name");
        assert!(source(&error).contains("not an object name"));
        let error = create(&mut conn, &database, &foreign)
            .await
            .expect_err("another prefix");
        assert!(source(&error).contains("not an object name"));
        let error = drop(&mut conn, &adopted, &foreign)
            .await
            .expect_err("another prefix");
        assert!(source(&error).contains("not an object name"));

        for error in [
            create(&mut conn, &database, &role).await,
            drop(&mut conn, &adopted, &role).await,
            size(&mut conn, &database).await.map(|_| ()),
            access(&mut conn, database.as_str()).await.map(|_| ()),
        ] {
            let error = error.expect_err("the connection is poisoned");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(source(&error).contains("poisoned"), "{}", source(&error));
        }
    }
}
