//! Privilege inspection: the one place that reads role and database
//! privileges.
//!
//! Every read is static SQL with bound names on the connection it is given,
//! changes nothing and needs only catalog access. Every comparison has an
//! operator of exactly its two types in `pg_catalog` (`oid`, `name` with
//! `text`, `text`), and every cast and function is qualified. Any membership
//! edge is a path, whatever its options, and chains are followed to their
//! end.
//!
//! The reads ([`role_facts`], [`database_facts`], [`role_may_connect`],
//! [`roles_that_may_connect`], [`public_may_connect`]) are one statement
//! each. The rules ([`tenant_role_findings`], [`tenant_database_findings`])
//! are pure, and [`check_tenant_role`] and [`check_tenant_database`] join the
//! two: any finding is `INTERNAL` without a reason, the findings in the
//! error's source. Nothing here repairs what it finds.

use std::fmt;

use sekvent_db::to_app_error;
use sekvent_error::AppError;
use sqlx::PgConnection;

use crate::{DatabaseName, TenantRole};

/// A role's attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RoleAttributes {
    /// `SUPERUSER`.
    pub superuser: bool,
    /// `INHERIT`.
    pub inherit: bool,
    /// `CREATEROLE`.
    pub create_role: bool,
    /// `CREATEDB`.
    pub create_db: bool,
    /// `LOGIN`.
    pub login: bool,
    /// `REPLICATION`.
    pub replication: bool,
    /// `BYPASSRLS`.
    pub bypass_rls: bool,
}

impl RoleAttributes {
    /// The SQL names of the attributes held that a tenant role never has:
    /// `SUPERUSER`, `INHERIT`, `CREATEROLE`, `CREATEDB`, `REPLICATION` and
    /// `BYPASSRLS`, in that order. `LOGIN` is not among them.
    pub fn unexpected_for_tenant(&self) -> Vec<&'static str> {
        [
            (self.superuser, "SUPERUSER"),
            (self.inherit, "INHERIT"),
            (self.create_role, "CREATEROLE"),
            (self.create_db, "CREATEDB"),
            (self.replication, "REPLICATION"),
            (self.bypass_rls, "BYPASSRLS"),
        ]
        .into_iter()
        .filter_map(|(held, name)| held.then_some(name))
        .collect()
    }
}

/// One role membership: `member` is a member of `role`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipEdge {
    /// The role granted.
    pub role: String,
    /// The role it is granted to.
    pub member: String,
    /// `WITH ADMIN`.
    pub admin_option: bool,
    /// `WITH INHERIT`.
    pub inherit_option: bool,
    /// `WITH SET`.
    pub set_option: bool,
}

/// What the catalogs say about one role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleFacts {
    /// The role name.
    pub name: String,
    /// Its attributes.
    pub attributes: RoleAttributes,
    /// Every edge naming the role on either side.
    pub edges: Vec<MembershipEdge>,
    /// Every role that reaches it through a chain, sorted.
    pub reaching: Vec<String>,
    /// Every role it reaches through a chain, sorted.
    pub reached: Vec<String>,
}

/// A database privilege.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum DatabasePrivilege {
    /// `CONNECT`.
    Connect,
    /// `TEMPORARY`.
    Temporary,
    /// `CREATE`.
    Create,
}

impl DatabasePrivilege {
    /// The privilege's SQL name: `CONNECT`, `TEMPORARY` or `CREATE`.
    pub fn sql_name(self) -> &'static str {
        match self {
            Self::Connect => "CONNECT",
            Self::Temporary => "TEMPORARY",
            Self::Create => "CREATE",
        }
    }
}

/// Who holds a grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grantee {
    /// Every role.
    Public,
    /// One role, by name.
    Role(String),
}

/// One entry of a database's ACL, one privilege each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseGrant {
    /// Who holds it.
    pub grantee: Grantee,
    /// What it allows.
    pub privilege: DatabasePrivilege,
    /// `WITH GRANT OPTION`.
    pub grantable: bool,
}

/// What the catalogs say about one database. A database without an ACL is
/// read as its defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseFacts {
    /// The database name.
    pub name: String,
    /// The owner's role name.
    pub owner: String,
    /// Whether it is a template.
    pub template: bool,
    /// Its ACL, one privilege per entry.
    pub grants: Vec<DatabaseGrant>,
}

/// What is wrong with an existing tenant role.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoleFinding {
    /// It holds this attribute (its SQL name).
    Attribute(&'static str),
    /// It reaches this role.
    Reaches(String),
    /// This role, not the maintenance role, reaches it.
    ReachedBy(String),
    /// The maintenance role's membership has `INHERIT`.
    MaintenanceInherits,
}

/// What is wrong with an existing tenant database.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DatabaseFinding {
    /// It is owned by this role instead.
    Owner(String),
    /// It is a template.
    Template,
    /// Its ACL holds this entry beyond the owner's and `PUBLIC`'s defaults.
    Grant(DatabaseGrant),
}

/// The role, its direct edges and both closures in one statement, so the
/// facts come from one snapshot. Row kinds: 0 the role itself (the second
/// name is the session's role), 1 a direct edge (role, member), 2 a role
/// that reaches it, 3 a role it reaches. `UNION` in the recursive terms
/// ends every chain, cycles included.
const ROLE_FACTS: &str = "\
WITH RECURSIVE
target AS (
    SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = $1
),
reaching(oid) AS (
    SELECT m.member FROM pg_catalog.pg_auth_members m JOIN target t ON m.roleid = t.oid
    UNION
    SELECT m.member FROM pg_catalog.pg_auth_members m JOIN reaching x ON m.roleid = x.oid
),
reached(oid) AS (
    SELECT m.roleid FROM pg_catalog.pg_auth_members m JOIN target t ON m.member = t.oid
    UNION
    SELECT m.roleid FROM pg_catalog.pg_auth_members m JOIN reached x ON m.member = x.oid
)
SELECT 0::pg_catalog.int4, r.rolname::pg_catalog.text, session_user::pg_catalog.text,
       r.rolsuper, r.rolinherit, r.rolcreaterole, r.rolcreatedb,
       r.rolcanlogin, r.rolreplication, r.rolbypassrls
FROM pg_catalog.pg_roles r JOIN target t ON r.oid = t.oid
UNION ALL
SELECT 1, g.rolname::pg_catalog.text, u.rolname::pg_catalog.text,
       m.admin_option, m.inherit_option, m.set_option, false, false, false, false
FROM pg_catalog.pg_auth_members m
JOIN target t ON t.oid IN (m.roleid, m.member)
JOIN pg_catalog.pg_roles g ON g.oid = m.roleid
JOIN pg_catalog.pg_roles u ON u.oid = m.member
UNION ALL
SELECT 2, r.rolname::pg_catalog.text, NULL, false, false, false, false, false, false, false
FROM reaching x JOIN pg_catalog.pg_roles r ON r.oid = x.oid
UNION ALL
SELECT 3, r.rolname::pg_catalog.text, NULL, false, false, false, false, false, false, false
FROM reached x JOIN pg_catalog.pg_roles r ON r.oid = x.oid";

/// One row of [`ROLE_FACTS`].
type RoleRow = (
    i32,
    String,
    Option<String>,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
);

/// The database, its owner, the template flag and the ACL exploded to one
/// row per privilege. A NULL ACL is read as `acldefault`: the owner's
/// privileges and `PUBLIC`'s `CONNECT` and `TEMPORARY`. An empty ACL gives
/// one row without a privilege. A NULL grantee name is `PUBLIC`.
const DATABASE_FACTS: &str = "\
SELECT d.datname::pg_catalog.text, pg_catalog.pg_get_userbyid(d.datdba)::pg_catalog.text,
       d.datistemplate,
       CASE WHEN a.grantee = 0::pg_catalog.oid THEN NULL
            ELSE pg_catalog.pg_get_userbyid(a.grantee)::pg_catalog.text END,
       a.privilege_type, a.is_grantable
FROM pg_catalog.pg_database d
LEFT JOIN LATERAL pg_catalog.aclexplode(COALESCE(d.datacl, pg_catalog.acldefault('d', d.datdba)))
    WITH ORDINALITY AS a(grantor, grantee, privilege_type, is_grantable, n) ON true
WHERE d.datname = $1
ORDER BY a.n";

/// One row of [`DATABASE_FACTS`].
type DatabaseRow = (
    String,
    String,
    bool,
    Option<String>,
    Option<String>,
    Option<bool>,
);

/// The effective `CONNECT` of one role, as `has_database_privilege`
/// computes it: an own grant, `PUBLIC`'s, an inherited membership's, or a
/// superuser's. No row when the role or the database is missing.
const ROLE_MAY_CONNECT: &str = "\
SELECT pg_catalog.has_database_privilege(r.oid, d.oid, 'CONNECT')
FROM pg_catalog.pg_roles r CROSS JOIN pg_catalog.pg_database d
WHERE r.rolname = $1 AND d.datname = $2";

/// Every `LOGIN` role with an effective `CONNECT`; one NULL row when the
/// database exists and none may connect, no row when it is missing.
const ROLES_THAT_MAY_CONNECT: &str = "\
SELECT r.rolname::pg_catalog.text
FROM pg_catalog.pg_database d
LEFT JOIN pg_catalog.pg_roles r
    ON r.rolcanlogin AND pg_catalog.has_database_privilege(r.oid, d.oid, 'CONNECT')
WHERE d.datname = $1";

/// Whether the ACL (or its default) gives `PUBLIC` `CONNECT`.
const PUBLIC_MAY_CONNECT: &str = "\
SELECT EXISTS (
    SELECT 1
    FROM pg_catalog.aclexplode(COALESCE(d.datacl, pg_catalog.acldefault('d', d.datdba))) a
    WHERE a.grantee = 0::pg_catalog.oid AND a.privilege_type = 'CONNECT'
)
FROM pg_catalog.pg_database d
WHERE d.datname = $1";

/// Every catalog statement of this module, by name, for the scan of
/// `cluster::tests`.
#[cfg(test)]
pub(super) const STATEMENTS: [(&str, &str); 5] = [
    ("ROLE_FACTS", ROLE_FACTS),
    ("DATABASE_FACTS", DATABASE_FACTS),
    ("ROLE_MAY_CONNECT", ROLE_MAY_CONNECT),
    ("ROLES_THAT_MAY_CONNECT", ROLES_THAT_MAY_CONNECT),
    ("PUBLIC_MAY_CONNECT", PUBLIC_MAY_CONNECT),
];

/// The facts of `role`, `None` when there is no such role. One statement.
pub async fn role_facts(
    conn: &mut PgConnection,
    role: &str,
) -> Result<Option<RoleFacts>, AppError> {
    Ok(read_role(conn, role).await?.map(|(facts, _)| facts))
}

/// The facts of `database`, `None` when there is no such database. One
/// statement; a privilege the crate does not know is `INTERNAL`.
pub async fn database_facts(
    conn: &mut PgConnection,
    database: &str,
) -> Result<Option<DatabaseFacts>, AppError> {
    let rows: Vec<DatabaseRow> = sqlx::query_as(DATABASE_FACTS)
        .bind(database)
        .fetch_all(&mut *conn)
        .await
        .map_err(to_app_error)?;
    database_facts_from_rows(rows)
}

/// Whether `role` holds an effective `CONNECT` on `database`; `None` when
/// either is missing. It says what the ACL allows, not whether a sign-in
/// succeeds (`LOGIN`, `ALLOW_CONNECTIONS` and host rules decide that too).
pub async fn role_may_connect(
    conn: &mut PgConnection,
    role: &str,
    database: &str,
) -> Result<Option<bool>, AppError> {
    sqlx::query_scalar(ROLE_MAY_CONNECT)
        .bind(role)
        .bind(database)
        .fetch_optional(&mut *conn)
        .await
        .map_err(to_app_error)
}

/// Every `LOGIN` role with an effective `CONNECT` on `database`, sorted by
/// name; `None` when there is no such database.
pub async fn roles_that_may_connect(
    conn: &mut PgConnection,
    database: &str,
) -> Result<Option<Vec<String>>, AppError> {
    let rows: Vec<Option<String>> = sqlx::query_scalar(ROLES_THAT_MAY_CONNECT)
        .bind(database)
        .fetch_all(&mut *conn)
        .await
        .map_err(to_app_error)?;
    Ok(connecting_from_rows(rows))
}

/// Whether `PUBLIC` holds `CONNECT` on `database`; `None` when there is no
/// such database. `TEMPORARY` alone is not `CONNECT`.
pub async fn public_may_connect(
    conn: &mut PgConnection,
    database: &str,
) -> Result<Option<bool>, AppError> {
    sqlx::query_scalar(PUBLIC_MAY_CONNECT)
        .bind(database)
        .fetch_optional(&mut *conn)
        .await
        .map_err(to_app_error)
}

/// Pure: what is wrong with an existing tenant role, given the name of the
/// maintenance role. In order: the unexpected attributes, every role it
/// reaches, every role but the maintenance role that reaches it (a member
/// of the maintenance role reaches it too), and an `INHERIT` on the
/// maintenance role's membership. `LOGIN` is not checked.
pub fn tenant_role_findings(facts: &RoleFacts, maintenance: &str) -> Vec<RoleFinding> {
    let mut found: Vec<RoleFinding> = facts
        .attributes
        .unexpected_for_tenant()
        .into_iter()
        .map(RoleFinding::Attribute)
        .collect();
    found.extend(facts.reached.iter().cloned().map(RoleFinding::Reaches));
    found.extend(
        facts
            .reaching
            .iter()
            .filter(|role| role.as_str() != maintenance)
            .cloned()
            .map(RoleFinding::ReachedBy),
    );
    let inherits = facts
        .edges
        .iter()
        .any(|edge| edge.role == facts.name && edge.member == maintenance && edge.inherit_option);
    if inherits {
        found.push(RoleFinding::MaintenanceInherits);
    }
    found
}

/// Pure: what is wrong with an existing tenant database that `owner` should
/// own. In order: another owner, the template flag, and every ACL entry
/// that is neither `owner`'s nor `PUBLIC`'s `CONNECT` or `TEMPORARY`
/// without grant option. A grant to the maintenance role is a finding like
/// any other.
pub fn tenant_database_findings(facts: &DatabaseFacts, owner: &str) -> Vec<DatabaseFinding> {
    let mut found = Vec::new();
    if facts.owner != owner {
        found.push(DatabaseFinding::Owner(facts.owner.clone()));
    }
    if facts.template {
        found.push(DatabaseFinding::Template);
    }
    found.extend(
        facts
            .grants
            .iter()
            .filter(|grant| !expected_grant(grant, owner))
            .cloned()
            .map(DatabaseFinding::Grant),
    );
    found
}

/// `INTERNAL` naming every finding in its source; the session's role is the
/// maintenance role. A missing role is `INTERNAL` too.
pub async fn check_tenant_role(conn: &mut PgConnection, role: &TenantRole) -> Result<(), AppError> {
    let read = read_role(conn, role.as_str()).await?;
    role_verdict(role.as_str(), read.as_ref())
}

/// [`check_tenant_role`] for a role that may not exist yet: a missing role
/// passes, an existing one is held to the same rules.
#[cfg(feature = "lifecycle")]
pub(crate) async fn check_existing_tenant_role(
    conn: &mut PgConnection,
    role: &TenantRole,
) -> Result<(), AppError> {
    match read_role(conn, role.as_str()).await? {
        None => Ok(()),
        read => role_verdict(role.as_str(), read.as_ref()),
    }
}

/// `INTERNAL` naming every finding in its source. A missing database is
/// `INTERNAL` too.
pub async fn check_tenant_database(
    conn: &mut PgConnection,
    database: &DatabaseName,
    owner: &TenantRole,
) -> Result<(), AppError> {
    let facts = database_facts(conn, database.as_str()).await?;
    database_verdict(database.as_str(), owner.as_str(), facts.as_ref())
}

/// The facts of `role` and the session's role, from one statement.
async fn read_role(
    conn: &mut PgConnection,
    role: &str,
) -> Result<Option<(RoleFacts, String)>, AppError> {
    let rows: Vec<RoleRow> = sqlx::query_as(ROLE_FACTS)
        .bind(role)
        .fetch_all(&mut *conn)
        .await
        .map_err(to_app_error)?;
    Ok(role_facts_from_rows(rows))
}

/// Assembles [`ROLE_FACTS`] rows; `None` without the role's own row.
fn role_facts_from_rows(rows: Vec<RoleRow>) -> Option<(RoleFacts, String)> {
    let mut own = None;
    let mut edges = Vec::new();
    let mut reaching = Vec::new();
    let mut reached = Vec::new();
    for row in rows {
        match row.0 {
            0 => {
                let attributes = RoleAttributes {
                    superuser: row.3,
                    inherit: row.4,
                    create_role: row.5,
                    create_db: row.6,
                    login: row.7,
                    replication: row.8,
                    bypass_rls: row.9,
                };
                own = Some((row.1, attributes, row.2.unwrap_or_default()));
            }
            1 => edges.push(MembershipEdge {
                role: row.1,
                member: row.2.unwrap_or_default(),
                admin_option: row.3,
                inherit_option: row.4,
                set_option: row.5,
            }),
            2 => reaching.push(row.1),
            _ => reached.push(row.1),
        }
    }
    let (name, attributes, session) = own?;
    edges.sort_by(|a, b| {
        (
            &a.role,
            &a.member,
            a.admin_option,
            a.inherit_option,
            a.set_option,
        )
            .cmp(&(
                &b.role,
                &b.member,
                b.admin_option,
                b.inherit_option,
                b.set_option,
            ))
    });
    reaching.sort();
    reaching.dedup();
    reached.sort();
    reached.dedup();
    let facts = RoleFacts {
        name,
        attributes,
        edges,
        reaching,
        reached,
    };
    Some((facts, session))
}

/// Assembles [`DATABASE_FACTS`] rows; `None` without a row.
fn database_facts_from_rows(rows: Vec<DatabaseRow>) -> Result<Option<DatabaseFacts>, AppError> {
    let mut facts: Option<DatabaseFacts> = None;
    for (name, owner, template, grantee, privilege, grantable) in rows {
        let entry = facts.get_or_insert_with(|| DatabaseFacts {
            name,
            owner,
            template,
            grants: Vec::new(),
        });
        if let Some(privilege) = privilege {
            entry.grants.push(DatabaseGrant {
                grantee: grantee.map_or(Grantee::Public, Grantee::Role),
                privilege: parse_privilege(&privilege)?,
                grantable: grantable.unwrap_or(false),
            });
        }
    }
    Ok(facts)
}

/// The names of [`ROLES_THAT_MAY_CONNECT`] rows, sorted; `None` without a
/// row.
fn connecting_from_rows(rows: Vec<Option<String>>) -> Option<Vec<String>> {
    if rows.is_empty() {
        return None;
    }
    let mut names: Vec<String> = rows.into_iter().flatten().collect();
    names.sort();
    names.dedup();
    Some(names)
}

/// A privilege name of `aclexplode`. Any other is `INTERNAL`: an unknown
/// privilege is never ignored.
fn parse_privilege(name: &str) -> Result<DatabasePrivilege, AppError> {
    match name {
        "CONNECT" => Ok(DatabasePrivilege::Connect),
        "TEMPORARY" => Ok(DatabasePrivilege::Temporary),
        "CREATE" => Ok(DatabasePrivilege::Create),
        other => Err(AppError::internal(format!(
            "the catalog reports a database privilege this crate does not know: {}",
            sekvent_telemetry::truncate_for_log(other, 64)
        ))),
    }
}

/// An ACL entry a tenant database may hold: any of the owner's, and
/// `PUBLIC`'s `CONNECT` or `TEMPORARY` without grant option.
fn expected_grant(grant: &DatabaseGrant, owner: &str) -> bool {
    match &grant.grantee {
        Grantee::Role(role) => role == owner,
        Grantee::Public => {
            !grant.grantable
                && matches!(
                    grant.privilege,
                    DatabasePrivilege::Connect | DatabasePrivilege::Temporary
                )
        }
    }
}

/// The outcome of [`check_tenant_role`] for what was read.
fn role_verdict(role: &str, read: Option<&(RoleFacts, String)>) -> Result<(), AppError> {
    let Some((facts, maintenance)) = read else {
        return Err(AppError::internal(format!(
            "tenant role {role} does not exist"
        )));
    };
    let found = tenant_role_findings(facts, maintenance);
    if found.is_empty() {
        return Ok(());
    }
    let described: Vec<String> = found.iter().map(describe_role_finding).collect();
    Err(AppError::internal(format!(
        "tenant role {role} is not as expected: {}",
        described.join("; ")
    )))
}

/// The outcome of [`check_tenant_database`] for what was read.
fn database_verdict(
    database: &str,
    owner: &str,
    facts: Option<&DatabaseFacts>,
) -> Result<(), AppError> {
    let Some(facts) = facts else {
        return Err(AppError::internal(format!(
            "tenant database {database} does not exist"
        )));
    };
    let found = tenant_database_findings(facts, owner);
    if found.is_empty() {
        return Ok(());
    }
    let described: Vec<String> = found
        .iter()
        .map(|finding| describe_database_finding(finding, owner))
        .collect();
    Err(AppError::internal(format!(
        "tenant database {database} is not as expected: {}",
        described.join("; ")
    )))
}

/// `it holds SUPERUSER`, `it reaches role x`, `role x reaches it`, `the
/// maintenance role's membership has INHERIT`.
impl fmt::Display for RoleFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&describe_role_finding(self))
    }
}

/// `it is owned by x`, `it is a template`, `PUBLIC holds CREATE`, `role x
/// holds CONNECT with grant option`.
impl fmt::Display for DatabaseFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owner(actual) => write!(f, "it is owned by {actual}"),
            Self::Template => f.write_str("it is a template"),
            Self::Grant(grant) => f.write_str(&describe_grant(grant)),
        }
    }
}

fn describe_role_finding(finding: &RoleFinding) -> String {
    match finding {
        RoleFinding::Attribute(attribute) => format!("it holds {attribute}"),
        RoleFinding::Reaches(role) => format!("it reaches role {role}"),
        RoleFinding::ReachedBy(role) => format!("role {role} reaches it"),
        RoleFinding::MaintenanceInherits => {
            "the maintenance role's membership has INHERIT".to_owned()
        }
    }
}

fn describe_database_finding(finding: &DatabaseFinding, owner: &str) -> String {
    match finding {
        DatabaseFinding::Owner(_) => format!("{finding}, not {owner}"),
        _ => finding.to_string(),
    }
}

/// `PUBLIC holds CREATE`, `role x holds CONNECT with grant option`.
fn describe_grant(grant: &DatabaseGrant) -> String {
    let holder = match &grant.grantee {
        Grantee::Public => "PUBLIC".to_owned(),
        Grantee::Role(role) => format!("role {role}"),
    };
    let option = if grant.grantable {
        " with grant option"
    } else {
        ""
    };
    format!("{holder} holds {}{option}", grant.privilege.sql_name())
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use sekvent_error::ErrorCode;

    use super::*;

    const MAINTENANCE: &str = "orders_maintenance";
    const TENANT: &str = "app_t_0123456789abcdef0123456789abcdef";

    fn clean_role() -> RoleFacts {
        RoleFacts {
            name: TENANT.to_owned(),
            attributes: RoleAttributes {
                login: true,
                ..RoleAttributes::default()
            },
            edges: vec![MembershipEdge {
                role: TENANT.to_owned(),
                member: MAINTENANCE.to_owned(),
                admin_option: true,
                inherit_option: false,
                set_option: true,
            }],
            reaching: vec![MAINTENANCE.to_owned()],
            reached: vec![],
        }
    }

    fn grant(grantee: Grantee, privilege: DatabasePrivilege, grantable: bool) -> DatabaseGrant {
        DatabaseGrant {
            grantee,
            privilege,
            grantable,
        }
    }

    fn role(name: &str) -> Grantee {
        Grantee::Role(name.to_owned())
    }

    fn clean_database() -> DatabaseFacts {
        DatabaseFacts {
            name: TENANT.to_owned(),
            owner: TENANT.to_owned(),
            template: false,
            grants: vec![
                grant(Grantee::Public, DatabasePrivilege::Temporary, false),
                grant(Grantee::Public, DatabasePrivilege::Connect, false),
                grant(role(TENANT), DatabasePrivilege::Create, false),
                grant(role(TENANT), DatabasePrivilege::Temporary, false),
                grant(role(TENANT), DatabasePrivilege::Connect, true),
            ],
        }
    }

    fn source_of(error: &AppError) -> String {
        error.source().map(ToString::to_string).unwrap_or_default()
    }

    #[test]
    fn no_attribute_is_unexpected_by_default_and_login_never_is() {
        assert_eq!(
            RoleAttributes::default().unexpected_for_tenant(),
            [] as [&str; 0]
        );
        let login = RoleAttributes {
            login: true,
            ..RoleAttributes::default()
        };
        assert_eq!(login.unexpected_for_tenant(), [] as [&str; 0]);
    }

    #[test]
    fn every_widening_attribute_is_named_in_a_fixed_order() {
        let all = RoleAttributes {
            superuser: true,
            inherit: true,
            create_role: true,
            create_db: true,
            login: true,
            replication: true,
            bypass_rls: true,
        };
        assert_eq!(
            all.unexpected_for_tenant(),
            [
                "SUPERUSER",
                "INHERIT",
                "CREATEROLE",
                "CREATEDB",
                "REPLICATION",
                "BYPASSRLS"
            ]
        );
    }

    #[test]
    fn a_clean_tenant_role_has_no_finding() {
        assert_eq!(
            tenant_role_findings(&clean_role(), MAINTENANCE),
            [] as [RoleFinding; 0]
        );
    }

    #[test]
    fn each_attribute_alone_is_a_finding() {
        type Case = (fn(&mut RoleAttributes), &'static str);
        let cases: [Case; 6] = [
            (|a| a.superuser = true, "SUPERUSER"),
            (|a| a.inherit = true, "INHERIT"),
            (|a| a.create_role = true, "CREATEROLE"),
            (|a| a.create_db = true, "CREATEDB"),
            (|a| a.replication = true, "REPLICATION"),
            (|a| a.bypass_rls = true, "BYPASSRLS"),
        ];
        for (set, name) in cases {
            let mut facts = clean_role();
            set(&mut facts.attributes);
            assert_eq!(
                tenant_role_findings(&facts, MAINTENANCE),
                [RoleFinding::Attribute(name)],
                "{name}"
            );
        }
    }

    #[test]
    fn a_reached_role_is_a_finding() {
        let mut facts = clean_role();
        facts.reached = vec!["billing_reader".to_owned()];
        assert_eq!(
            tenant_role_findings(&facts, MAINTENANCE),
            [RoleFinding::Reaches("billing_reader".to_owned())]
        );
    }

    #[test]
    fn a_reaching_role_other_than_the_maintenance_role_is_a_finding() {
        let mut facts = clean_role();
        facts.reaching = vec!["inventory_admin".to_owned(), MAINTENANCE.to_owned()];
        assert_eq!(
            tenant_role_findings(&facts, MAINTENANCE),
            [RoleFinding::ReachedBy("inventory_admin".to_owned())]
        );
    }

    #[test]
    fn a_member_of_the_maintenance_role_reaches_the_tenant_role() {
        // The member reaches the tenant through the maintenance role, so the
        // closure names it next to the maintenance role.
        let mut facts = clean_role();
        facts.reaching = vec![MAINTENANCE.to_owned(), "orders_operator".to_owned()];
        assert_eq!(
            tenant_role_findings(&facts, MAINTENANCE),
            [RoleFinding::ReachedBy("orders_operator".to_owned())]
        );
    }

    #[test]
    fn an_inheriting_maintenance_membership_is_a_finding() {
        let mut facts = clean_role();
        facts.edges[0].inherit_option = true;
        assert_eq!(
            tenant_role_findings(&facts, MAINTENANCE),
            [RoleFinding::MaintenanceInherits]
        );
    }

    #[test]
    fn an_inheriting_edge_of_another_member_is_not_the_maintenance_finding() {
        let mut facts = clean_role();
        facts.edges.push(MembershipEdge {
            role: TENANT.to_owned(),
            member: "orders_operator".to_owned(),
            admin_option: false,
            inherit_option: true,
            set_option: false,
        });
        facts.reaching.push("orders_operator".to_owned());
        assert_eq!(
            tenant_role_findings(&facts, MAINTENANCE),
            [RoleFinding::ReachedBy("orders_operator".to_owned())]
        );
    }

    #[test]
    fn findings_of_one_role_come_in_a_fixed_order() {
        let mut facts = clean_role();
        facts.attributes.superuser = true;
        facts.reached = vec!["pg_read_all_data".to_owned()];
        facts.reaching = vec![MAINTENANCE.to_owned(), "x_admin".to_owned()];
        facts.edges[0].inherit_option = true;
        assert_eq!(
            tenant_role_findings(&facts, MAINTENANCE),
            [
                RoleFinding::Attribute("SUPERUSER"),
                RoleFinding::Reaches("pg_read_all_data".to_owned()),
                RoleFinding::ReachedBy("x_admin".to_owned()),
                RoleFinding::MaintenanceInherits,
            ]
        );
    }

    #[test]
    fn a_clean_tenant_database_has_no_finding() {
        assert_eq!(
            tenant_database_findings(&clean_database(), TENANT),
            [] as [DatabaseFinding; 0]
        );
    }

    #[test]
    fn another_owner_is_a_finding_and_its_entries_too() {
        let mut facts = clean_database();
        facts.owner = "billing_owner".to_owned();
        facts.grants = vec![grant(
            role("billing_owner"),
            DatabasePrivilege::Connect,
            false,
        )];
        assert_eq!(
            tenant_database_findings(&facts, TENANT),
            [
                DatabaseFinding::Owner("billing_owner".to_owned()),
                DatabaseFinding::Grant(grant(
                    role("billing_owner"),
                    DatabasePrivilege::Connect,
                    false
                )),
            ]
        );
    }

    #[test]
    fn a_template_is_a_finding() {
        let mut facts = clean_database();
        facts.template = true;
        assert_eq!(
            tenant_database_findings(&facts, TENANT),
            [DatabaseFinding::Template]
        );
    }

    #[test]
    fn a_grant_to_a_third_role_is_a_finding() {
        let mut facts = clean_database();
        let extra = grant(role("orders_reader"), DatabasePrivilege::Connect, false);
        facts.grants.push(extra.clone());
        assert_eq!(
            tenant_database_findings(&facts, TENANT),
            [DatabaseFinding::Grant(extra)]
        );
    }

    #[test]
    fn a_grant_to_the_maintenance_role_is_a_finding() {
        let mut facts = clean_database();
        let extra = grant(role(MAINTENANCE), DatabasePrivilege::Connect, false);
        facts.grants.push(extra.clone());
        assert_eq!(
            tenant_database_findings(&facts, TENANT),
            [DatabaseFinding::Grant(extra)]
        );
    }

    #[test]
    fn create_for_public_is_a_finding() {
        let mut facts = clean_database();
        let extra = grant(Grantee::Public, DatabasePrivilege::Create, false);
        facts.grants.push(extra.clone());
        assert_eq!(
            tenant_database_findings(&facts, TENANT),
            [DatabaseFinding::Grant(extra)]
        );
    }

    #[test]
    fn a_public_default_with_grant_option_is_a_finding() {
        let mut facts = clean_database();
        let extra = grant(Grantee::Public, DatabasePrivilege::Connect, true);
        facts.grants.push(extra.clone());
        assert_eq!(
            tenant_database_findings(&facts, TENANT),
            [DatabaseFinding::Grant(extra)]
        );
    }

    #[test]
    fn privilege_names_parse_and_an_unknown_one_is_internal() {
        assert_eq!(
            parse_privilege("CONNECT").unwrap(),
            DatabasePrivilege::Connect
        );
        assert_eq!(
            parse_privilege("TEMPORARY").unwrap(),
            DatabasePrivilege::Temporary
        );
        assert_eq!(
            parse_privilege("CREATE").unwrap(),
            DatabasePrivilege::Create
        );
        let error = parse_privilege("USAGE").unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert!(source_of(&error).contains("USAGE"));
        for privilege in [
            DatabasePrivilege::Connect,
            DatabasePrivilege::Temporary,
            DatabasePrivilege::Create,
        ] {
            assert_eq!(parse_privilege(privilege.sql_name()).unwrap(), privilege);
        }
    }

    fn role_row(kind: i32, first: &str, second: Option<&str>, flags: [bool; 7]) -> RoleRow {
        (
            kind,
            first.to_owned(),
            second.map(str::to_owned),
            flags[0],
            flags[1],
            flags[2],
            flags[3],
            flags[4],
            flags[5],
            flags[6],
        )
    }

    #[test]
    fn role_rows_assemble_sorted_facts_and_the_session_role() {
        let none = [false; 7];
        let rows = vec![
            role_row(3, "z_group", None, none),
            role_row(
                1,
                TENANT,
                Some(MAINTENANCE),
                [true, false, true, false, false, false, false],
            ),
            role_row(2, MAINTENANCE, None, none),
            role_row(
                1,
                "y_group",
                Some(TENANT),
                [false, true, false, false, false, false, false],
            ),
            role_row(
                0,
                TENANT,
                Some(MAINTENANCE),
                [true, false, true, false, true, false, true],
            ),
            role_row(3, "y_group", None, none),
            role_row(2, "a_operator", None, none),
        ];
        let (facts, session) = role_facts_from_rows(rows).unwrap();
        assert_eq!(session, MAINTENANCE);
        assert_eq!(facts.name, TENANT);
        assert_eq!(
            facts.attributes,
            RoleAttributes {
                superuser: true,
                inherit: false,
                create_role: true,
                create_db: false,
                login: true,
                replication: false,
                bypass_rls: true,
            }
        );
        assert_eq!(
            facts.edges,
            [
                MembershipEdge {
                    role: TENANT.to_owned(),
                    member: MAINTENANCE.to_owned(),
                    admin_option: true,
                    inherit_option: false,
                    set_option: true,
                },
                MembershipEdge {
                    role: "y_group".to_owned(),
                    member: TENANT.to_owned(),
                    admin_option: false,
                    inherit_option: true,
                    set_option: false,
                },
            ]
        );
        assert_eq!(facts.reaching, ["a_operator", MAINTENANCE]);
        assert_eq!(facts.reached, ["y_group", "z_group"]);
    }

    #[test]
    fn role_rows_without_the_role_itself_are_no_role() {
        assert_eq!(role_facts_from_rows(Vec::new()), None);
        let orphan = role_row(2, MAINTENANCE, None, [false; 7]);
        assert_eq!(role_facts_from_rows(vec![orphan]), None);
    }

    fn database_row(
        grantee: Option<&str>,
        privilege: Option<&str>,
        grantable: Option<bool>,
    ) -> DatabaseRow {
        (
            "orders_control".to_owned(),
            "orders_owner".to_owned(),
            true,
            grantee.map(str::to_owned),
            privilege.map(str::to_owned),
            grantable,
        )
    }

    #[test]
    fn database_rows_assemble_the_acl_in_order() {
        let rows = vec![
            database_row(None, Some("TEMPORARY"), Some(false)),
            database_row(None, Some("CONNECT"), Some(false)),
            database_row(Some("orders_owner"), Some("CREATE"), Some(true)),
        ];
        assert_eq!(
            database_facts_from_rows(rows).unwrap(),
            Some(DatabaseFacts {
                name: "orders_control".to_owned(),
                owner: "orders_owner".to_owned(),
                template: true,
                grants: vec![
                    grant(Grantee::Public, DatabasePrivilege::Temporary, false),
                    grant(Grantee::Public, DatabasePrivilege::Connect, false),
                    grant(role("orders_owner"), DatabasePrivilege::Create, true),
                ],
            })
        );
        let empty_acl = database_facts_from_rows(vec![database_row(None, None, None)])
            .unwrap()
            .unwrap();
        assert_eq!(empty_acl.grants, [] as [DatabaseGrant; 0]);
        assert_eq!(database_facts_from_rows(Vec::new()).unwrap(), None);
    }

    #[test]
    fn an_unknown_privilege_in_the_acl_fails_the_read() {
        let rows: Vec<DatabaseRow> = vec![(
            "orders_control".to_owned(),
            "orders_owner".to_owned(),
            false,
            None,
            Some("TRUNCATE".to_owned()),
            Some(false),
        )];
        let error = database_facts_from_rows(rows).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
    }

    #[test]
    fn connecting_rows_are_sorted_and_a_lone_null_is_nobody() {
        assert_eq!(connecting_from_rows(Vec::new()), None);
        assert_eq!(connecting_from_rows(vec![None]), Some(Vec::new()));
        assert_eq!(
            connecting_from_rows(vec![
                Some("orders_owner".to_owned()),
                Some("billing_owner".to_owned()),
                Some("orders_owner".to_owned()),
            ]),
            Some(vec!["billing_owner".to_owned(), "orders_owner".to_owned()])
        );
    }

    #[test]
    fn the_role_verdict_passes_a_clean_role_and_names_every_finding() {
        let clean = (clean_role(), MAINTENANCE.to_owned());
        assert!(role_verdict(TENANT, Some(&clean)).is_ok());

        let mut facts = clean_role();
        facts.attributes.bypass_rls = true;
        facts.reached = vec!["orders_reader".to_owned()];
        facts.reaching.push("orders_operator".to_owned());
        facts.edges[0].inherit_option = true;
        let error = role_verdict(TENANT, Some(&(facts, MAINTENANCE.to_owned()))).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(error.message(), "internal error");
        let source = source_of(&error);
        assert!(source.contains(TENANT), "{source}");
        assert!(source.contains("BYPASSRLS"), "{source}");
        assert!(source.contains("reaches role orders_reader"), "{source}");
        assert!(
            source.contains("role orders_operator reaches it"),
            "{source}"
        );
        assert!(source.contains("INHERIT"), "{source}");
    }

    #[test]
    fn the_role_verdict_refuses_a_missing_role() {
        let error = role_verdict(TENANT, None).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert!(source_of(&error).contains("does not exist"));
    }

    #[test]
    fn the_database_verdict_passes_a_clean_database_and_names_every_finding() {
        assert!(database_verdict(TENANT, TENANT, Some(&clean_database())).is_ok());

        let mut facts = clean_database();
        facts.owner = "orders_owner".to_owned();
        facts.template = true;
        facts
            .grants
            .push(grant(Grantee::Public, DatabasePrivilege::Create, true));
        let error = database_verdict(TENANT, TENANT, Some(&facts)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        let source = source_of(&error);
        assert!(source.contains("owned by orders_owner"), "{source}");
        assert!(source.contains("template"), "{source}");
        assert!(
            source.contains("PUBLIC holds CREATE with grant option"),
            "{source}"
        );
    }

    #[test]
    fn the_database_verdict_refuses_a_missing_database() {
        let error = database_verdict(TENANT, TENANT, None).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert!(source_of(&error).contains("does not exist"));
    }

    #[test]
    fn grants_are_described_by_holder_privilege_and_option() {
        assert_eq!(
            describe_grant(&grant(
                role("orders_reader"),
                DatabasePrivilege::Temporary,
                false
            )),
            "role orders_reader holds TEMPORARY"
        );
        assert_eq!(
            describe_grant(&grant(Grantee::Public, DatabasePrivilege::Connect, true)),
            "PUBLIC holds CONNECT with grant option"
        );
    }

    #[test]
    fn findings_display_their_description() {
        assert_eq!(
            RoleFinding::Attribute("SUPERUSER").to_string(),
            "it holds SUPERUSER"
        );
        assert_eq!(
            RoleFinding::Reaches("orders_reader".to_owned()).to_string(),
            "it reaches role orders_reader"
        );
        assert_eq!(
            RoleFinding::ReachedBy("orders_reader".to_owned()).to_string(),
            "role orders_reader reaches it"
        );
        assert_eq!(
            RoleFinding::MaintenanceInherits.to_string(),
            "the maintenance role's membership has INHERIT"
        );
        assert_eq!(
            DatabaseFinding::Owner("orders_owner".to_owned()).to_string(),
            "it is owned by orders_owner"
        );
        assert_eq!(
            describe_database_finding(&DatabaseFinding::Owner("orders_owner".to_owned()), TENANT),
            format!("it is owned by orders_owner, not {TENANT}")
        );
        assert_eq!(DatabaseFinding::Template.to_string(), "it is a template");
        assert_eq!(
            DatabaseFinding::Grant(grant(Grantee::Public, DatabasePrivilege::Create, false))
                .to_string(),
            "PUBLIC holds CREATE"
        );
        assert_eq!(DatabasePrivilege::Temporary.sql_name(), "TEMPORARY");
    }
}
