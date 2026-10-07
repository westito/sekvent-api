use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use sekvent_error::ErrorCode;
use sekvent_runtime::{DependencyProbe, ProbeFailure, ProbeStatus};
use tokio::time::Instant;

use crate::error::sqlx_impl::{Access, access};
use crate::{Pool, classify};

/// How long a successful check vouches for a saturated pool. Past it, the
/// probe checks again even when every connection is in use: during an
/// outage the slots that are still connecting count as in use too.
const FRESH_UP: Duration = Duration::from_secs(30);

/// How long one check may take to get a connection and ping it. Below the
/// runtime's default `probe_timeout` (2 s), so the probe records its own
/// `Down` before the runtime gives up on it.
const CHECK_TIMEOUT: Duration = Duration::from_millis(1500);

/// Detail of a probe on a closed pool.
const CLOSED: &str = "pool closed";

/// Detail of a check that exceeded [`CHECK_TIMEOUT`].
const NO_ANSWER: &str = "no answer in time";

/// Readiness probe of one pool: acquire a connection and ping it.
///
/// A lazy pool is probed like any other: `lazy` only means "no connection
/// at startup". When every connection of the pool is in use, the probe
/// reports `Up` without queueing behind the load if its last successful
/// check is less than 30 s old, so readiness does not flap while the
/// service is busy; otherwise it checks as usual. A check gets at most
/// 1.5 s, below the runtime's default `probe_timeout`.
///
/// | failure | status |
/// |---|---|
/// | the pool was closed | `Down(Unreachable("pool closed"))` |
/// | the server rejected the credentials | `Down(Rejected("credentials rejected"))` |
/// | a missing grant, including MySQL's "access denied to the database" | `Down(Rejected("access denied"))` |
/// | the pool's acquire timeout passed | `Down(Unreachable("no connection available in time"))` |
/// | no connection and ping within 1.5 s | `Down(Unreachable("no answer in time"))` |
/// | I/O, TLS, a connection-class SQLSTATE | `Down(Unreachable("unreachable"))` |
/// | anything else | `Down(Unreachable("ping failed"))` |
#[derive(Debug, Clone)]
pub struct PoolProbe {
    name: String,
    pool: Pool,
    required: bool,
    up_at: Arc<Mutex<Option<Instant>>>,
}

impl PoolProbe {
    /// A required probe named `name`.
    pub fn new(name: impl Into<String>, pool: Pool) -> Self {
        Self {
            name: name.into(),
            pool,
            required: true,
            up_at: Arc::new(Mutex::new(None)),
        }
    }

    /// Report it without gating readiness.
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    async fn check(&self) -> ProbeStatus {
        self.decide(
            is_closed(&self.pool),
            saturated(&self.pool),
            ping(&self.pool),
        )
        .await
    }

    /// One check, given the pool's state and the ping to run when the
    /// state does not decide it.
    async fn decide(
        &self,
        closed: bool,
        saturated: bool,
        ping: impl Future<Output = Result<(), sqlx::Error>>,
    ) -> ProbeStatus {
        if closed {
            let status = ProbeStatus::Down(ProbeFailure::Unreachable(CLOSED));
            self.record(status);
            return status;
        }
        if saturated && self.fresh_up() {
            return ProbeStatus::Up;
        }
        let status = match tokio::time::timeout(CHECK_TIMEOUT, ping).await {
            Ok(Ok(())) => ProbeStatus::Up,
            Ok(Err(error)) => ProbeStatus::Down(failure(&error)),
            Err(_elapsed) => ProbeStatus::Down(ProbeFailure::Unreachable(NO_ANSWER)),
        };
        if let ProbeStatus::Down(failure) = status {
            tracing::debug!(
                pool = %self.name,
                kind = failure.kind(),
                detail = failure.detail(),
                "database probe failed"
            );
        }
        self.record(status);
        status
    }

    /// Whether the last successful check is recent enough to stand in for
    /// a new one.
    fn fresh_up(&self) -> bool {
        let up_at = *self.up_at.lock().unwrap_or_else(PoisonError::into_inner);
        up_at.is_some_and(|at| Instant::now().saturating_duration_since(at) < FRESH_UP)
    }

    fn record(&self, status: ProbeStatus) {
        *self.up_at.lock().unwrap_or_else(PoisonError::into_inner) =
            status.is_up().then(Instant::now);
    }
}

impl DependencyProbe for PoolProbe {
    fn name(&self) -> &str {
        &self.name
    }

    fn required(&self) -> bool {
        self.required
    }

    fn probe(&self) -> BoxFuture<'_, ProbeStatus> {
        Box::pin(self.check())
    }
}

fn is_closed(pool: &Pool) -> bool {
    match pool {
        #[cfg(feature = "sqlx-postgres")]
        Pool::Postgres(pool) => pool.is_closed(),
        #[cfg(feature = "sqlx-mysql")]
        Pool::MySql(pool) => pool.is_closed(),
    }
}

/// Whether every connection the pool may open is open (or opening) and in
/// use.
fn saturated(pool: &Pool) -> bool {
    match pool {
        #[cfg(feature = "sqlx-postgres")]
        Pool::Postgres(pool) => {
            pool.size() >= pool.options().get_max_connections() && pool.num_idle() == 0
        }
        #[cfg(feature = "sqlx-mysql")]
        Pool::MySql(pool) => {
            pool.size() >= pool.options().get_max_connections() && pool.num_idle() == 0
        }
    }
}

async fn ping(pool: &Pool) -> Result<(), sqlx::Error> {
    use sqlx::Connection as _;
    match pool {
        #[cfg(feature = "sqlx-postgres")]
        Pool::Postgres(pool) => pool.acquire().await?.ping().await,
        #[cfg(feature = "sqlx-mysql")]
        Pool::MySql(pool) => pool.acquire().await?.ping().await,
    }
}

/// The probe failure for a sqlx error; the detail never quotes the driver.
pub(crate) fn failure(error: &sqlx::Error) -> ProbeFailure {
    if let Some(access) = access(error) {
        return ProbeFailure::Rejected(match access {
            Access::CredentialsRejected => "credentials rejected",
            Access::Denied => "access denied",
        });
    }
    if matches!(error, sqlx::Error::PoolTimedOut) {
        return ProbeFailure::Unreachable("no connection available in time");
    }
    match classify(error) {
        ErrorCode::Unavailable => ProbeFailure::Unreachable("unreachable"),
        _ => ProbeFailure::Unreachable("ping failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::error::Error as StdError;
    use std::fmt;
    use std::future::{pending, ready};

    use sqlx::error::{DatabaseError, ErrorKind};

    use super::*;

    #[derive(Debug)]
    struct Coded(&'static str);

    impl fmt::Display for Coded {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("coded error")
        }
    }

    impl StdError for Coded {}

    impl DatabaseError for Coded {
        fn message(&self) -> &'static str {
            "coded error"
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

    fn coded(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(Coded(code)))
    }

    const UNREACHED: &str = "postgres://u@192.0.2.1:1/a";

    fn up() -> std::future::Ready<Result<(), sqlx::Error>> {
        ready(Ok(()))
    }

    fn hangs() -> std::future::Pending<Result<(), sqlx::Error>> {
        pending()
    }

    #[test]
    fn failures_follow_the_classification_table() {
        assert_eq!(
            failure(&coded("28P01")),
            ProbeFailure::Rejected("credentials rejected")
        );
        assert_eq!(
            failure(&coded("28000")),
            ProbeFailure::Rejected("credentials rejected")
        );
        assert_eq!(
            failure(&coded("42501")),
            ProbeFailure::Rejected("access denied")
        );
        assert_eq!(
            failure(&sqlx::Error::PoolTimedOut),
            ProbeFailure::Unreachable("no connection available in time")
        );
        for error in [
            sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
            sqlx::Error::Tls("handshake".into()),
            sqlx::Error::PoolClosed,
            coded("08006"),
        ] {
            assert_eq!(
                failure(&error),
                ProbeFailure::Unreachable("unreachable"),
                "{error:?}"
            );
        }
        assert_eq!(
            failure(&sqlx::Error::Protocol("x".to_owned())),
            ProbeFailure::Unreachable("ping failed")
        );
        assert_eq!(
            failure(&coded("42000")),
            ProbeFailure::Unreachable("ping failed")
        );
    }

    #[cfg(feature = "sqlx-postgres")]
    #[tokio::test]
    async fn names_and_flags_are_reported() {
        let pool = Pool::Postgres(sqlx::PgPool::connect_lazy(UNREACHED).unwrap());
        let probe = PoolProbe::new("orders_db", pool.clone());
        assert_eq!(probe.name(), "orders_db");
        assert!(probe.required());
        assert!(!saturated(&pool), "a lazy pool has no connection yet");
        assert!(!is_closed(&pool));
        let optional = probe.optional();
        assert!(!optional.required());
        assert!(format!("{optional:?}").contains("orders_db"));
        pool.close().await;
    }

    #[cfg(feature = "sqlx-postgres")]
    #[tokio::test]
    async fn a_closed_pool_is_down_even_after_an_up() {
        let pool = Pool::Postgres(sqlx::PgPool::connect_lazy(UNREACHED).unwrap());
        let probe = PoolProbe::new("orders_db", pool.clone());
        assert_eq!(probe.decide(false, false, up()).await, ProbeStatus::Up);
        pool.close().await;
        assert!(is_closed(&pool));
        // Closed and saturated at once: the fresh Up does not stand in.
        assert_eq!(
            probe.decide(true, true, hangs()).await,
            ProbeStatus::Down(ProbeFailure::Unreachable(CLOSED))
        );
        assert!(!probe.fresh_up());
        // The real check reads the pool's state and never pings.
        assert_eq!(
            probe.probe().await,
            ProbeStatus::Down(ProbeFailure::Unreachable(CLOSED))
        );
    }

    #[cfg(feature = "sqlx-postgres")]
    #[tokio::test(start_paused = true)]
    async fn a_saturated_pool_reuses_only_a_fresh_up() {
        let pool = Pool::Postgres(sqlx::PgPool::connect_lazy(UNREACHED).unwrap());
        let probe = PoolProbe::new("orders_db", pool.clone());

        // No previous result: a saturated pool is checked like any other.
        assert_eq!(
            probe
                .decide(false, true, ready(Err(sqlx::Error::PoolTimedOut)))
                .await,
            ProbeStatus::Down(ProbeFailure::Unreachable("no connection available in time"))
        );
        assert!(!probe.fresh_up());

        assert_eq!(probe.decide(false, false, up()).await, ProbeStatus::Up);
        tokio::time::advance(FRESH_UP.checked_sub(Duration::from_millis(1)).unwrap()).await;
        let start = Instant::now();
        // A fresh Up stands in at once; the hanging ping is never awaited.
        assert_eq!(probe.decide(false, true, hangs()).await, ProbeStatus::Up);
        assert_eq!(start.elapsed(), Duration::ZERO);

        // Not saturated: checked even while the Up is fresh.
        assert_eq!(
            probe.decide(false, false, hangs()).await,
            ProbeStatus::Down(ProbeFailure::Unreachable(NO_ANSWER))
        );
        assert_eq!(start.elapsed(), CHECK_TIMEOUT);
        assert!(!probe.fresh_up(), "a Down is never reused");
        pool.close().await;
    }

    #[cfg(feature = "sqlx-postgres")]
    #[tokio::test(start_paused = true)]
    async fn a_stale_up_is_checked_again_within_the_check_timeout() {
        let pool = Pool::Postgres(sqlx::PgPool::connect_lazy(UNREACHED).unwrap());
        let probe = PoolProbe::new("orders_db", pool.clone());
        assert_eq!(probe.decide(false, false, up()).await, ProbeStatus::Up);
        tokio::time::advance(FRESH_UP).await;
        assert!(!probe.fresh_up());

        let start = Instant::now();
        assert_eq!(
            probe.decide(false, true, hangs()).await,
            ProbeStatus::Down(ProbeFailure::Unreachable(NO_ANSWER))
        );
        assert_eq!(start.elapsed(), CHECK_TIMEOUT);
        // The Down is recorded, so the next saturated round checks again.
        assert_eq!(
            probe.decide(false, true, hangs()).await,
            ProbeStatus::Down(ProbeFailure::Unreachable(NO_ANSWER))
        );
        assert_eq!(start.elapsed(), CHECK_TIMEOUT * 2);
        // A later success is reused again.
        assert_eq!(probe.decide(false, true, up()).await, ProbeStatus::Up);
        assert_eq!(probe.decide(false, true, hangs()).await, ProbeStatus::Up);
        pool.close().await;
    }

    #[cfg(feature = "sqlx-mysql")]
    #[tokio::test]
    async fn a_lazy_mysql_pool_is_not_saturated() {
        let pool = Pool::MySql(sqlx::MySqlPool::connect_lazy("mysql://u@192.0.2.1:1/a").unwrap());
        assert!(!saturated(&pool));
        assert!(!is_closed(&pool));
        pool.close().await;
        assert!(is_closed(&pool));
    }
}
