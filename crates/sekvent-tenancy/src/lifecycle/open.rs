//! A tenant's database opened for one lifecycle operation: a pool of at most two connections as
//! the tenant's role, migrated, outside the tenant cache, closed when the operation ends.
//!
//! Review rule: [`LifecycleCapability::new`] is called only in `provision.rs`, `state.rs`,
//! `adopt.rs` and `usage.rs`. `select`, the resolver, the tenant cache, `purge.rs` and the admin
//! surface never call it; a purge never opens the tenant.

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use sekvent_error::AppError;
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::time::timeout;

use super::hooks::TenantHooks;
use super::provisioner::Provisioner;
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
    pool: PgPool,
}

impl LifecyclePool {
    /// The pool, connected as the tenant's role.
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Closes the pool. A close that takes longer than 10 s is abandoned; the connections are
    /// dropped with the pool.
    pub(crate) async fn close(self) {
        close_pool(&self.pool).await;
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
/// (`TENANT_STATE`): the stored names checked (`Provisioner::stored_names`), a pool of at most
/// two connections with the connector's options for the tenant's derived role under the row's
/// role key, connected, and the tenant migrator run on it (for `ReadOnly`, on a read-write pool
/// of its own first).
///
/// A failure to connect or to migrate is `UNAVAILABLE` without a reason ("the tenant's database
/// could not be opened"); its source is a fixed stage text (`connect`, `migrate`) with at most a
/// SQLSTATE, never a driver message or connect options.
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
    let options = inner
        .connector
        .options(row.info.id, &role, &database, &row.info.role_key)?;
    match mode {
        OpenMode::ReadWrite => {
            let pool = connect(options, MAX_CONNECTIONS).await?;
            migrate(inner.migrator, &pool).await?;
            Ok(LifecyclePool { pool })
        }
        OpenMode::ReadOnly => {
            let writer = connect(options.clone(), 1).await?;
            migrate(inner.migrator, &writer).await?;
            close_pool(&writer).await;
            let pool = connect(options.options(READ_ONLY_SETTINGS), MAX_CONNECTIONS).await?;
            Ok(LifecyclePool { pool })
        }
    }
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
    /// Connecting as the tenant's role.
    Connect,
    /// Running the tenant migrator.
    Migrate,
}

impl Stage {
    const fn as_str(self) -> &'static str {
        match self {
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
        Connector, Maintenance, ObjectNames, Registry, RoleKey, RoleSecrets, TenantId, TenantInfo,
        reasons,
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

    /// A provisioner built without I/O.
    fn provisioner() -> Provisioner<Hooks> {
        let clock = Arc::new(ManualClock::new(SystemTime::UNIX_EPOCH));
        let registry = Registry::new(lazy_pool(3), clock).expect("three connections");
        let secrets = RoleSecrets::parse(
            &Secret::new("k1:correct horse battery staple 936".to_owned()),
            "example-app.role.v1",
        )
        .expect("the test secrets parse");
        let channel = Channel::new("orders_changes").expect("a channel name");
        Provisioner::builder(
            registry,
            Maintenance::new(nowhere(), names()),
            Connector::new(nowhere(), secrets),
            &MIGRATOR,
            Sender::new(lazy_pool(1), channel),
            Hooks,
        )
        .role_key_id("k1")
        .connection_limit(2)
        .tenant_cap(10)
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
            pool: lazy_pool(MAX_CONNECTIONS),
        };
        assert_eq!(format!("{pool:?}"), "LifecyclePool(..)");
        assert_eq!(pool.pool().size(), 0);
        let inner = pool.pool().clone();
        pool.close().await;
        assert!(inner.is_closed());
    }
}
