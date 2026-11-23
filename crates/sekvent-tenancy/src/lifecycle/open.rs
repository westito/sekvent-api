//! A tenant's database for one lifecycle operation. An active tenant that this process's tenant
//! cache holds warm lends the operation its entry's pool, held so the entry is not evicted.
//! Otherwise a pool of at most two connections is opened as the tenant's role, migrated, outside
//! the tenant cache, and closed when the operation ends; while it exists it holds one slot of the
//! cache's capacity, so the cache's connection budget covers it.
//!
//! Review rule: [`LifecycleCapability::new`] is called only in `provision.rs`, `state.rs`,
//! `adopt.rs` and `usage.rs`. `select`, the resolver, the tenant cache, `purge.rs` and the admin
//! surface never call it; a purge never opens the tenant.

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use sekvent_db::KeyedReservation;
use sekvent_db::reasons::KEYED_CAPACITY;
use sekvent_error::AppError;
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::time::timeout;

use super::hooks::TenantHooks;
use super::provisioner::Provisioner;
use crate::error::serving;
use crate::tenants::{PoolLease, TenantCache};
use crate::{TenantRow, TenantState};

/// The most connections a lifecycle pool holds.
const MAX_CONNECTIONS: u32 = 2;

/// How long taking a connection from a lifecycle pool, the first included, may take.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long closing a lifecycle pool may take; past it the connections are dropped with the pool.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// The session settings of a read-only lifecycle pool.
const READ_ONLY_SETTINGS: [(&str, &str); 2] = [
    ("default_transaction_read_only", "on"),
    ("statement_timeout", "5s"),
];

/// The answer to a tenant that could not be opened for a lifecycle operation.
const OPEN_FAILED: &str = "the tenant's database could not be opened";

/// Permission to open a tenant that is not `active`. Only lifecycle operations make one.
pub(crate) struct LifecycleCapability(());

impl LifecycleCapability {
    /// The capability. See the module's review rule.
    pub(crate) fn new() -> Self {
        Self(())
    }
}

/// A tenant's migrated pool for one operation. `Debug` prints the type name only.
pub(crate) struct LifecyclePool {
    held: Held,
}

/// Where a lifecycle pool comes from.
enum Held {
    /// The warm cache entry of an active tenant, held for the operation.
    Warm(PoolLease),
    /// A pool opened for the operation, and the cache slot it holds (none without a keyed
    /// cache).
    Opened {
        pool: PgPool,
        slot: Option<KeyedReservation>,
    },
}

impl LifecyclePool {
    /// The pool, connected as the tenant's role.
    pub(crate) fn pool(&self) -> &PgPool {
        match &self.held {
            Held::Warm(lease) => &lease.pool,
            Held::Opened { pool, .. } => pool,
        }
    }

    /// Whether the pool is a warm cache entry's.
    #[cfg(test)]
    fn is_warm(&self) -> bool {
        matches!(self.held, Held::Warm(_))
    }

    /// Ends the operation's use. A warm entry's lease is dropped and the pool stays the cache's;
    /// an opened pool is closed, then its cache slot given back. A close that takes longer than
    /// 10 s is abandoned; the connections are dropped with the pool.
    pub(crate) async fn close(self) {
        match self.held {
            Held::Warm(lease) => drop(lease),
            Held::Opened { pool, slot } => {
                close_pool(&pool).await;
                drop(slot);
            }
        }
    }
}

// `LifecyclePool(..)`
impl fmt::Debug for LifecyclePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LifecyclePool(..)")
    }
}

/// How a lifecycle pool's sessions run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenMode {
    /// Read-write sessions (migrate, seed, adopted, before suspend, before resume).
    ReadWrite,
    /// Sessions with `default_transaction_read_only = on` and `statement_timeout = 5s` (usage,
    /// describe). The migrator runs first on a read-write pool of one connection, closed before
    /// the read-only pool is returned.
    ReadOnly,
}

/// Opens `row`'s tenant for one operation, in any state but `purging` and `purged`
/// (`TENANT_STATE`), after the stored names are checked (`Provisioner::stored_names`).
///
/// An `active` row whose tenant the provisioner's tenant cache holds warm, with the database,
/// the role and the role key the row names, gets that entry's pool, held for the operation
/// (`mode` does not apply: the sessions are the cache's). Otherwise one slot of the cache's
/// capacity is reserved (`TENANT_BUSY`, retried after a second, when every slot is in use, being
/// opened or reserved, before any connection), then a pool of at most two connections with the
/// connector's options for the tenant's derived role under the row's role key is connected and
/// the tenant migrator run on it (for `ReadOnly`, on a read-write pool of its own first).
///
/// A failure to reserve for another cause (a closed cache), to connect or to migrate is
/// `UNAVAILABLE` without a reason ("the tenant's database could not be opened"); its source is a
/// fixed stage text (`reserve`, `connect`, `migrate`) with at most a SQLSTATE, never a driver
/// message or connect options.
pub(crate) async fn open<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    capability: &LifecycleCapability,
    row: &TenantRow,
    mode: OpenMode,
) -> Result<LifecyclePool, AppError> {
    let _ = capability;
    if matches!(row.info.state, TenantState::Purging | TenantState::Purged) {
        return Err(crate::error::tenant_state());
    }
    let (role, database) = provisioner.stored_names(row)?;
    let inner = &provisioner.inner;
    let cache = inner.cache.as_deref();
    if let Some(lease) = warm(cache, row) {
        return Ok(LifecyclePool {
            held: Held::Warm(lease),
        });
    }
    let options = inner
        .connector
        .options(row.info.id, &role, &database, &row.info.role_key)?;
    let slot = reserve(cache).await?;
    let pool = match mode {
        OpenMode::ReadWrite => {
            let pool = connect(options, MAX_CONNECTIONS).await?;
            migrate(inner.migrator, &pool).await?;
            pool
        }
        OpenMode::ReadOnly => {
            let writer = connect(options.clone(), 1).await?;
            migrate(inner.migrator, &writer).await?;
            close_pool(&writer).await;
            connect(options.options(READ_ONLY_SETTINGS), MAX_CONNECTIONS).await?
        }
    };
    Ok(LifecyclePool {
        held: Held::Opened { pool, slot },
    })
}

/// The warm entry of `row`'s tenant: only for an `active` row, and only when the entry's pool
/// connects to the database and as the role the row names, with the row's role key.
fn warm(cache: Option<&dyn TenantCache>, row: &TenantRow) -> Option<PoolLease> {
    if row.info.state != TenantState::Active {
        return None;
    }
    let lease = cache?.lease_pool(row.info.id)?;
    let options = lease.pool.connect_options();
    let same = lease.info.role_key == row.info.role_key
        && options.get_database() == Some(row.db_name.as_str())
        && options.get_username() == row.db_role;
    same.then_some(lease)
}

/// One slot of the cache's capacity; `None` without a cache or for the fixed form. A full cache
/// is `TENANT_BUSY`; any other refusal `UNAVAILABLE` without a reason.
async fn reserve(cache: Option<&dyn TenantCache>) -> Result<Option<KeyedReservation>, AppError> {
    let Some(cache) = cache else {
        return Ok(None);
    };
    cache.reserve().await.map_err(|error| {
        if error.reason() == Some(KEYED_CAPACITY) {
            serving::busy()
        } else {
            open_failed(Stage::Reserve, None)
        }
    })
}

/// A pool of at most `max` connections on `options`, its first connection made now.
async fn connect(options: PgConnectOptions, max: u32) -> Result<PgPool, AppError> {
    PgPoolOptions::new()
        .max_connections(max)
        .min_connections(0)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect_with(options)
        .await
        .map_err(|error| open_failed(Stage::Connect, sqlstate_in(&error)))
}

/// Runs `migrator` on `pool`; on a failure the pool is closed.
async fn migrate(migrator: &Migrator, pool: &PgPool) -> Result<(), AppError> {
    match migrator.run(pool).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let sqlstate = sqlstate_in(&error);
            close_pool(pool).await;
            Err(open_failed(Stage::Migrate, sqlstate))
        }
    }
}

/// Closes `pool` within [`CLOSE_TIMEOUT`].
async fn close_pool(pool: &PgPool) {
    // Past the bound the connections are dropped with the pool.
    let _ = timeout(CLOSE_TIMEOUT, pool.close()).await;
}

/// Where opening a tenant for a lifecycle operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Reserving a slot of the tenant cache.
    Reserve,
    /// Connecting as the tenant's role.
    Connect,
    /// Running the tenant migrator.
    Migrate,
}

impl Stage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Reserve => "reserve",
            Self::Connect => "connect",
            Self::Migrate => "migrate",
        }
    }
}

/// Why opening a tenant for a lifecycle operation failed. Its text is fixed words and, at most, a
/// five-character SQLSTATE: never a name, a host, an option, a credential or a driver's text.
#[derive(Debug)]
struct OpenFailed {
    stage: Stage,
    sqlstate: Option<String>,
}

impl fmt::Display for OpenFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "opening the tenant for a lifecycle operation failed: {}",
            self.stage.as_str()
        )?;
        if let Some(code) = &self.sqlstate {
            write!(f, " (sqlstate {code})")?;
        }
        Ok(())
    }
}

impl StdError for OpenFailed {}

/// `UNAVAILABLE` without a reason, with nothing but `OpenFailed` as its source.
fn open_failed(stage: Stage, sqlstate: Option<String>) -> AppError {
    AppError::unavailable(OPEN_FAILED).with_source(OpenFailed { stage, sqlstate })
}

/// The first printable SQLSTATE of a `sqlx::Error` in `error`'s source chain, `error` itself
/// included. Formats nothing.
fn sqlstate_in(error: &(dyn StdError + 'static)) -> Option<String> {
    let mut next = Some(error);
    while let Some(link) = next {
        let code = link
            .downcast_ref::<sqlx::Error>()
            .and_then(sekvent_db::sqlstate)
            .filter(|code| is_sqlstate(code));
        if let Some(code) = code {
            return Some(code.into_owned());
        }
        next = link.source();
    }
    None
}

/// Five ASCII letters or digits.
fn is_sqlstate(code: &str) -> bool {
    code.len() == 5 && code.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;
    use std::time::SystemTime;

    use sekvent_config::Secret;
    use sekvent_context::ManualClock;
    use sekvent_error::ErrorCode;
    use sqlx::error::{DatabaseError, ErrorKind};
    use sqlx::migrate::MigrateError;
    use uuid::Uuid;

    use super::*;
    use crate::control::{MAIN_CLUSTER, ProvisionStep};
    use crate::invalidation::{Channel, Sender};
    use crate::{
        Connector, Maintenance, ObjectNames, Registry, RoleKey, RoleSecrets, ScopeFactory,
        ScopeFuture, SelectorRules, TenantId, TenantInfo, Tenants, reasons,
    };

    static MIGRATOR: Migrator = Migrator::DEFAULT;

    struct Hooks;

    impl TenantHooks for Hooks {
        type Seed = ();
    }

    #[derive(Debug)]
    struct FakeDbError(&'static str);

    impl fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("driver text naming user app_t_owner")
        }
    }

    impl StdError for FakeDbError {}

    impl DatabaseError for FakeDbError {
        fn message(&self) -> &'static str {
            "driver text naming user app_t_owner"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    fn database_error(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError(code)))
    }

    /// Options nobody listens on.
    fn nowhere() -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(1)
    }

    /// A pool that never connects until asked.
    fn lazy_pool(max_connections: u32) -> PgPool {
        PgPoolOptions::new()
            .max_connections(max_connections)
            .connect_lazy_with(nowhere())
    }

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("a valid prefix")
    }

    fn secrets() -> RoleSecrets {
        RoleSecrets::parse(
            &Secret::new("k1:correct horse battery staple 936".to_owned()),
            "example-app.role.v1",
        )
        .expect("the test secrets parse")
    }

    /// A provisioner built without I/O.
    fn provisioner() -> Provisioner<Hooks> {
        provisioner_with(None)
    }

    /// A provisioner built without I/O, announcing to `tenants` when given.
    fn provisioner_with(tenants: Option<Tenants<()>>) -> Provisioner<Hooks> {
        let clock = Arc::new(ManualClock::new(SystemTime::UNIX_EPOCH));
        let registry = Registry::new(lazy_pool(3), clock).expect("three connections");
        let channel = Channel::new("orders_changes").expect("a channel name");
        let builder = Provisioner::builder(
            registry,
            Maintenance::new(nowhere(), names()),
            Connector::new(nowhere(), secrets()),
            &MIGRATOR,
            Sender::new(lazy_pool(1), channel),
            Hooks,
        )
        .role_key_id("k1")
        .connection_limit(2)
        .tenant_cap(10);
        match tenants {
            Some(tenants) => builder.tenants(tenants),
            None => builder,
        }
        .build()
        .expect("the provisioner settings are valid")
    }

    fn row(state: TenantState) -> TenantRow {
        let id = TenantId::from_uuid(Uuid::from_u128(7));
        TenantRow {
            info: TenantInfo {
                id,
                subdomain: "orders".to_owned(),
                display_name: "Orders".to_owned(),
                state,
                primary_host: "orders.example.com".to_owned(),
                hosts: vec!["orders.example.com".to_owned()],
                role_key: RoleKey {
                    key_id: "k1".to_owned(),
                    generation: 1,
                },
            },
            provision_step: ProvisionStep::Inserted,
            db_cluster: MAIN_CLUSTER.to_owned(),
            db_name: names().database(id).as_str().to_owned(),
            db_role: names().role(id).as_str().to_owned(),
            schema_version: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[tokio::test]
    async fn purging_and_purged_tenants_are_never_opened() {
        let provisioner = provisioner();
        for state in [TenantState::Purging, TenantState::Purged] {
            let error = open(
                &provisioner,
                &LifecycleCapability::new(),
                &row(state),
                OpenMode::ReadWrite,
            )
            .await
            .expect_err("the state refuses");
            assert_eq!(error.code(), ErrorCode::FailedPrecondition);
            assert_eq!(error.reason(), Some(reasons::TENANT_STATE));
        }
    }

    #[tokio::test]
    async fn a_row_changed_by_hand_is_refused_before_any_connection() {
        let provisioner = provisioner();
        let mut changed = row(TenantState::Provisioning);
        changed.db_role = "app_t_someone_else".to_owned();
        let error = open(
            &provisioner,
            &LifecycleCapability::new(),
            &changed,
            OpenMode::ReadOnly,
        )
        .await
        .expect_err("the stored role is not the derived one");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);

        let mut unknown_key = row(TenantState::Suspended);
        unknown_key.info.role_key.key_id = "k9".to_owned();
        let error = open(
            &provisioner,
            &LifecycleCapability::new(),
            &unknown_key,
            OpenMode::ReadWrite,
        )
        .await
        .expect_err("the key derives no password");
        assert_eq!(error.code(), ErrorCode::Internal);
    }

    #[test]
    fn a_failure_says_only_its_stage_and_sqlstate() {
        assert_eq!(Stage::Reserve.as_str(), "reserve");
        assert_eq!(Stage::Connect.as_str(), "connect");
        assert_eq!(Stage::Migrate.as_str(), "migrate");
        let error = open_failed(Stage::Migrate, Some("42P07".to_owned()));
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), None);
        assert_eq!(error.message(), OPEN_FAILED);
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("opening the tenant for a lifecycle operation failed: migrate (sqlstate 42P07)")
        );
        let plain = open_failed(Stage::Connect, None);
        assert_eq!(
            plain.source().map(ToString::to_string).as_deref(),
            Some("opening the tenant for a lifecycle operation failed: connect")
        );
    }

    #[test]
    fn the_sqlstate_comes_from_a_sqlx_error_in_the_chain_only() {
        assert_eq!(sqlstate_in(&AppError::unavailable("no source")), None);
        let io = AppError::unavailable("io").with_source(std::io::Error::other("42P07"));
        assert_eq!(sqlstate_in(&io), None);
        assert_eq!(sqlstate_in(&sqlx::Error::PoolTimedOut), None);
        assert_eq!(
            sqlstate_in(&database_error("28P01")).as_deref(),
            Some("28P01")
        );
        let migrate = MigrateError::Execute(database_error("42P07"));
        assert_eq!(sqlstate_in(&migrate).as_deref(), Some("42P07"));
        assert_eq!(sqlstate_in(&database_error("42P0'")), None);
        assert_eq!(sqlstate_in(&database_error("42P071")), None);
    }

    #[tokio::test]
    async fn a_lifecycle_pool_prints_its_type_name_and_closes() {
        let pool = LifecyclePool {
            held: Held::Opened {
                pool: lazy_pool(MAX_CONNECTIONS),
                slot: None,
            },
        };
        assert_eq!(format!("{pool:?}"), "LifecyclePool(..)");
        assert!(!pool.is_warm());
        assert_eq!(pool.pool().size(), 0);
        let inner = pool.pool().clone();
        pool.close().await;
        assert!(inner.is_closed());
    }

    /// A factory the unit tests never reach.
    struct Unreached;

    impl ScopeFactory for Unreached {
        type Scope = ();

        fn build<'a>(&'a self, _info: &'a TenantInfo, _pool: PgPool) -> ScopeFuture<'a, ()> {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    /// A keyed tenant cache of one slot over pools that never connect.
    fn one_slot_cache() -> Tenants<()> {
        let clock = Arc::new(ManualClock::new(SystemTime::UNIX_EPOCH));
        let registry = Registry::new(lazy_pool(3), clock).expect("three connections");
        Tenants::builder(
            registry,
            Connector::new(nowhere(), secrets()),
            names(),
            SelectorRules::new(),
            &MIGRATOR,
            Unreached,
        )
        .capacity(1)
        .build()
        .expect("the cache settings are valid")
    }

    #[tokio::test]
    async fn a_full_tenant_cache_answers_busy_before_any_connection() {
        let tenants = one_slot_cache();
        let provisioner = provisioner_with(Some(tenants.clone()));
        let held = TenantCache::reserve(&tenants)
            .await
            .expect("the one slot is free")
            .expect("a keyed cache reserves");
        assert_eq!(tenants.stats().reserved, 1);
        for mode in [OpenMode::ReadWrite, OpenMode::ReadOnly] {
            let error = open(
                &provisioner,
                &LifecycleCapability::new(),
                &row(TenantState::Active),
                mode,
            )
            .await
            .expect_err("the cache is full");
            assert_eq!(error.code(), ErrorCode::Unavailable);
            assert_eq!(error.reason(), Some(reasons::TENANT_BUSY));
            assert_eq!(error.domain(), None);
            assert_eq!(error.retry_after(), Some(Duration::from_secs(1)));
        }
        assert_eq!(tenants.stats().capacity_refusals, 2);

        drop(held);
        assert_eq!(tenants.stats().reserved, 0);

        tenants.close().await;
        let error = open(
            &provisioner,
            &LifecycleCapability::new(),
            &row(TenantState::Active),
            OpenMode::ReadWrite,
        )
        .await
        .expect_err("the cache is closed");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), None);
        assert_eq!(
            error.source().map(ToString::to_string).as_deref(),
            Some("opening the tenant for a lifecycle operation failed: reserve")
        );
    }

    #[tokio::test]
    async fn the_fixed_form_lends_no_pool_and_reserves_nothing() {
        let tenants = Tenants::fixed(());
        assert!(TenantCache::lease_pool(&tenants, TenantId::LOCAL).is_none());
        let slot = TenantCache::reserve(&tenants).await.expect("no budget");
        assert!(slot.is_none());
        assert!(
            warm(
                Some(&tenants as &dyn TenantCache),
                &row(TenantState::Active)
            )
            .is_none()
        );
        assert!(warm(None, &row(TenantState::Active)).is_none());
        assert!(reserve(None).await.expect("no cache").is_none());
    }
}
