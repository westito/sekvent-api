//! Leases with fencing tokens, one row per name in an application-owned table.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sekvent_error::{AppError, ErrorCode};
use sqlx::AssertSqlSafe;
use tokio::time::Instant;

use crate::reasons::{LEASE_LOST, LEASE_SCHEMA_MISSING};
use crate::{Pool, to_app_error};

#[cfg(feature = "runtime")]
mod guard;
mod held;
mod sql;

#[cfg(feature = "runtime")]
pub use guard::LeaseGuard;
pub use held::HeldLease;
use held::{BoxFut, Labels, Renew, Unclaimed};
use sql::{Dialect, Statements};

/// The table a [`LeaseStore`] uses unless told otherwise.
pub const DEFAULT_LEASE_TABLE: &str = "sekvent_leases";

/// The TTL `LeaseGuard` uses unless told otherwise; also a sensible
/// default for [`LeaseStore::try_acquire`].
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);

const MIN_TTL: Duration = Duration::from_secs(1);
const MAX_TTL: Duration = Duration::from_hours(24);
const MAX_NAME_BYTES: usize = 200;
const MAX_HOLDER_BYTES: usize = 200;
/// Postgres truncates identifiers past 63 bytes; MySQL allows 64.
const MAX_TABLE_BYTES: usize = 63;

/// Runs `$body` with `$pool` bound to the backend's sqlx pool. The body is
/// type-checked once per backend.
macro_rules! on_pool {
    ($pool:expr, |$p:ident| $body:expr) => {
        match $pool {
            #[cfg(feature = "sqlx-postgres")]
            Pool::Postgres($p) => $body,
            #[cfg(feature = "sqlx-mysql")]
            Pool::MySql($p) => $body,
        }
    };
}

/// Fencing token: grows with every acquisition of a lease name.
///
/// Store it with guarded writes (`WHERE last_fence <= $fence`) or check it
/// in the write's transaction ([`LeaseStore::check_fence_postgres`],
/// [`LeaseStore::check_fence_mysql`]), so a holder that lost its lease
/// without noticing cannot overwrite its successor's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FencingToken(u64);

impl FencingToken {
    /// A token with this value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for FencingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Who holds a lease now.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeaseInfo {
    /// The holder label of the store that acquired it.
    pub holder: String,
    /// The fence of the current acquisition.
    pub fence: FencingToken,
    /// Time left until it expires, by the database's clock.
    pub remaining: Duration,
}

/// Lease rows in one table of one pool. Cheap to clone.
///
/// Names are 1–200 bytes of `[A-Za-z0-9._:/-]`, TTLs 1 s to 24 h, ticks at
/// or after the Unix epoch; anything else is `INVALID_ARGUMENT`. Database
/// failures are classified with [`to_app_error`](crate::to_app_error).
#[derive(Clone)]
pub struct LeaseStore {
    pool: Pool,
    table: String,
    holder: String,
    sql: Arc<Statements>,
}

impl fmt::Debug for LeaseStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaseStore")
            .field("pool", &self.pool)
            .field("table", &self.table)
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

impl LeaseStore {
    /// Table `sekvent_leases`, holder label `pid-<process id>`.
    pub fn new(pool: &Pool) -> Self {
        Self {
            sql: Arc::new(Statements::new(dialect(pool), DEFAULT_LEASE_TABLE)),
            pool: pool.clone(),
            table: DEFAULT_LEASE_TABLE.to_owned(),
            holder: format!("pid-{}", std::process::id()),
        }
    }

    /// Use `table` instead. `INVALID_ARGUMENT` unless it is a plain
    /// identifier: 1–63 bytes of `[a-z0-9_]`, not starting with a digit.
    pub fn with_table(mut self, table: &str) -> Result<Self, AppError> {
        validate_table(table)?;
        self.sql = Arc::new(Statements::new(dialect(&self.pool), table));
        table.clone_into(&mut self.table);
        Ok(self)
    }

    /// A label shown in [`info`](Self::info) and logs (hostname, pod);
    /// visible ASCII, at most 200 bytes.
    pub fn with_holder(mut self, holder: &str) -> Result<Self, AppError> {
        validate_holder(holder)?;
        holder.clone_into(&mut self.holder);
        Ok(self)
    }

    /// The table name.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The holder label.
    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// The DDL for this backend and table, for the application's migrations.
    pub fn schema_sql(&self) -> String {
        self.sql.schema.clone()
    }

    /// Create the table unless it exists (needs the `CREATE` privilege).
    pub async fn ensure_schema(&self) -> Result<(), AppError> {
        let schema = AssertSqlSafe(self.sql.schema.as_str());
        on_pool!(&self.pool, |pool| sqlx::raw_sql(schema)
            .execute(pool)
            .await
            .map(drop))
        .map_err(to_app_error)
    }

    /// `FAILED_PRECONDITION` / `LEASE_SCHEMA_MISSING`, naming the table,
    /// when the table is absent or lacks a column; any other failure is
    /// classified with [`to_app_error`](crate::to_app_error).
    pub async fn verify_schema(&self) -> Result<(), AppError> {
        let verify = AssertSqlSafe(self.sql.verify.as_str());
        on_pool!(&self.pool, |pool| sqlx::query(verify)
            .execute(pool)
            .await
            .map(drop))
        .map_err(|error| schema_error(&self.table, error))
    }

    /// Take `name` unless someone holds it.
    pub async fn try_acquire(&self, name: &str, ttl: Duration) -> Result<Option<Lease>, AppError> {
        validate_name(name)?;
        validate_ttl(ttl)?;
        self.acquire(name, None, ttl).await
    }

    /// Take `name` for the scheduled `tick`, unless it is held or `tick` (or
    /// a later one) already ran. A successful acquisition records `tick`.
    pub async fn try_acquire_tick(
        &self,
        name: &str,
        tick: SystemTime,
        ttl: Duration,
    ) -> Result<Option<Lease>, AppError> {
        validate_name(name)?;
        validate_ttl(ttl)?;
        let tick_us = tick_micros(tick)?;
        self.acquire(name, Some(tick_us), ttl).await
    }

    /// The scheduled time of the last tick any holder acquired `name` for.
    pub async fn last_tick(&self, name: &str) -> Result<Option<SystemTime>, AppError> {
        validate_name(name)?;
        let sql = AssertSqlSafe(self.sql.last_tick.as_str());
        let row = on_pool!(&self.pool, |pool| sqlx::query_scalar::<_, Option<i64>>(sql)
            .bind(name)
            .fetch_optional(pool)
            .await)
        .map_err(to_app_error)?;
        Ok(row
            .flatten()
            .and_then(|us| u64::try_from(us).ok())
            .map(|us| UNIX_EPOCH + Duration::from_micros(us)))
    }

    /// Who holds `name` now; `None` when it is free.
    pub async fn info(&self, name: &str) -> Result<Option<LeaseInfo>, AppError> {
        validate_name(name)?;
        let sql = AssertSqlSafe(self.sql.info.as_str());
        let row = on_pool!(&self.pool, |pool| sqlx::query_as::<_, (String, i64, i64)>(
            sql
        )
        .bind(name)
        .fetch_optional(pool)
        .await)
        .map_err(to_app_error)?;
        Ok(row.map(|(holder, fence, remaining_us)| LeaseInfo {
            holder,
            fence: FencingToken(to_u64(fence)),
            remaining: Duration::from_micros(to_u64(remaining_us)),
        }))
    }

    /// In the caller's transaction: `ABORTED` / `LEASE_LOST` unless `name`
    /// is still held under `fence`. Takes a shared lock on the row, so no
    /// new holder can take over before the transaction ends; keep such
    /// transactions shorter than a third of the TTL, since the lock also
    /// delays the holder's own renewal.
    #[cfg(feature = "sqlx-postgres")]
    pub async fn check_fence_postgres(
        &self,
        conn: &mut sqlx::PgConnection,
        name: &str,
        fence: FencingToken,
    ) -> Result<(), AppError> {
        validate_name(name)?;
        let sql = sql::check_fence(Dialect::Postgres, &self.table);
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(name)
            .bind(to_i64(fence.get()))
            .fetch_optional(conn)
            .await
            .map_err(to_app_error)?;
        fence_held(row.is_some(), name, fence)
    }

    /// In the caller's transaction: `ABORTED` / `LEASE_LOST` unless `name`
    /// is still held under `fence`. Takes a shared lock on the row (`LOCK IN
    /// SHARE MODE`), so no new holder can take over before the transaction
    /// ends; keep such transactions shorter than a third of the TTL.
    #[cfg(feature = "sqlx-mysql")]
    pub async fn check_fence_mysql(
        &self,
        conn: &mut sqlx::MySqlConnection,
        name: &str,
        fence: FencingToken,
    ) -> Result<(), AppError> {
        validate_name(name)?;
        let sql = sql::check_fence(Dialect::MySql, &self.table);
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(name)
            .bind(to_i64(fence.get()))
            .fetch_optional(conn)
            .await
            .map_err(to_app_error)?;
        fence_held(row.is_some(), name, fence)
    }

    /// The owner is generated before anything is sent, so an acquisition
    /// whose COMMIT went out but whose outcome never arrived (an error, or
    /// the caller dropping the future) is released by owner in the
    /// background.
    async fn acquire(
        &self,
        name: &str,
        tick_us: Option<i64>,
        ttl: Duration,
    ) -> Result<Option<Lease>, AppError> {
        let row = Arc::new(LeaseRow {
            store: self.clone(),
            name: name.to_owned(),
            owner: uuid::Uuid::new_v4().simple().to_string(),
            ttl,
        });
        let mut unclaimed = Unclaimed::new(Arc::clone(&row) as Arc<dyn Renew>, name);
        let acquired = match &self.pool {
            #[cfg(feature = "sqlx-postgres")]
            Pool::Postgres(pool) => {
                self.acquire_postgres(pool, &row, tick_us, &mut unclaimed)
                    .await
            }
            #[cfg(feature = "sqlx-mysql")]
            Pool::MySql(pool) => {
                self.acquire_mysql(pool, &row, tick_us, &mut unclaimed)
                    .await
            }
        }
        .map_err(to_app_error)?;
        let Some((fence, sent)) = acquired else {
            return Ok(None);
        };
        unclaimed.claim();
        let fence = FencingToken(fence);
        tracing::debug!(lease = %name, fence = %fence, holder = %self.holder, "lease acquired");
        Ok(Some(Lease {
            row,
            fence,
            renewed_at: sent,
        }))
    }

    /// The acquiring `UPDATE … RETURNING fence` in a transaction of its
    /// own; when the row does not exist yet, create it expired (outside the
    /// transaction) and try once more. Of several racing creators only one
    /// inserts; the others see the row and lose to the `UPDATE` that wins
    /// its lock. Returns the fence and when the transaction began: `now()`
    /// is the transaction's start, so validity counts from before `BEGIN`.
    #[cfg(feature = "sqlx-postgres")]
    async fn acquire_postgres(
        &self,
        pool: &sqlx::PgPool,
        row: &LeaseRow,
        tick_us: Option<i64>,
        unclaimed: &mut Unclaimed,
    ) -> Result<Option<(u64, Instant)>, sqlx::Error> {
        let mut conn = pool.acquire().await?;
        let mut created = false;
        loop {
            let sent = Instant::now();
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            let fence: Option<i64> = sqlx::query_scalar(AssertSqlSafe(self.sql.acquire.as_str()))
                .bind(row.owner.as_str())
                .bind(self.holder.as_str())
                .bind(to_i64_micros(row.ttl))
                .bind(tick_us)
                .bind(row.name.as_str())
                .bind(tick_us)
                .bind(tick_us)
                .fetch_optional(&mut *tx)
                .await?;
            if let Some(fence) = fence {
                unclaimed.arm();
                tx.commit().await?;
                return Ok(Some((to_u64(fence), sent)));
            }
            tx.rollback().await?;
            if created || !self.create_row(&mut conn, &row.name).await? {
                return Ok(None);
            }
            created = true;
        }
    }

    #[cfg(feature = "sqlx-postgres")]
    async fn create_row(
        &self,
        conn: &mut sqlx::PgConnection,
        name: &str,
    ) -> Result<bool, sqlx::Error> {
        let inserted = sqlx::query(AssertSqlSafe(self.sql.create.as_str()))
            .bind(name)
            .execute(conn)
            .await?
            .rows_affected();
        Ok(inserted == 1)
    }

    /// MySQL has no `RETURNING`: the `UPDATE` reports a matched row (sqlx
    /// sets `CLIENT_FOUND_ROWS`, and the new owner changes the row anyway),
    /// then the fence is read back under the fresh owner, in the same
    /// transaction, so a failed read rolls the `UPDATE` back. The row is
    /// created as on Postgres.
    #[cfg(feature = "sqlx-mysql")]
    async fn acquire_mysql(
        &self,
        pool: &sqlx::MySqlPool,
        row: &LeaseRow,
        tick_us: Option<i64>,
        unclaimed: &mut Unclaimed,
    ) -> Result<Option<(u64, Instant)>, sqlx::Error> {
        let mut conn = pool.acquire().await?;
        let mut created = false;
        loop {
            let sent = Instant::now();
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            let matched = sqlx::query(AssertSqlSafe(self.sql.acquire.as_str()))
                .bind(row.owner.as_str())
                .bind(self.holder.as_str())
                .bind(to_i64_micros(row.ttl))
                .bind(tick_us)
                .bind(row.name.as_str())
                .bind(tick_us)
                .bind(tick_us)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            let fence: Option<i64> = if matched == 1 {
                sqlx::query_scalar(AssertSqlSafe(self.sql.read_fence.as_str()))
                    .bind(row.name.as_str())
                    .bind(row.owner.as_str())
                    .fetch_optional(&mut *tx)
                    .await?
            } else {
                None
            };
            if let Some(fence) = fence {
                unclaimed.arm();
                tx.commit().await?;
                return Ok(Some((to_u64(fence), sent)));
            }
            // Ends the transaction, and with it the locks the `UPDATE` took.
            tx.rollback().await?;
            if created {
                return Ok(None);
            }
            // INSERT IGNORE: the values are validated, so it can only skip
            // the duplicate key; the count is 1 or 0 under either row-count
            // flag, unlike ON DUPLICATE KEY UPDATE.
            let inserted = sqlx::query(AssertSqlSafe(self.sql.create.as_str()))
                .bind(row.name.as_str())
                .execute(&mut *conn)
                .await?
                .rows_affected();
            if inserted == 0 {
                return Ok(None);
            }
            created = true;
        }
    }

    async fn renew_row(&self, name: &str, owner: &str, ttl: Duration) -> Result<bool, AppError> {
        let sql = AssertSqlSafe(self.sql.renew.as_str());
        let matched = on_pool!(&self.pool, |pool| sqlx::query(sql)
            .bind(to_i64_micros(ttl))
            .bind(name)
            .bind(owner)
            .execute(pool)
            .await
            .map(|done| done.rows_affected()))
        .map_err(to_app_error)?;
        Ok(matched == 1)
    }

    async fn release_row(&self, name: &str, owner: &str) -> Result<(), AppError> {
        let sql = AssertSqlSafe(self.sql.release.as_str());
        on_pool!(&self.pool, |pool| sqlx::query(sql)
            .bind(name)
            .bind(owner)
            .execute(pool)
            .await
            .map(drop))
        .map_err(to_app_error)?;
        tracing::debug!(lease = %name, holder = %self.holder, "lease released");
        Ok(())
    }
}

/// A held lease, renewed by hand ([`renew`](Self::renew)) or in the
/// background ([`keep_alive`](Self::keep_alive)).
///
/// Dropping it without [`release`](Self::release) lets it expire after its
/// TTL.
pub struct Lease {
    row: Arc<LeaseRow>,
    fence: FencingToken,
    renewed_at: Instant,
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("name", &self.row.name)
            .field("holder", &self.row.store.holder)
            .field("fence", &self.fence)
            .field("ttl", &self.row.ttl)
            .field("valid_until", &self.valid_until())
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// The lease name.
    pub fn name(&self) -> &str {
        &self.row.name
    }

    /// The fencing token of this acquisition.
    pub fn fence(&self) -> FencingToken {
        self.fence
    }

    /// Until when this process may assume it holds the lease (tokio
    /// clock): the TTL from when the last successful acquisition or
    /// renewal was sent, minus a tenth for clock-rate drift.
    pub fn valid_until(&self) -> Instant {
        valid_until(self.renewed_at, self.row.ttl)
    }

    /// Extend the lease by its TTL. `ABORTED` / `LEASE_LOST` when it is no
    /// longer ours (expired, or taken over).
    pub async fn renew(&mut self) -> Result<(), AppError> {
        let sent = Instant::now();
        if self
            .row
            .store
            .renew_row(&self.row.name, &self.row.owner, self.row.ttl)
            .await?
        {
            self.renewed_at = sent;
            Ok(())
        } else {
            Err(lease_lost(&self.row.name, self.fence))
        }
    }

    /// Give the lease up; the fence stays. A no-op when it is no longer ours.
    pub async fn release(self) -> Result<(), AppError> {
        self.row
            .store
            .release_row(&self.row.name, &self.row.owner)
            .await
    }

    /// Renew in the background until released or lost.
    ///
    /// # Panics
    ///
    /// Outside a tokio runtime.
    pub fn keep_alive(self) -> HeldLease {
        let labels = Labels {
            name: self.row.name.clone(),
            holder: self.row.store.holder.clone(),
            fence: self.fence,
        };
        let ttl = self.row.ttl;
        HeldLease::start(self.row, labels, ttl, self.renewed_at)
    }
}

/// One acquisition of a row. `owner` is a capability and never logged.
struct LeaseRow {
    store: LeaseStore,
    name: String,
    owner: String,
    ttl: Duration,
}

impl Renew for LeaseRow {
    fn renew(&self) -> BoxFut<'_, Result<bool, AppError>> {
        Box::pin(self.store.renew_row(&self.name, &self.owner, self.ttl))
    }

    fn release(&self) -> BoxFut<'_, Result<(), AppError>> {
        Box::pin(self.store.release_row(&self.name, &self.owner))
    }
}

fn dialect(pool: &Pool) -> Dialect {
    match pool {
        #[cfg(feature = "sqlx-postgres")]
        Pool::Postgres(_) => Dialect::Postgres,
        #[cfg(feature = "sqlx-mysql")]
        Pool::MySql(_) => Dialect::MySql,
    }
}

/// `sent + ttl − ttl / 10`: the database's expiry is at least `ttl` after
/// `sent`; the tenth covers clock-rate drift between the two clocks.
pub(super) fn valid_until(sent: Instant, ttl: Duration) -> Instant {
    sent + ttl - ttl / 10
}

fn lease_lost(name: &str, fence: FencingToken) -> AppError {
    AppError::new(ErrorCode::Aborted, "the lease is no longer held")
        .with_reason(LEASE_LOST)
        .with_metadata("lease", name)
        .with_metadata("fence", fence.to_string())
}

/// `LEASE_SCHEMA_MISSING` for an undefined table or column; anything else
/// is classified like every other database failure, so a deadlock or a
/// timeout stays retryable.
fn schema_error(table: &str, error: sqlx::Error) -> AppError {
    if !undefined_table_or_column(&error) {
        return to_app_error(error);
    }
    AppError::failed_precondition(format!(
        "lease table `{table}` is missing or incomplete; create it with \
         LeaseStore::schema_sql in a migration or LeaseStore::ensure_schema"
    ))
    .with_reason(LEASE_SCHEMA_MISSING)
    .with_metadata("table", table.to_owned())
    .with_source(error)
}

/// Postgres reports SQLSTATE `42P01` / `42703`; MySQL and MariaDB report
/// errno 1146 / 1054 with SQLSTATE `42S02` / `42S22`.
fn undefined_table_or_column(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if matches!(
                database.code().as_deref(),
                Some("42P01" | "42703" | "42S02" | "42S22")
            )
    )
}

fn fence_held(held: bool, name: &str, fence: FencingToken) -> Result<(), AppError> {
    if held {
        Ok(())
    } else {
        Err(lease_lost(name, fence))
    }
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_i64_micros(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

pub(super) fn validate_name(name: &str) -> Result<(), AppError> {
    let valid = (1..=MAX_NAME_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AppError::invalid_argument(
            "a lease name is 1-200 bytes of letters, digits and `._:/-`",
        ))
    }
}

pub(super) fn validate_ttl(ttl: Duration) -> Result<(), AppError> {
    if (MIN_TTL..=MAX_TTL).contains(&ttl) {
        Ok(())
    } else {
        Err(AppError::invalid_argument(format!(
            "a lease TTL is between 1 s and 24 h, got {} ms",
            ttl.as_millis()
        )))
    }
}

fn validate_table(table: &str) -> Result<(), AppError> {
    let valid = (1..=MAX_TABLE_BYTES).contains(&table.len())
        && !table.starts_with(|c: char| c.is_ascii_digit())
        && table
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(AppError::invalid_argument(
            "a lease table name is 1-63 bytes of lowercase letters, digits and `_`, \
             not starting with a digit",
        ))
    }
}

fn validate_holder(holder: &str) -> Result<(), AppError> {
    let valid = (1..=MAX_HOLDER_BYTES).contains(&holder.len())
        && holder.bytes().all(|b| b.is_ascii_graphic());
    if valid {
        Ok(())
    } else {
        Err(AppError::invalid_argument(
            "a lease holder label is 1-200 bytes of visible ASCII",
        ))
    }
}

fn tick_micros(tick: SystemTime) -> Result<i64, AppError> {
    tick.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_micros()).ok())
        .ok_or_else(|| AppError::invalid_argument("a lease tick is at or after the Unix epoch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_restricted() {
        let longest = "n".repeat(200);
        let too_long = "n".repeat(201);
        for name in [
            "orders-sync",
            "a",
            "billing/nightly:v2.1_x",
            longest.as_str(),
        ] {
            validate_name(name).unwrap();
        }
        for name in ["", "with space", "orders\nsync", "é", too_long.as_str()] {
            let error = validate_name(name).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{name:?}");
        }
    }

    #[test]
    fn ttls_are_bounded() {
        validate_ttl(Duration::from_secs(1)).unwrap();
        validate_ttl(DEFAULT_LEASE_TTL).unwrap();
        validate_ttl(Duration::from_hours(24)).unwrap();
        for ttl in [
            Duration::ZERO,
            Duration::from_millis(999),
            Duration::from_hours(24) + Duration::from_nanos(1),
        ] {
            let error = validate_ttl(ttl).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert!(error.message().starts_with("a lease TTL"));
        }
    }

    #[test]
    fn tables_are_plain_identifiers() {
        let longest = "t".repeat(63);
        let too_long = "t".repeat(64);
        for table in ["sekvent_leases", "_leases", "leases2", longest.as_str()] {
            validate_table(table).unwrap();
        }
        for table in [
            "",
            "2leases",
            "Leases",
            "app.leases",
            "leases;drop",
            "\"leases\"",
            too_long.as_str(),
        ] {
            assert_eq!(
                validate_table(table).unwrap_err().code(),
                ErrorCode::InvalidArgument,
                "{table:?}"
            );
        }
    }

    #[test]
    fn holders_are_visible_ascii() {
        validate_holder("orders-7f9c-pod.eu-1").unwrap();
        validate_holder(&"h".repeat(200)).unwrap();
        let too_long = "h".repeat(201);
        for holder in ["", "two words", "tab\t", "ü", too_long.as_str()] {
            assert_eq!(
                validate_holder(holder).unwrap_err().code(),
                ErrorCode::InvalidArgument,
                "{holder:?}"
            );
        }
    }

    #[test]
    fn ticks_are_whole_microseconds_since_the_epoch() {
        assert_eq!(tick_micros(UNIX_EPOCH).unwrap(), 0);
        assert_eq!(
            tick_micros(UNIX_EPOCH + Duration::from_nanos(1_500_999)).unwrap(),
            1_500
        );
        let error = tick_micros(UNIX_EPOCH - Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn fencing_tokens_order_and_print_as_numbers() {
        let token = FencingToken::new(41);
        assert_eq!(token.get(), 41);
        assert_eq!(token.to_string(), "41");
        assert!(FencingToken::new(42) > token);
    }

    #[test]
    fn conversions_saturate() {
        assert_eq!(to_u64(-1), 0);
        assert_eq!(to_u64(5), 5);
        assert_eq!(to_i64(u64::MAX), i64::MAX);
        assert_eq!(to_i64_micros(Duration::from_secs(2)), 2_000_000);
        assert_eq!(to_i64_micros(Duration::MAX), i64::MAX);
    }

    #[test]
    fn lost_leases_are_aborted_with_the_reason() {
        let error = fence_held(false, "orders-sync", FencingToken::new(3)).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Aborted);
        assert_eq!(error.reason(), Some(LEASE_LOST));
        assert_eq!(error.metadata()["lease"], "orders-sync");
        assert_eq!(error.metadata()["fence"], "3");
        fence_held(true, "orders-sync", FencingToken::new(3)).unwrap();
    }

    #[derive(Debug)]
    struct FakeDbError(&'static str);

    impl fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("fake database error")
        }
    }

    impl std::error::Error for FakeDbError {}

    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &'static str {
            "fake database error"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn database(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError(code)))
    }

    #[test]
    fn only_an_undefined_table_or_column_means_the_schema_is_missing() {
        for code in ["42P01", "42703", "42S02", "42S22"] {
            let error = schema_error("app_leases", database(code));
            assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{code}");
            assert_eq!(error.reason(), Some(LEASE_SCHEMA_MISSING));
            assert_eq!(error.metadata()["table"], "app_leases");
            assert!(error.message().contains("app_leases"));
        }
        for (error, code, reason) in [
            (database("40P01"), ErrorCode::Aborted, None),
            (database("40001"), ErrorCode::Aborted, None),
            (
                database("42501"),
                ErrorCode::PermissionDenied,
                Some(crate::reasons::DB_PERMISSION_DENIED),
            ),
            (database("42601"), ErrorCode::Internal, None),
            (sqlx::Error::PoolTimedOut, ErrorCode::Unavailable, None),
        ] {
            let mapped = schema_error("app_leases", error);
            assert_eq!(mapped.code(), code);
            assert_eq!(mapped.reason(), reason);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn validity_is_the_ttl_minus_a_tenth() {
        let now = Instant::now();
        assert_eq!(
            valid_until(now, Duration::from_secs(30)),
            now + Duration::from_secs(27)
        );
    }

    #[cfg(feature = "sqlx-postgres")]
    mod postgres {
        use super::*;

        fn unreachable_pool() -> Pool {
            Pool::Postgres(
                sqlx::postgres::PgPoolOptions::new()
                    .acquire_timeout(Duration::from_millis(200))
                    .connect_lazy("postgres://u:hunter2@127.0.0.1:1/orders")
                    .unwrap(),
            )
        }

        #[tokio::test]
        async fn stores_are_configured_and_validated_before_any_query() {
            let pool = unreachable_pool();
            let store = LeaseStore::new(&pool);
            assert_eq!(store.table(), DEFAULT_LEASE_TABLE);
            assert_eq!(store.holder(), format!("pid-{}", std::process::id()));
            assert!(
                store
                    .schema_sql()
                    .starts_with("CREATE TABLE IF NOT EXISTS \"sekvent_leases\"")
            );

            let store = store
                .with_table("app_leases")
                .unwrap()
                .with_holder("orders-0")
                .unwrap();
            assert!(store.schema_sql().contains("\"app_leases\""));
            let debug = format!("{store:?}");
            assert!(debug.contains("app_leases") && debug.contains("orders-0"));
            assert!(!debug.contains("hunter2"));
            assert!(store.clone().with_table("App").is_err());
            assert!(store.clone().with_holder("").is_err());

            let invalid = |result: Result<Option<Lease>, AppError>| {
                assert_eq!(result.unwrap_err().code(), ErrorCode::InvalidArgument);
            };
            invalid(store.try_acquire("", DEFAULT_LEASE_TTL).await);
            invalid(store.try_acquire("orders", Duration::ZERO).await);
            invalid(
                store
                    .try_acquire_tick(
                        "orders",
                        UNIX_EPOCH - Duration::from_secs(1),
                        DEFAULT_LEASE_TTL,
                    )
                    .await,
            );
            invalid(
                store
                    .try_acquire_tick("bad name", SystemTime::now(), DEFAULT_LEASE_TTL)
                    .await,
            );
            assert_eq!(
                store.last_tick("").await.unwrap_err().code(),
                ErrorCode::InvalidArgument
            );
            assert_eq!(
                store.info("").await.unwrap_err().code(),
                ErrorCode::InvalidArgument
            );
            pool.close().await;
        }

        #[tokio::test]
        async fn an_unreachable_database_is_unavailable() {
            let pool = unreachable_pool();
            let store = LeaseStore::new(&pool);
            let guard = Duration::from_secs(30);
            let unavailable = |error: AppError| {
                assert_eq!(error.code(), ErrorCode::Unavailable);
                assert!(!error.to_wire().message.contains("hunter2"));
            };
            let outcome = tokio::time::timeout(guard, async {
                unavailable(
                    store
                        .try_acquire("orders", DEFAULT_LEASE_TTL)
                        .await
                        .unwrap_err(),
                );
                unavailable(store.last_tick("orders").await.unwrap_err());
                unavailable(store.info("orders").await.unwrap_err());
                unavailable(store.ensure_schema().await.unwrap_err());
                // Unreachable is not "schema missing".
                unavailable(store.verify_schema().await.unwrap_err());
                unavailable(
                    store
                        .renew_row("orders", "owner", DEFAULT_LEASE_TTL)
                        .await
                        .unwrap_err(),
                );
                unavailable(store.release_row("orders", "owner").await.unwrap_err());
            })
            .await;
            assert!(outcome.is_ok(), "an unreachable database must fail fast");
            pool.close().await;
        }
    }

    #[cfg(feature = "sqlx-mysql")]
    #[tokio::test]
    async fn mysql_stores_use_the_mysql_dialect() {
        let pool = Pool::MySql(sqlx::MySqlPool::connect_lazy("mysql://u@192.0.2.1:1/a").unwrap());
        let store = LeaseStore::new(&pool);
        assert!(store.schema_sql().ends_with(") ENGINE = InnoDB;\n"));
        pool.close().await;
    }
}
