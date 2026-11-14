//! A Postgres pool per key, and the value built on it: [`KeyedPools`].
//!
//! A [`Keyed`] cache built with [`KeyedBuilder::from_parts`]: its factory
//! asks the connector for the key's candidates, connects with the first one
//! the server accepts, runs `init` and `build_value` on the pool, and hands
//! the value to the cache with a finisher that closes the pool. Until the
//! value is handed over the pool sits in a [`PoolGuard`], which closes it on
//! every other way out.
//!
//! `PgConnectOptions` derives `Debug` with the password: nothing here formats
//! one, and a driver's connect error is reduced to its class and SQLSTATE
//! before anything keeps or logs it.

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use sekvent_error::{AppError, ErrorCode};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use super::task::{BoxFut, Tracker};
use super::{Keyed, KeyedBuilder, KeyedRef, KeyedStats, Made, Settings};
use crate::reasons::DB_CREDENTIALS_REJECTED;
use crate::{ConnectFailure, classify_connect, public_message};

/// The SQLSTATE of a rejected password (`invalid_password`): the one
/// connect failure that moves on to the next candidate.
const INVALID_PASSWORD: &str = "28P01";

/// The connect options to try for one key, in order. Never empty.
///
/// The next candidate is tried only when the server rejected the password of
/// the one before it (SQLSTATE `28P01`): a password in rotation. Any other
/// failure ends the creation. `Debug` shows how many candidates there are,
/// never one of them.
pub struct ConnectCandidates {
    first: PgConnectOptions,
    rest: Vec<PgConnectOptions>,
}

impl ConnectCandidates {
    /// One candidate.
    pub fn new(first: PgConnectOptions) -> Self {
        Self {
            first,
            rest: Vec::new(),
        }
    }

    /// Also try `next`, once the server rejected the password of every
    /// candidate before it.
    #[must_use]
    pub fn or(mut self, next: PgConnectOptions) -> Self {
        self.rest.push(next);
        self
    }

    fn len(&self) -> usize {
        1 + self.rest.len()
    }
}

impl From<PgConnectOptions> for ConnectCandidates {
    fn from(first: PgConnectOptions) -> Self {
        Self::new(first)
    }
}

impl fmt::Debug for ConnectCandidates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectCandidates")
            .field("count", &self.len())
            .finish()
    }
}

type Connector<K> =
    Arc<dyn Fn(K) -> BoxFut<'static, Result<ConnectCandidates, AppError>> + Send + Sync>;
type PoolHook<K, T> = Arc<dyn Fn(K, PgPool) -> BoxFut<'static, Result<T, AppError>> + Send + Sync>;

/// The pool options every key's pool is built with.
#[derive(Debug, Clone, Copy)]
struct PoolSettings {
    max_connections: u32,
    acquire_timeout: Duration,
    idle_timeout: Option<Duration>,
    max_lifetime: Option<Duration>,
}

impl PoolSettings {
    const DEFAULT: Self = Self {
        max_connections: 5,
        acquire_timeout: Duration::from_secs(5),
        idle_timeout: Some(Duration::from_mins(10)),
        max_lifetime: Some(Duration::from_mins(30)),
    };

    fn options(self) -> PgPoolOptions {
        PgPoolOptions::new()
            .max_connections(self.max_connections)
            .min_connections(0)
            .acquire_timeout(self.acquire_timeout)
            .idle_timeout(self.idle_timeout)
            .max_lifetime(self.max_lifetime)
    }

    fn validate(self) -> Result<(), AppError> {
        if self.max_connections == 0 {
            return Err(AppError::invalid_argument(
                "max_connections must be at least 1",
            ));
        }
        if self.acquire_timeout.is_zero() {
            return Err(AppError::invalid_argument(
                "acquire_timeout must be longer than zero",
            ));
        }
        if self.idle_timeout == Some(Duration::ZERO) {
            return Err(AppError::invalid_argument(
                "connection_idle_timeout must be longer than zero",
            ));
        }
        if self.max_lifetime == Some(Duration::ZERO) {
            return Err(AppError::invalid_argument(
                "connection_max_lifetime must be longer than zero",
            ));
        }
        Ok(())
    }
}

/// The cache's own settings: `Keyed`'s, except that an idle pool is
/// evicted after ten minutes by default.
const CACHE_DEFAULT: Settings = Settings {
    evict_idle_after: Some(Duration::from_mins(10)),
    ..Settings::DEFAULT
};

/// A Postgres pool per key, and the value built on it.
///
/// [`get`](Self::get) returns the key's value, creating it when the cache
/// does not hold it: the connector's candidates are tried in order (the next
/// one only after a rejected password), `init` runs on the new pool, then
/// `build_value` makes the value (without it the value is the pool). All of
/// it runs under `create_timeout`; a creation that does not publish closes
/// the pool it connected. A value leaves the cache like any [`Keyed`] value;
/// once no [`KeyedRef`] remains it is dropped and its pool closed.
///
/// Clones share the cache.
pub struct KeyedPools<K, V = PgPool> {
    cache: Keyed<K, V>,
}

impl<K, V> Clone for KeyedPools<K, V> {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
        }
    }
}

impl<K, V> fmt::Debug for KeyedPools<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedPools")
            .field("stats", &self.cache.inner.stats())
            .finish_non_exhaustive()
    }
}

impl<K> KeyedPools<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    /// Start a cache whose pools connect with what `connector` returns for
    /// a key: `PgConnectOptions`, or `ConnectCandidates` to try in order.
    ///
    /// Set every connect option explicitly: `PgConnectOptions::new()` reads
    /// the `PG*` environment variables.
    pub fn builder<C, Fut, O>(connector: C) -> KeyedPoolsBuilder<K>
    where
        C: Fn(K) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, AppError>> + Send + 'static,
        O: Into<ConnectCandidates> + 'static,
    {
        let connector: Connector<K> = Arc::new(
            move |key| -> BoxFut<'static, Result<ConnectCandidates, AppError>> {
                let connecting = connector(key);
                Box::pin(async move { connecting.await.map(Into::into) })
            },
        );
        let build: PoolHook<K, PgPool> =
            Arc::new(|_key, pool| -> BoxFut<'static, Result<PgPool, AppError>> {
                Box::pin(std::future::ready(Ok(pool)))
            });
        KeyedPoolsBuilder {
            connector,
            init: None,
            build,
            cache: CACHE_DEFAULT,
            pool: PoolSettings::DEFAULT,
        }
    }
}

impl<K, V> KeyedPools<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// The value of `key`, created if the cache does not hold it.
    ///
    /// Fails as [`Keyed::get`] does, with the connector's, `init`'s or
    /// `build_value`'s own error, or with a connect failure: `UNAVAILABLE`
    /// when the server could not be reached in time, `INTERNAL` /
    /// `DB_CREDENTIALS_REJECTED` when it refused the login, `INTERNAL`
    /// otherwise; each with the generic database message.
    pub async fn get(&self, key: &K) -> Result<KeyedRef<V>, AppError> {
        self.cache.get(key).await
    }

    /// The value of `key` if the cache holds one. Never creates, never
    /// waits, and does not count as a use.
    pub fn peek(&self, key: &K) -> Option<KeyedRef<V>> {
        self.cache.peek(key)
    }

    /// Drop the cache's value of `key`; whether there was one. Its pool is
    /// closed once no reference remains.
    pub async fn invalidate(&self, key: &K) -> bool {
        self.cache.invalidate(key).await
    }

    /// Evict every entry that was idle for `evict_idle_after`; how many.
    pub async fn evict_idle(&self) -> usize {
        self.cache.evict_idle().await
    }

    /// Evict every entry that nothing refers to and that was last used at
    /// least `idle` ago; how many.
    pub async fn evict_idle_older_than(&self, idle: Duration) -> usize {
        self.cache.evict_idle_older_than(idle).await
    }

    /// Drop every value, cancel every creation and refuse further `get`s;
    /// see [`Keyed::close`].
    pub async fn close(&self) {
        self.cache.close().await;
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.cache.is_closed()
    }

    /// A snapshot of the cache's gauges and counters.
    pub fn stats(&self) -> KeyedStats {
        self.cache.stats()
    }

    /// The runtime unit of this cache; see [`Keyed::into_unit`].
    #[cfg(feature = "runtime")]
    pub fn into_unit(
        self,
    ) -> impl FnMut(
        sekvent_runtime::UnitContext,
    ) -> futures::future::BoxFuture<'static, Result<(), AppError>>
    + Send
    + 'static {
        self.cache.into_unit()
    }
}

/// Builds a [`KeyedPools`] cache.
///
/// | Setting | Default | Refused |
/// |---|---|---|
/// | `capacity` | 64 | 0 |
/// | `evict_idle_after` | `Some(10 min)` | `Some(0)` |
/// | `create_timeout` | 30 s | 0 |
/// | `reap_interval` | 30 s | 0 |
/// | `max_connections` | 5 | 0 |
/// | minimum connections | 0, fixed | |
/// | `acquire_timeout` | 5 s | 0 |
/// | `connection_idle_timeout` | `Some(10 min)` | `Some(0)` |
/// | `connection_max_lifetime` | `Some(30 min)` | `Some(0)` |
pub struct KeyedPoolsBuilder<K, V = PgPool> {
    connector: Connector<K>,
    init: Option<PoolHook<K, ()>>,
    build: PoolHook<K, V>,
    cache: Settings,
    pool: PoolSettings,
}

impl<K, V> fmt::Debug for KeyedPoolsBuilder<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedPoolsBuilder")
            .field("capacity", &self.cache.capacity)
            .field("evict_idle_after", &self.cache.evict_idle_after)
            .field("create_timeout", &self.cache.create_timeout)
            .field("reap_interval", &self.cache.reap_interval)
            .field("max_connections", &self.pool.max_connections)
            .field("acquire_timeout", &self.pool.acquire_timeout)
            .field("connection_idle_timeout", &self.pool.idle_timeout)
            .field("connection_max_lifetime", &self.pool.max_lifetime)
            .finish_non_exhaustive()
    }
}

impl<K, V> KeyedPoolsBuilder<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// The most pools, warm and being created, the cache holds (default
    /// 64). The server must allow `capacity × max_connections` connections
    /// plus headroom for retiring pools.
    #[must_use]
    pub fn capacity(mut self, pools: usize) -> Self {
        self.cache.capacity = pools;
        self
    }

    /// How long a pool nothing refers to stays before
    /// [`KeyedPools::evict_idle`] evicts it (default `Some(10 min)`).
    #[must_use]
    pub fn evict_idle_after(mut self, idle: Option<Duration>) -> Self {
        self.cache.evict_idle_after = idle;
        self
    }

    /// How long one creation may take, every connect attempt, `init` and
    /// `build_value` together (default 30 s). Keep `acquire_timeout` below
    /// it.
    #[must_use]
    pub fn create_timeout(mut self, timeout: Duration) -> Self {
        self.cache.create_timeout = timeout;
        self
    }

    /// How often the unit of `into_unit` evicts idle pools (default 30 s).
    #[must_use]
    pub fn reap_interval(mut self, interval: Duration) -> Self {
        self.cache.reap_interval = interval;
        self
    }

    /// The most connections of one key's pool (default 5). A pool keeps no
    /// minimum.
    #[must_use]
    pub fn max_connections(mut self, max: u32) -> Self {
        self.pool.max_connections = max;
        self
    }

    /// How long acquiring a connection may take, and so how long one
    /// connect attempt may take (default 5 s).
    #[must_use]
    pub fn acquire_timeout(mut self, timeout: Duration) -> Self {
        self.pool.acquire_timeout = timeout;
        self
    }

    /// How long a connection may sit idle in its pool (default
    /// `Some(10 min)`; `None` keeps it).
    #[must_use]
    pub fn connection_idle_timeout(mut self, idle: Option<Duration>) -> Self {
        self.pool.idle_timeout = idle;
        self
    }

    /// How long a connection may live (default `Some(30 min)`; `None`
    /// forever).
    #[must_use]
    pub fn connection_max_lifetime(mut self, lifetime: Option<Duration>) -> Self {
        self.pool.max_lifetime = lifetime;
        self
    }

    /// Run `init` on a new pool before anything else sees it. Its error is
    /// the creation's error, and the pool is closed.
    #[must_use]
    pub fn init<F, Fut>(mut self, init: F) -> Self
    where
        F: Fn(K, PgPool) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        self.init = Some(Arc::new(
            move |key, pool| -> BoxFut<'static, Result<(), AppError>> { Box::pin(init(key, pool)) },
        ));
        self
    }

    /// The cache. `INVALID_ARGUMENT`, naming the setting, for the values
    /// the table of [`KeyedPoolsBuilder`] refuses.
    pub fn build(self) -> Result<KeyedPools<K, V>, AppError> {
        self.pool.validate()?;
        let parts = Arc::new(Parts {
            connector: self.connector,
            init: self.init,
            build: self.build,
            pool: self.pool,
        });
        let cache = KeyedBuilder::from_parts(
            move |key, tracker| -> BoxFut<'static, Result<Made<V>, AppError>> {
                Box::pin(create(Arc::clone(&parts), key, tracker))
            },
        )
        .settings(self.cache)
        .build()?;
        Ok(KeyedPools { cache })
    }
}

impl<K> KeyedPoolsBuilder<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    /// Cache what `build` makes of a key's pool instead of the pool. It
    /// runs once per creation, after `init`; its error is the creation's
    /// error, and the pool is closed. The value is dropped before its pool
    /// is closed: a value that owns tasks ends them in its `Drop`.
    #[must_use]
    pub fn build_value<V, F, Fut>(self, build: F) -> KeyedPoolsBuilder<K, V>
    where
        V: Send + Sync + 'static,
        F: Fn(K, PgPool) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<V, AppError>> + Send + 'static,
    {
        KeyedPoolsBuilder {
            connector: self.connector,
            init: self.init,
            build: Arc::new(move |key, pool| -> BoxFut<'static, Result<V, AppError>> {
                Box::pin(build(key, pool))
            }),
            cache: self.cache,
            pool: self.pool,
        }
    }
}

/// What every creation of one cache shares.
struct Parts<K, V> {
    connector: Connector<K>,
    init: Option<PoolHook<K, ()>>,
    build: PoolHook<K, V>,
    pool: PoolSettings,
}

/// One creation: candidates, connect, `init`, `build_value`, publish.
async fn create<K, V>(
    parts: Arc<Parts<K, V>>,
    key: K,
    tracker: Arc<Tracker>,
) -> Result<Made<V>, AppError>
where
    K: Clone + Send + 'static,
    V: Send + 'static,
{
    let candidates = (parts.connector)(key.clone()).await?;
    let guard = PoolGuard::new(connect(parts.pool, candidates).await?, tracker);
    if let Some(init) = &parts.init {
        init(key.clone(), guard.pool.clone()).await?;
    }
    let value = (parts.build)(key, guard.pool.clone()).await?;
    let pool = guard.disarm();
    Ok(Made {
        value,
        finisher: Some(Box::pin(close_pool(pool))),
    })
}

async fn close_pool(pool: PgPool) {
    pool.close().await;
}

/// Owns a connected pool until the creation publishes it. Dropped armed it
/// closes the pool on a tracked task, counted in `retiring`: closing, not
/// dropping, because a hook may still hold a clone.
struct PoolGuard {
    pool: PgPool,
    tracker: Arc<Tracker>,
    armed: bool,
}

impl PoolGuard {
    fn new(pool: PgPool, tracker: Arc<Tracker>) -> Self {
        Self {
            pool,
            tracker,
            armed: true,
        }
    }

    /// The pool, now the cache's to close.
    fn disarm(mut self) -> PgPool {
        self.armed = false;
        self.pool.clone()
    }
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        if self.armed {
            let closing = close_pool(self.pool.clone());
            drop(self.tracker.spawn_retiring(self.tracker.retire(), closing));
        }
    }
}

/// Connects with the first candidate the server accepts; moves on only
/// after a rejected password.
async fn connect(
    settings: PoolSettings,
    candidates: ConnectCandidates,
) -> Result<PgPool, AppError> {
    let total = candidates.len();
    let mut rest = candidates.rest.into_iter();
    let mut current = candidates.first;
    let mut tried = 1;
    loop {
        match settings.options().connect_with(current).await {
            Ok(pool) => {
                if tried > 1 {
                    tracing::info!(
                        candidate = tried,
                        candidates = total,
                        "keyed pool connected with a later connect candidate"
                    );
                }
                return Ok(pool);
            }
            Err(error) => {
                let next = if falls_back(&error) {
                    rest.next()
                } else {
                    None
                };
                match next {
                    Some(next) => {
                        current = next;
                        tried += 1;
                    }
                    None => return Err(connect_error(&error, tried, total)),
                }
            }
        }
    }
}

/// Whether a failed connect may be retried with the next candidate: only
/// when the server rejected the password.
fn falls_back(error: &sqlx::Error) -> bool {
    crate::sqlstate(error).as_deref() == Some(INVALID_PASSWORD)
}

/// The creation's error for a failed connect. The driver's error is neither
/// kept nor logged: its text names users, hosts and databases.
fn connect_error(error: &sqlx::Error, tried: usize, total: usize) -> AppError {
    let class = classify_connect(error);
    let code = if matches!(class, ConnectFailure::Unreachable | ConnectFailure::Timeout) {
        ErrorCode::Unavailable
    } else {
        ErrorCode::Internal
    };
    let app = AppError::new(code, public_message(code));
    let app = if class == ConnectFailure::Auth {
        app.with_reason(DB_CREDENTIALS_REJECTED)
    } else {
        app
    };
    app.with_source(ConnectFailed {
        class,
        sqlstate: printable_sqlstate(error),
        tried,
        total,
    })
}

/// The server's SQLSTATE when it is five ASCII letters or digits.
fn printable_sqlstate(error: &sqlx::Error) -> Option<String> {
    crate::sqlstate(error)
        .filter(|code| code.len() == 5 && code.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .map(std::borrow::Cow::into_owned)
}

/// The source of a connect failure: its class, the server's SQLSTATE and
/// how many candidates were tried, nothing of the driver's text.
#[derive(Debug)]
struct ConnectFailed {
    class: ConnectFailure,
    sqlstate: Option<String>,
    tried: usize,
    total: usize,
}

impl fmt::Display for ConnectFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "connect failed: {}", self.class)?;
        if let Some(sqlstate) = &self.sqlstate {
            write!(f, " (sqlstate {sqlstate})")?;
        }
        write!(f, ", {} of {} candidates tried", self.tried, self.total)
    }
}

impl StdError for ConnectFailed {}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use sqlx::error::{DatabaseError, ErrorKind};

    use super::*;

    const PASSWORD: &str = "Candidate7secret";

    fn options(port: u16) -> PgConnectOptions {
        PgConnectOptions::new()
            .host("db.internal.example")
            .port(port)
            .username("tenant_role")
            .password(PASSWORD)
            .database("tenant_db")
    }

    #[derive(Debug)]
    struct FakeDbError(Option<&'static str>);

    impl fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("fake driver text for user tenant_role")
        }
    }

    impl StdError for FakeDbError {}

    impl DatabaseError for FakeDbError {
        fn message(&self) -> &'static str {
            "fake driver text for user tenant_role"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            self.0.map(Cow::Borrowed)
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

    fn database(code: Option<&'static str>) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError(code)))
    }

    fn io() -> sqlx::Error {
        sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused by db.internal.example",
        ))
    }

    fn source_text(error: &AppError) -> String {
        StdError::source(error)
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    #[test]
    fn candidates_keep_their_order() {
        let candidates = ConnectCandidates::new(options(1))
            .or(options(2))
            .or(options(3));
        assert_eq!(candidates.len(), 3);
        let ports: Vec<u16> = std::iter::once(&candidates.first)
            .chain(&candidates.rest)
            .map(PgConnectOptions::get_port)
            .collect();
        assert_eq!(ports, [1, 2, 3]);

        let single = ConnectCandidates::from(options(7));
        assert_eq!(single.len(), 1);
        assert_eq!(single.first.get_port(), 7);
    }

    #[test]
    fn debug_shows_only_how_many_candidates() {
        let candidates = ConnectCandidates::new(options(5432)).or(options(5433));
        let debug = format!("{candidates:?}");
        assert_eq!(debug, "ConnectCandidates { count: 2 }");
        assert!(!debug.contains(PASSWORD));
        assert!(!debug.contains("db.internal.example"));
    }

    #[test]
    fn only_a_rejected_password_falls_back() {
        assert!(falls_back(&database(Some("28P01"))));
        assert!(!falls_back(&database(Some("28000"))));
        assert!(!falls_back(&database(Some("3D000"))));
        assert!(!falls_back(&database(None)));
        assert!(!falls_back(&io()));
        assert!(!falls_back(&sqlx::Error::PoolTimedOut));
    }

    #[test]
    fn connect_errors_carry_the_class_and_sqlstate_but_no_driver_text() {
        let rejected = connect_error(&database(Some("28P01")), 2, 2);
        assert_eq!(rejected.code(), ErrorCode::Internal);
        assert_eq!(rejected.message(), "internal error");
        assert_eq!(rejected.reason(), Some(DB_CREDENTIALS_REJECTED));
        assert_eq!(
            source_text(&rejected),
            "connect failed: auth (sqlstate 28P01), 2 of 2 candidates tried"
        );

        let no_login = connect_error(&database(Some("28000")), 1, 2);
        assert_eq!(no_login.reason(), Some(DB_CREDENTIALS_REJECTED));
        assert_eq!(
            source_text(&no_login),
            "connect failed: auth (sqlstate 28000), 1 of 2 candidates tried"
        );

        let missing = connect_error(&database(Some("3D000")), 1, 2);
        assert_eq!(missing.code(), ErrorCode::Internal);
        assert_eq!(missing.reason(), None);
        assert_eq!(
            source_text(&missing),
            "connect failed: other (sqlstate 3D000), 1 of 2 candidates tried"
        );

        let unreachable = connect_error(&io(), 1, 1);
        assert_eq!(unreachable.code(), ErrorCode::Unavailable);
        assert_eq!(unreachable.message(), "database unavailable");
        assert_eq!(unreachable.reason(), None);
        assert_eq!(
            source_text(&unreachable),
            "connect failed: unreachable, 1 of 1 candidates tried"
        );

        let timeout = connect_error(&sqlx::Error::PoolTimedOut, 1, 1);
        assert_eq!(timeout.code(), ErrorCode::Unavailable);
        assert_eq!(
            source_text(&timeout),
            "connect failed: timeout, 1 of 1 candidates tried"
        );

        let bad = connect_error(&sqlx::Error::Configuration("bad".into()), 1, 1);
        assert_eq!(bad.code(), ErrorCode::Internal);
        assert_eq!(bad.reason(), None);

        let odd = connect_error(&database(Some("28P0'")), 1, 1);
        assert_eq!(
            source_text(&odd),
            "connect failed: other, 1 of 1 candidates tried"
        );
        let long = connect_error(&database(Some("28P01X")), 1, 1);
        assert!(!source_text(&long).contains("sqlstate"));

        for error in [rejected, no_login, missing, unreachable, odd] {
            let debug = format!("{error:?}");
            assert!(!debug.contains("fake driver text"), "{debug}");
            assert!(!debug.contains("db.internal.example"), "{debug}");
            assert!(!source_text(&error).contains("tenant_role"));
        }
    }

    fn pools() -> KeyedPoolsBuilder<u32> {
        KeyedPools::builder(
            |port: u32| async move { Ok(options(u16::try_from(port).unwrap_or(1))) },
        )
    }

    #[tokio::test]
    async fn build_refuses_zero_settings_and_names_them() {
        let refused: [(KeyedPoolsBuilder<u32>, &str); 8] = [
            (pools().capacity(0), "capacity"),
            (
                pools().evict_idle_after(Some(Duration::ZERO)),
                "evict_idle_after",
            ),
            (pools().create_timeout(Duration::ZERO), "create_timeout"),
            (pools().reap_interval(Duration::ZERO), "reap_interval"),
            (pools().max_connections(0), "max_connections"),
            (pools().acquire_timeout(Duration::ZERO), "acquire_timeout"),
            (
                pools().connection_idle_timeout(Some(Duration::ZERO)),
                "connection_idle_timeout",
            ),
            (
                pools().connection_max_lifetime(Some(Duration::ZERO)),
                "connection_max_lifetime",
            ),
        ];
        for (builder, setting) in refused {
            let error = builder.build().unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{setting}");
            assert!(error.message().contains(setting), "{}", error.message());
        }

        let built = pools()
            .evict_idle_after(None)
            .connection_idle_timeout(None)
            .connection_max_lifetime(None)
            .init(|_, _| async { Ok(()) })
            .build()
            .unwrap();
        assert!(!built.is_closed());
        assert_eq!(built.stats().capacity, 64);
    }

    #[tokio::test]
    async fn debug_shows_settings_and_stats_only() {
        let builder = pools().build_value(|port, pool| async move { Ok((port, pool)) });
        let debug = format!("{builder:?}");
        assert!(debug.contains("max_connections: 5"), "{debug}");
        assert!(debug.contains("acquire_timeout: 5s"), "{debug}");
        assert!(debug.contains("evict_idle_after: Some(600s)"), "{debug}");
        assert!(!debug.contains(PASSWORD));

        let pools = builder.build().unwrap();
        let debug = format!("{:?}", pools.clone());
        assert!(
            debug.starts_with("KeyedPools { stats: KeyedStats {"),
            "{debug}"
        );
        assert!(!debug.contains(PASSWORD));
        assert!(pools.peek(&1).is_none());
        assert_eq!(pools.evict_idle().await, 0);
        assert_eq!(pools.evict_idle_older_than(Duration::ZERO).await, 0);
        assert!(!pools.invalidate(&1).await);
        pools.close().await;
        assert!(pools.is_closed());
        assert!(pools.get(&1).await.is_err());
    }

    #[tokio::test]
    async fn the_cache_settings_reach_the_cache_as_one_value() {
        let defaults = pools().build().unwrap();
        assert_eq!(defaults.cache.inner.settings, CACHE_DEFAULT);
        assert_eq!(
            defaults.cache.inner.settings.evict_idle_after,
            Some(Duration::from_mins(10))
        );
        assert_eq!(
            defaults.cache.inner.settings.capacity,
            Settings::DEFAULT.capacity
        );

        let custom = pools()
            .capacity(3)
            .evict_idle_after(Some(Duration::from_secs(5)))
            .create_timeout(Duration::from_secs(7))
            .reap_interval(Duration::from_secs(9))
            .build()
            .unwrap();
        assert_eq!(
            custom.cache.inner.settings,
            Settings {
                capacity: 3,
                evict_idle_after: Some(Duration::from_secs(5)),
                create_timeout: Duration::from_secs(7),
                reap_interval: Duration::from_secs(9),
            }
        );
    }

    fn lazy_pool() -> PgPool {
        PoolSettings::DEFAULT
            .options()
            .connect_lazy_with(options(1))
    }

    #[tokio::test]
    async fn an_armed_guard_closes_its_pool_on_a_tracked_task() {
        let tracker = Arc::new(Tracker::default());
        let pool = lazy_pool();
        let held = pool.clone();
        drop(PoolGuard::new(pool, Arc::clone(&tracker)));
        held.close_event().await;
        tracker.wait_idle().await;
        assert!(held.is_closed());
        assert_eq!(tracker.retiring(), 0);
    }

    #[tokio::test]
    async fn a_disarmed_guard_leaves_its_pool_open() {
        let tracker = Arc::new(Tracker::default());
        let pool = PoolGuard::new(lazy_pool(), Arc::clone(&tracker)).disarm();
        tracker.wait_idle().await;
        assert!(!pool.is_closed());
        assert_eq!(tracker.retiring(), 0);
        close_pool(pool.clone()).await;
        assert!(pool.is_closed());
    }
}
