//! The control rows of tenants and their hosts: static SQL with bound values on a connection the
//! caller holds; nothing here commits.

use std::time::SystemTime;

use chrono::{DateTime, Utc};
use sekvent_db::{constraint_name, to_app_error};
use sekvent_error::AppError;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::host::{canonical_host, is_loopback_host};
use crate::{DatabaseName, RoleKey, TenantId, TenantInfo, TenantRole, TenantState};

/// The one value `tenants.db_cluster` holds.
pub const MAIN_CLUSTER: &str = "main";

/// How far provisioning of a tenant got, as stored in `tenants.provision_step`. The variants are in
/// the order the steps are recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ProvisionStep {
    /// The row exists and no step is recorded: the stored text is empty.
    Inserted,
    /// The tenant's role exists.
    Role,
    /// The tenant's database exists.
    Database,
    /// The tenant's schema is migrated.
    Migrated,
    /// The product's seed ran.
    Seeded,
    /// Provisioning is complete.
    Active,
}

impl ProvisionStep {
    /// The stored text: the empty text, `role`, `database`, `migrated`, `seeded`, `active`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "",
            Self::Role => "role",
            Self::Database => "database",
            Self::Migrated => "migrated",
            Self::Seeded => "seeded",
            Self::Active => "active",
        }
    }

    /// The step of exactly one of the six stored texts; `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "" => Some(Self::Inserted),
            "role" => Some(Self::Role),
            "database" => Some(Self::Database),
            "migrated" => Some(Self::Migrated),
            "seeded" => Some(Self::Seeded),
            "active" => Some(Self::Active),
            _ => None,
        }
    }
}

/// The whole control row of a tenant with its hosts.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TenantRow {
    /// What serving needs of the tenant.
    pub info: TenantInfo,
    /// The provisioning step recorded for the tenant.
    pub provision_step: ProvisionStep,
    /// The cluster label as stored; serving opens a tenant only when it is `MAIN_CLUSTER`.
    pub db_cluster: String,
    /// The database name as stored; check it with `DatabaseName::parse` before use.
    pub db_name: String,
    /// The role name as stored; it must equal `ObjectNames::role(info.id)` before use.
    pub db_role: String,
    /// The last tenant schema version stored for the tenant; `None` before the first.
    pub schema_version: Option<i64>,
    /// When the row was inserted.
    pub created_at: SystemTime,
    /// When the row's state, subdomain or hosts last changed.
    pub updated_at: SystemTime,
}

/// A tenant to insert. It starts in `Provisioning` with an empty step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTenant {
    /// The tenant's id.
    pub id: TenantId,
    /// The tenant's label.
    pub subdomain: String,
    /// The name people see.
    pub display_name: String,
    /// The tenant's canonical hosts.
    pub hosts: Vec<String>,
    /// One of `hosts`.
    pub primary_host: String,
    /// The tenant's database.
    pub database: DatabaseName,
    /// The tenant's role.
    pub role: TenantRole,
    /// The key the role password is derived under.
    pub role_key: RoleKey,
}

/// What `store_schema_version` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VersionStored {
    /// The stored version was absent or lower and is now `version`.
    Stored,
    /// The stored version is already `version` or higher; nothing was written.
    Unchanged,
    /// The row is locked by another transaction, or does not exist; nothing was written.
    Busy,
}

/// The columns of `tenants` every read selects, from the alias `t`.
macro_rules! tenant_columns {
    () => {
        "t.id, t.subdomain, t.display_name, t.state, t.provision_step, t.db_cluster, t.db_name, \
         t.db_role, t.db_role_key_id, t.db_role_generation, t.schema_version, t.created_at, \
         t.updated_at"
    };
}

/// The tenant's hosts and its primary host, read in the same statement as its row.
macro_rules! host_columns {
    () => {
        "ARRAY(SELECT h.host FROM tenant_hosts h WHERE h.tenant_id = t.id) AS hosts, \
         (SELECT h.host FROM tenant_hosts h WHERE h.tenant_id = t.id AND h.is_primary) \
         AS primary_host"
    };
}

const INSERT_TENANT: &str = concat!(
    "INSERT INTO tenants AS t (id, subdomain, display_name, state, db_name, db_role, \
     db_role_key_id, db_role_generation, created_at, updated_at) \
     VALUES ($1, $2, $3, 'provisioning', $4, $5, $6, $7, $8, $8) \
     ON CONFLICT (id) DO NOTHING RETURNING ",
    tenant_columns!()
);

const INSERT_HOSTS: &str = "INSERT INTO tenant_hosts (host, tenant_id, is_primary) \
     SELECT n.host, $1, n.host = $3 FROM unnest($2::text[]) AS n(host)";

const GET: &str = concat!(
    "SELECT ",
    tenant_columns!(),
    ", ",
    host_columns!(),
    " FROM tenants t WHERE t.id = $1"
);

const LOCK: &str = concat!(
    "SELECT ",
    tenant_columns!(),
    " FROM tenants t WHERE t.id = $1 FOR UPDATE"
);

const HOSTS_OF: &str = "SELECT host, is_primary FROM tenant_hosts WHERE tenant_id = $1";

const BY_HOST: &str = concat!(
    "SELECT ",
    tenant_columns!(),
    ", ",
    host_columns!(),
    " FROM tenants t WHERE t.id = (SELECT tenant_id FROM tenant_hosts WHERE host = $1)"
);

const BY_LABEL: &str = concat!(
    "SELECT ",
    tenant_columns!(),
    ", ",
    host_columns!(),
    " FROM tenants t WHERE t.subdomain = $1 AND t.state <> 'purged' ORDER BY t.id LIMIT 2"
);

const LIST_ACTIVE: &str = concat!(
    "SELECT ",
    tenant_columns!(),
    ", ",
    host_columns!(),
    " FROM tenants t WHERE t.state = 'active' ORDER BY t.id"
);

const SET_STATE: &str = "UPDATE tenants SET state = $2, updated_at = $3 WHERE id = $1";

const SET_SUBDOMAIN: &str = "UPDATE tenants SET subdomain = $2, updated_at = $3 WHERE id = $1";

const DELETE_HOSTS: &str = "DELETE FROM tenant_hosts WHERE tenant_id = $1";

const SET_STEP: &str = "UPDATE tenants SET provision_step = $2, updated_at = $3 WHERE id = $1";

const SET_ROLE_KEY: &str = "UPDATE tenants SET db_role_key_id = $2, db_role_generation = $3, \
     updated_at = $4 WHERE id = $1";

const TOUCH: &str = "UPDATE tenants SET updated_at = $2 WHERE id = $1";

const COUNT_LIVE: &str = "SELECT count(*) FROM tenants WHERE state <> 'purged'";

const PAGE: &str = concat!(
    "SELECT ",
    tenant_columns!(),
    ", ",
    host_columns!(),
    " FROM tenants t WHERE ($1::uuid IS NULL OR t.id > $1) ORDER BY t.id LIMIT $2"
);

const STORE_VERSION: &str = "WITH locked AS ( \
         SELECT id, schema_version FROM tenants WHERE id = $1 FOR UPDATE SKIP LOCKED \
     ), moved AS ( \
         UPDATE tenants SET schema_version = $2 FROM locked \
         WHERE tenants.id = locked.id \
           AND (locked.schema_version IS NULL OR locked.schema_version < $2) \
         RETURNING tenants.id \
     ) \
     SELECT (SELECT count(*) FROM locked), (SELECT count(*) FROM moved)";

/// The unique constraints and indexes whose violation means another tenant holds the name.
const HELD_ELSEWHERE: [&str; 3] = [
    "tenant_hosts_pkey",
    "tenants_db_role_key",
    "tenants_db_name_key",
];

/// The columns of `tenants` as stored.
#[derive(sqlx::FromRow)]
struct StoredTenant {
    id: Uuid,
    subdomain: String,
    display_name: String,
    state: String,
    provision_step: String,
    db_cluster: String,
    db_name: String,
    db_role: String,
    db_role_key_id: String,
    db_role_generation: i32,
    schema_version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// A row of `tenants` with its hosts, read by one statement.
#[derive(sqlx::FromRow)]
struct StoredRow {
    #[sqlx(flatten)]
    tenant: StoredTenant,
    hosts: Vec<String>,
    primary_host: Option<String>,
}

impl StoredRow {
    fn decode(self) -> Result<TenantRow, AppError> {
        decode(self.tenant, self.hosts, self.primary_host)
    }
}

/// `INTERNAL` for a stored row this crate cannot use; `cause` names the column, never a value.
fn corrupt(cause: &'static str) -> AppError {
    AppError::internal(cause)
}

/// The row as serving sees it, or `INTERNAL` for stored values outside what this crate writes.
/// The cluster label is copied unchecked; the hosts are trusted to be canonical and sorted here.
fn decode(
    stored: StoredTenant,
    mut hosts: Vec<String>,
    primary: Option<String>,
) -> Result<TenantRow, AppError> {
    let state = TenantState::parse(&stored.state)
        .ok_or_else(|| corrupt("tenants.state holds a value outside the five states"))?;
    let provision_step = ProvisionStep::parse(&stored.provision_step)
        .ok_or_else(|| corrupt("tenants.provision_step holds a value outside the six steps"))?;
    let generation = u32::try_from(stored.db_role_generation)
        .ok()
        .filter(|generation| *generation >= 1)
        .ok_or_else(|| corrupt("tenants.db_role_generation is not positive"))?;
    hosts.sort_unstable();
    let primary_host = match primary {
        Some(primary) => primary,
        None if hosts.is_empty() && matches!(state, TenantState::Purging | TenantState::Purged) => {
            String::new()
        }
        None if hosts.is_empty() => {
            return Err(corrupt(
                "tenant_hosts holds no host of a tenant that is served",
            ));
        }
        None => {
            return Err(corrupt(
                "tenant_hosts.is_primary marks no host of the tenant",
            ));
        }
    };
    Ok(TenantRow {
        info: TenantInfo {
            id: TenantId::from_uuid(stored.id),
            subdomain: stored.subdomain,
            display_name: stored.display_name,
            state,
            primary_host,
            hosts,
            role_key: RoleKey {
                key_id: stored.db_role_key_id,
                generation,
            },
        },
        provision_step,
        db_cluster: stored.db_cluster,
        db_name: stored.db_name,
        db_role: stored.db_role,
        schema_version: stored.schema_version,
        created_at: stored.created_at.into(),
        updated_at: stored.updated_at.into(),
    })
}

/// The host set to store: every host canonical, at least one, the primary among them; sorted and
/// without duplicates. `INVALID_ARGUMENT` names the position, never the text.
fn checked_hosts(hosts: &[String], primary_host: &str) -> Result<Vec<String>, AppError> {
    if hosts.is_empty() {
        return Err(
            AppError::invalid_argument("a tenant needs at least one host")
                .with_field_violation("hosts", "at least one host is required"),
        );
    }
    for (index, host) in hosts.iter().enumerate() {
        if canonical_host(host).ok().as_deref() != Some(host.as_str()) {
            return Err(AppError::invalid_argument(format!(
                "host {index} is not a canonical host"
            ))
            .with_field_violation(format!("hosts[{index}]"), "not a canonical host"));
        }
    }
    let mut sorted = hosts.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted
        .binary_search_by(|host| host.as_str().cmp(primary_host))
        .is_err()
    {
        return Err(
            AppError::invalid_argument("the primary host is not one of the hosts")
                .with_field_violation("primary_host", "not one of the hosts"),
        );
    }
    Ok(sorted)
}

/// The validated host set and the stored generation of `tenant`.
fn checked_tenant(tenant: &NewTenant) -> Result<(Vec<String>, i32), AppError> {
    let hosts = checked_hosts(&tenant.hosts, &tenant.primary_host)?;
    let generation = stored_generation(tenant.role_key.generation)?;
    Ok((hosts, generation))
}

/// `generation` as stored: 1 to 2147483647, else `INVALID_ARGUMENT`.
fn stored_generation(generation: u32) -> Result<i32, AppError> {
    i32::try_from(generation)
        .ok()
        .filter(|generation| *generation >= 1)
        .ok_or_else(|| {
            AppError::invalid_argument("the role key's generation is out of range")
                .with_field_violation("role_key.generation", "from 1 to 2147483647")
        })
}

/// A failed write: `TENANT_STATE` when another tenant holds a host, the role or the database
/// name; every other failure through `to_app_error`.
fn write_error(error: sqlx::Error) -> AppError {
    if constraint_name(&error).is_some_and(|name| HELD_ELSEWHERE.contains(&name)) {
        crate::error::tenant_state().with_source(error)
    } else {
        to_app_error(error)
    }
}

/// Inserts `hosts` for `id`, marking `primary_host`.
async fn insert_hosts(
    conn: &mut PgConnection,
    id: TenantId,
    hosts: &[String],
    primary_host: &str,
) -> Result<(), AppError> {
    sqlx::query(INSERT_HOSTS)
        .bind(Uuid::from(id))
        .bind(hosts)
        .bind(primary_host)
        .execute(conn)
        .await
        .map_err(write_error)?;
    Ok(())
}

/// Inserts `tenant` and its hosts; `(row, true)`. A repeated id changes nothing and returns
/// `(the existing row, false)`. Call it inside a transaction (two statements) and roll back on an
/// error. `INVALID_ARGUMENT` before any statement for a host that is not canonical, no host, or a
/// primary host outside the set (hosts are sorted and de-duplicated here). `FAILED_PRECONDITION` /
/// `TENANT_STATE` when a host, the role or the database name is held by another tenant
/// (`tenant_hosts_pkey`, `tenants_db_role_key`, `tenants_db_name_key`).
pub async fn insert(
    conn: &mut PgConnection,
    tenant: &NewTenant,
    now: SystemTime,
) -> Result<(TenantRow, bool), AppError> {
    let (hosts, generation) = checked_tenant(tenant)?;
    let inserted: Option<StoredTenant> = sqlx::query_as(INSERT_TENANT)
        .bind(Uuid::from(tenant.id))
        .bind(tenant.subdomain.as_str())
        .bind(tenant.display_name.as_str())
        .bind(tenant.database.as_str())
        .bind(tenant.role.as_str())
        .bind(tenant.role_key.key_id.as_str())
        .bind(generation)
        .bind(DateTime::<Utc>::from(now))
        .fetch_optional(&mut *conn)
        .await
        .map_err(write_error)?;
    let Some(stored) = inserted else {
        let existing = get(conn, tenant.id)
            .await?
            .ok_or_else(|| corrupt("tenants lost the row whose id conflicted"))?;
        return Ok((existing, false));
    };
    insert_hosts(conn, tenant.id, &hosts, &tenant.primary_host).await?;
    let row = decode(stored, hosts, Some(tenant.primary_host.clone()))?;
    Ok((row, true))
}

/// The row of `id` in any state, with one statement. A stored state outside the five values, a
/// stored provisioning step outside the six texts, a generation that is not positive, or a missing
/// primary host (unless the row has no host and is `Purging` or `Purged`) is `INTERNAL`.
pub async fn get(conn: &mut PgConnection, id: TenantId) -> Result<Option<TenantRow>, AppError> {
    let stored: Option<StoredRow> = sqlx::query_as(GET)
        .bind(Uuid::from(id))
        .fetch_optional(conn)
        .await
        .map_err(to_app_error)?;
    stored.map(StoredRow::decode).transpose()
}

/// `get` under `FOR UPDATE`, inside the caller's transaction; the hosts are read by a second
/// statement after the lock is held, so a waiter sees the hosts the lock holder committed.
pub async fn lock(conn: &mut PgConnection, id: TenantId) -> Result<Option<TenantRow>, AppError> {
    let stored: Option<StoredTenant> = sqlx::query_as(LOCK)
        .bind(Uuid::from(id))
        .fetch_optional(&mut *conn)
        .await
        .map_err(to_app_error)?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let held: Vec<(String, bool)> = sqlx::query_as(HOSTS_OF)
        .bind(Uuid::from(id))
        .fetch_all(conn)
        .await
        .map_err(to_app_error)?;
    let primary = held
        .iter()
        .find(|(_, is_primary)| *is_primary)
        .map(|(host, _)| host.clone());
    let hosts = held.into_iter().map(|(host, _)| host).collect();
    decode(stored, hosts, primary).map(Some)
}

/// The row of the tenant that holds exactly `host`, with one statement.
pub async fn by_host(conn: &mut PgConnection, host: &str) -> Result<Option<TenantRow>, AppError> {
    let stored: Option<StoredRow> = sqlx::query_as(BY_HOST)
        .bind(host)
        .fetch_optional(conn)
        .await
        .map_err(to_app_error)?;
    stored.map(StoredRow::decode).transpose()
}

/// The loopback path-label rule: with one statement reads at most two rows that are not `Purged`
/// and whose `subdomain` is `label`; `Some` only when there is exactly one and it has at least one
/// host for which `is_loopback_host` holds.
pub async fn by_loopback_label(
    conn: &mut PgConnection,
    label: &str,
) -> Result<Option<TenantRow>, AppError> {
    let mut stored: Vec<StoredRow> = sqlx::query_as(BY_LABEL)
        .bind(label)
        .fetch_all(conn)
        .await
        .map_err(to_app_error)?;
    if stored.len() != 1 {
        return Ok(None);
    }
    let row = stored.swap_remove(0).decode()?;
    Ok(row
        .info
        .hosts
        .iter()
        .any(|host| is_loopback_host(host))
        .then_some(row))
}

/// Every `Active` tenant with its whole host set, ordered by id, with one statement.
pub async fn list_active(conn: &mut PgConnection) -> Result<Vec<TenantInfo>, AppError> {
    let stored: Vec<StoredRow> = sqlx::query_as(LIST_ACTIVE)
        .fetch_all(conn)
        .await
        .map_err(to_app_error)?;
    stored
        .into_iter()
        .map(|stored| stored.decode().map(|row| row.info))
        .collect()
}

/// Sets the state and `updated_at`; whether a row was changed.
pub async fn set_state(
    conn: &mut PgConnection,
    id: TenantId,
    state: TenantState,
    now: SystemTime,
) -> Result<bool, AppError> {
    let done = sqlx::query(SET_STATE)
        .bind(Uuid::from(id))
        .bind(state.as_str())
        .bind(DateTime::<Utc>::from(now))
        .execute(conn)
        .await
        .map_err(to_app_error)?;
    Ok(done.rows_affected() == 1)
}

/// Replaces the tenant's whole host set and its subdomain, and sets `updated_at`. Call it inside a
/// transaction (three statements). Validation and the `TENANT_STATE` mapping as `insert`; a
/// missing row is `NOT_FOUND` without a reason.
pub async fn set_hosts(
    conn: &mut PgConnection,
    id: TenantId,
    subdomain: &str,
    hosts: &[String],
    primary_host: &str,
    now: SystemTime,
) -> Result<(), AppError> {
    let hosts = checked_hosts(hosts, primary_host)?;
    let done = sqlx::query(SET_SUBDOMAIN)
        .bind(Uuid::from(id))
        .bind(subdomain)
        .bind(DateTime::<Utc>::from(now))
        .execute(&mut *conn)
        .await
        .map_err(to_app_error)?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no such tenant"));
    }
    sqlx::query(DELETE_HOSTS)
        .bind(Uuid::from(id))
        .execute(&mut *conn)
        .await
        .map_err(to_app_error)?;
    insert_hosts(conn, id, &hosts, primary_host).await
}

/// Raises `schema_version` to `version` with one statement that takes the row
/// `FOR UPDATE SKIP LOCKED` and never waits. Never lowers the version and never touches
/// `updated_at`.
pub async fn store_schema_version(
    conn: &mut PgConnection,
    id: TenantId,
    version: i64,
) -> Result<VersionStored, AppError> {
    let (locked, moved): (i64, i64) = sqlx::query_as(STORE_VERSION)
        .bind(Uuid::from(id))
        .bind(version)
        .fetch_one(conn)
        .await
        .map_err(to_app_error)?;
    Ok(if locked == 0 {
        VersionStored::Busy
    } else if moved == 0 {
        VersionStored::Unchanged
    } else {
        VersionStored::Stored
    })
}

/// Records `step` as the tenant's provisioning step and sets `updated_at`; whether a row was
/// changed.
pub async fn set_step(
    conn: &mut PgConnection,
    id: TenantId,
    step: ProvisionStep,
    now: SystemTime,
) -> Result<bool, AppError> {
    let done = sqlx::query(SET_STEP)
        .bind(Uuid::from(id))
        .bind(step.as_str())
        .bind(DateTime::<Utc>::from(now))
        .execute(conn)
        .await
        .map_err(to_app_error)?;
    Ok(done.rows_affected() == 1)
}

/// Stores the key the tenant's role password is derived under and sets `updated_at`; whether a
/// row was changed. `INVALID_ARGUMENT` before any statement for a generation outside 1 to
/// 2147483647.
pub async fn set_role_key(
    conn: &mut PgConnection,
    id: TenantId,
    key: &RoleKey,
    now: SystemTime,
) -> Result<bool, AppError> {
    let generation = stored_generation(key.generation)?;
    let done = sqlx::query(SET_ROLE_KEY)
        .bind(Uuid::from(id))
        .bind(key.key_id.as_str())
        .bind(generation)
        .bind(DateTime::<Utc>::from(now))
        .execute(conn)
        .await
        .map_err(to_app_error)?;
    Ok(done.rows_affected() == 1)
}

/// Removes every host of the tenant and sets `updated_at`; whether the tenant's row exists. Call
/// it inside a transaction (two statements). The subdomain stays.
pub async fn clear_hosts(
    conn: &mut PgConnection,
    id: TenantId,
    now: SystemTime,
) -> Result<bool, AppError> {
    let done = sqlx::query(TOUCH)
        .bind(Uuid::from(id))
        .bind(DateTime::<Utc>::from(now))
        .execute(&mut *conn)
        .await
        .map_err(to_app_error)?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(DELETE_HOSTS)
        .bind(Uuid::from(id))
        .execute(conn)
        .await
        .map_err(to_app_error)?;
    Ok(true)
}

/// The number of tenants that are not `purged`, with one statement.
pub async fn count_live(conn: &mut PgConnection) -> Result<u64, AppError> {
    let count: i64 = sqlx::query_scalar(COUNT_LIVE)
        .fetch_one(conn)
        .await
        .map_err(to_app_error)?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// At most `limit` tenants in any state, ordered by id, starting after `after` (from the first
/// when `None`), with one statement. A `limit` of 0 reads nothing.
pub async fn page(
    conn: &mut PgConnection,
    after: Option<TenantId>,
    limit: u32,
) -> Result<Vec<TenantRow>, AppError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let stored: Vec<StoredRow> = sqlx::query_as(PAGE)
        .bind(after.map(Uuid::from))
        .bind(i64::from(limit))
        .fetch_all(conn)
        .await
        .map_err(to_app_error)?;
    stored.into_iter().map(StoredRow::decode).collect()
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::time::Duration;

    use sekvent_error::ErrorCode;

    use super::*;
    use crate::ObjectNames;

    const STEPS: [(ProvisionStep, &str); 6] = [
        (ProvisionStep::Inserted, ""),
        (ProvisionStep::Role, "role"),
        (ProvisionStep::Database, "database"),
        (ProvisionStep::Migrated, "migrated"),
        (ProvisionStep::Seeded, "seeded"),
        (ProvisionStep::Active, "active"),
    ];

    fn at(seconds: u64) -> DateTime<Utc> {
        DateTime::<Utc>::from(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
    }

    fn stored(state: &str) -> StoredTenant {
        StoredTenant {
            id: Uuid::new_v4(),
            subdomain: "orders".to_owned(),
            display_name: "Orders".to_owned(),
            state: state.to_owned(),
            provision_step: String::new(),
            db_cluster: MAIN_CLUSTER.to_owned(),
            db_name: "app_t_orders".to_owned(),
            db_role: "app_t_orders".to_owned(),
            db_role_key_id: "k1".to_owned(),
            db_role_generation: 1,
            schema_version: Some(2),
            created_at: at(1_700_000_000),
            updated_at: at(1_700_000_060),
        }
    }

    fn hosts(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    fn source_of(error: &AppError) -> String {
        error.source().map(ToString::to_string).unwrap_or_default()
    }

    fn assert_corrupt(result: Result<TenantRow, AppError>, column: &str) {
        let error = result.expect_err("the stored row is refused");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(error.message(), "internal error");
        assert!(source_of(&error).contains(column), "{}", source_of(&error));
    }

    fn assert_invalid(result: Result<Vec<String>, AppError>, field: &str) {
        let error = result.expect_err("the hosts are refused");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.reason(), None);
        assert_eq!(error.field_violations()[0].field, field);
        assert!(!error.message().contains("example"), "{}", error.message());
    }

    #[test]
    fn every_step_round_trips_through_its_stored_text() {
        for (step, text) in STEPS {
            assert_eq!(step.as_str(), text);
            assert_eq!(ProvisionStep::parse(text), Some(step));
        }
        assert_eq!(ProvisionStep::parse(""), Some(ProvisionStep::Inserted));
    }

    #[test]
    fn parse_refuses_every_other_step_text() {
        for text in ["Role", "ROLE", " role", "role ", " ", "created", "purged"] {
            assert_eq!(ProvisionStep::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_steps_are_ordered_as_they_are_recorded() {
        let order: Vec<ProvisionStep> = STEPS.iter().map(|(step, _)| *step).collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, order);
        assert!(ProvisionStep::Inserted < ProvisionStep::Role);
        assert!(ProvisionStep::Role < ProvisionStep::Database);
        assert!(ProvisionStep::Database < ProvisionStep::Migrated);
        assert!(ProvisionStep::Migrated < ProvisionStep::Seeded);
        assert!(ProvisionStep::Seeded < ProvisionStep::Active);
    }

    #[test]
    fn a_valid_row_decodes_field_by_field_with_sorted_hosts() {
        let row = stored("active");
        let id = row.id;
        let decoded = decode(
            row,
            hosts(&["orders.example.com", "api.orders.example.com"]),
            Some("orders.example.com".to_owned()),
        )
        .expect("the row decodes");
        assert_eq!(decoded.info.id, TenantId::from_uuid(id));
        assert_eq!(decoded.info.subdomain, "orders");
        assert_eq!(decoded.info.display_name, "Orders");
        assert_eq!(decoded.info.state, TenantState::Active);
        assert_eq!(decoded.info.primary_host, "orders.example.com");
        assert_eq!(
            decoded.info.hosts,
            ["api.orders.example.com", "orders.example.com"]
        );
        assert_eq!(
            decoded.info.role_key,
            RoleKey {
                key_id: "k1".to_owned(),
                generation: 1,
            }
        );
        assert_eq!(decoded.provision_step, ProvisionStep::Inserted);
        assert_eq!(decoded.db_cluster, MAIN_CLUSTER);
        assert_eq!(decoded.db_name, "app_t_orders");
        assert_eq!(decoded.db_role, "app_t_orders");
        assert_eq!(decoded.schema_version, Some(2));
        assert_eq!(
            decoded.created_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
        assert_eq!(
            decoded.updated_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_060)
        );
    }

    #[test]
    fn a_stored_step_and_another_cluster_are_kept() {
        let row = StoredTenant {
            provision_step: "seeded".to_owned(),
            db_cluster: "replica".to_owned(),
            ..stored("provisioning")
        };
        let decoded = decode(
            row,
            hosts(&["orders.example.com"]),
            Some("orders.example.com".to_owned()),
        )
        .expect("the row decodes");
        assert_eq!(decoded.provision_step, ProvisionStep::Seeded);
        assert_eq!(decoded.db_cluster, "replica");
        assert_eq!(decoded.info.state, TenantState::Provisioning);
    }

    #[test]
    fn an_unknown_state_is_internal() {
        let primary = Some("orders.example.com".to_owned());
        for state in ["Active", "deleted", ""] {
            assert_corrupt(
                decode(
                    stored(state),
                    hosts(&["orders.example.com"]),
                    primary.clone(),
                ),
                "tenants.state",
            );
        }
    }

    #[test]
    fn an_unknown_step_is_internal() {
        let row = StoredTenant {
            provision_step: "Migrated".to_owned(),
            ..stored("active")
        };
        assert_corrupt(
            decode(
                row,
                hosts(&["orders.example.com"]),
                Some("orders.example.com".to_owned()),
            ),
            "tenants.provision_step",
        );
    }

    #[test]
    fn a_generation_that_is_not_positive_is_internal() {
        for generation in [0, -1, i32::MIN] {
            let row = StoredTenant {
                db_role_generation: generation,
                ..stored("active")
            };
            assert_corrupt(
                decode(
                    row,
                    hosts(&["orders.example.com"]),
                    Some("orders.example.com".to_owned()),
                ),
                "tenants.db_role_generation",
            );
        }
        let largest = StoredTenant {
            db_role_generation: i32::MAX,
            ..stored("active")
        };
        let decoded = decode(
            largest,
            hosts(&["orders.example.com"]),
            Some("orders.example.com".to_owned()),
        )
        .expect("the largest generation decodes");
        assert_eq!(decoded.info.role_key.generation, 2_147_483_647);
    }

    #[test]
    fn hosts_without_a_primary_are_internal() {
        assert_corrupt(
            decode(stored("active"), hosts(&["orders.example.com"]), None),
            "tenant_hosts.is_primary",
        );
        assert_corrupt(
            decode(stored("purged"), hosts(&["orders.example.com"]), None),
            "tenant_hosts.is_primary",
        );
    }

    #[test]
    fn a_served_row_without_hosts_is_internal() {
        for state in ["provisioning", "active", "suspended"] {
            assert_corrupt(decode(stored(state), Vec::new(), None), "tenant_hosts");
        }
    }

    #[test]
    fn a_purging_or_purged_row_may_have_no_hosts() {
        for (text, state) in [
            ("purging", TenantState::Purging),
            ("purged", TenantState::Purged),
        ] {
            let decoded = decode(stored(text), Vec::new(), None).expect("the row decodes");
            assert_eq!(decoded.info.state, state);
            assert_eq!(decoded.info.primary_host, "");
            assert_eq!(decoded.info.hosts, [] as [String; 0]);
            assert_eq!(decoded.info.origin(), None);
        }
    }

    #[test]
    fn hosts_are_validated_sorted_and_de_duplicated() {
        let checked = checked_hosts(
            &hosts(&[
                "orders.example.com",
                "api.orders.example.com",
                "orders.example.com",
                "orders.localhost:3000",
            ]),
            "orders.example.com",
        )
        .expect("the hosts are valid");
        assert_eq!(
            checked,
            [
                "api.orders.example.com",
                "orders.example.com",
                "orders.localhost:3000"
            ]
        );
    }

    #[test]
    fn an_empty_host_set_is_refused() {
        assert_invalid(checked_hosts(&[], ""), "hosts");
    }

    #[test]
    fn a_host_that_is_not_canonical_is_refused_by_position() {
        for (texts, field) in [
            (
                &["orders.example.com", "Orders.Example.com"][..],
                "hosts[1]",
            ),
            (&["orders.example.com."][..], "hosts[0]"),
            (
                &["orders.example.com", "orders.example.com:8443"][..],
                "hosts[1]",
            ),
            (&["orders.example.com", ""][..], "hosts[1]"),
            (&["not a host"][..], "hosts[0]"),
        ] {
            assert_invalid(checked_hosts(&hosts(texts), "orders.example.com"), field);
        }
    }

    #[test]
    fn a_primary_host_outside_the_set_is_refused() {
        assert_invalid(
            checked_hosts(&hosts(&["orders.example.com"]), "billing.example.com"),
            "primary_host",
        );
        assert_invalid(
            checked_hosts(&hosts(&["orders.example.com"]), ""),
            "primary_host",
        );
    }

    #[test]
    fn a_new_tenant_needs_a_generation_that_fits_the_column() {
        let names = ObjectNames::new("app_t_").expect("the prefix is valid");
        let id = TenantId::from_uuid(Uuid::new_v4());
        let tenant = |generation: u32| NewTenant {
            id,
            subdomain: "inventory".to_owned(),
            display_name: "Inventory".to_owned(),
            hosts: hosts(&["inventory.example.com"]),
            primary_host: "inventory.example.com".to_owned(),
            database: names.database(id),
            role: names.role(id),
            role_key: RoleKey {
                key_id: "k1".to_owned(),
                generation,
            },
        };
        let (checked, generation) = checked_tenant(&tenant(1)).expect("generation 1 fits");
        assert_eq!(checked, ["inventory.example.com"]);
        assert_eq!(generation, 1);
        assert_eq!(
            checked_tenant(&tenant(2_147_483_647))
                .expect("the largest generation fits")
                .1,
            i32::MAX
        );
        for generation in [0, 2_147_483_648, u32::MAX] {
            let error = checked_tenant(&tenant(generation)).expect_err("out of range");
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.field_violations()[0].field, "role_key.generation");
        }
    }

    #[test]
    fn only_set_step_writes_the_step_and_no_statement_writes_the_cluster() {
        assert!(SET_STEP.contains("provision_step = $2"));
        for statement in [SET_ROLE_KEY, TOUCH, PAGE, COUNT_LIVE] {
            assert!(!statement.contains("provision_step ="), "{statement}");
            assert!(!statement.contains("db_cluster ="), "{statement}");
        }
        assert!(PAGE.contains("t.provision_step") && PAGE.contains("ARRAY(SELECT h.host"));
        assert!(COUNT_LIVE.contains("state <> 'purged'"));
    }

    #[test]
    fn stored_generation_admits_one_to_the_largest_integer() {
        assert_eq!(stored_generation(1).expect("1 fits"), 1);
        assert_eq!(
            stored_generation(2_147_483_647).expect("the largest fits"),
            i32::MAX
        );
        for generation in [0, 2_147_483_648, u32::MAX] {
            let error = stored_generation(generation).expect_err("out of range");
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.field_violations()[0].field, "role_key.generation");
        }
    }

    #[test]
    fn the_statements_read_the_step_and_the_cluster() {
        for statement in [GET, LOCK, BY_HOST, BY_LABEL, LIST_ACTIVE, INSERT_TENANT] {
            assert!(statement.contains("t.provision_step"), "{statement}");
            assert!(statement.contains("t.db_cluster"), "{statement}");
        }
        for statement in [INSERT_TENANT, SET_STATE, SET_SUBDOMAIN, STORE_VERSION] {
            assert!(!statement.contains("provision_step ="), "{statement}");
            assert!(!statement.contains("db_cluster ="), "{statement}");
        }
    }
}
