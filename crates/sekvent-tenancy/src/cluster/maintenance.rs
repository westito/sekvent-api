//! The cluster's maintenance connection: a role that is not a superuser,
//! one session at a time.
//!
//! The maintenance role has `LOGIN`, `CREATEDB` and `CREATEROLE` and holds
//! every tenant role `WITH INHERIT FALSE, SET TRUE`: it can switch to a
//! tenant role and has none of its privileges until it does. Postgres 16 or
//! later is required.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use sekvent_db::to_app_error;
use sekvent_error::AppError;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, PgConnection, Postgres, Transaction};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::error::Elapsed;
use tokio::time::timeout;

use crate::{DatabaseName, ObjectNames, TenantRole};

/// The client-side bound of one public operation of `roles`, `databases` or
/// `handover`. A handover takes one deadline this long for all its steps.
pub(crate) const OPERATION_TIMEOUT: Duration = Duration::from_secs(150);

/// The longest wait for the one maintenance permit.
const PERMIT_WAIT: Duration = Duration::from_secs(30);
/// The bound of connecting, authenticating and setting up a session.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The bound of closing a session or the pool.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// The bound of acquiring the connection of a [`MaintenancePool`].
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// The application name of every maintenance session.
const APPLICATION_NAME: &str = "sekvent-tenancy-maintenance";
/// The server-side `lock_timeout` of every maintenance session.
const LOCK_TIMEOUT: &str = "10s";
/// The server-side `statement_timeout` of every maintenance session.
const STATEMENT_TIMEOUT: &str = "120s";
/// No object of the connected database stands in for a catalog operator or
/// function.
const SAFE_SEARCH_PATH: &str = "pg_catalog,pg_temp";

/// `UNAVAILABLE` past [`PERMIT_WAIT`].
const BUSY: &str = "the maintenance connection is busy";
/// `UNAVAILABLE` past [`CONNECT_TIMEOUT`] or [`CLOSE_TIMEOUT`].
const NO_ANSWER: &str = "the maintenance database did not answer in time";
/// `UNAVAILABLE` past [`OPERATION_TIMEOUT`].
const OUT_OF_TIME: &str = "the maintenance operation did not finish in time";

/// The source of the refusal of a poisoned connection.
const POISONED: &str = "the maintenance connection is poisoned and can only be closed";
/// The source of the refusal of a connection whose session is gone.
const DETACHED: &str = "the maintenance connection has no session";

/// The options of a maintenance session: the product's server settings with
/// the session rules applied (the safe `search_path`, the server-side
/// bounds, the application name, no statement logging).
fn session_options(base: &PgConnectOptions) -> PgConnectOptions {
    base.clone()
        .application_name(APPLICATION_NAME)
        .options([
            ("search_path", SAFE_SEARCH_PATH),
            ("lock_timeout", LOCK_TIMEOUT),
            ("statement_timeout", STATEMENT_TIMEOUT),
        ])
        .disable_statement_logging()
}

/// The options of a [`MaintenancePool`] session on `database`: the server
/// bounds and read-only transactions, the database's own `search_path`.
fn pool_options(base: &PgConnectOptions, database: &DatabaseName) -> PgConnectOptions {
    base.clone()
        .database(database.as_str())
        .application_name(APPLICATION_NAME)
        .options([
            ("lock_timeout", LOCK_TIMEOUT),
            ("statement_timeout", STATEMENT_TIMEOUT),
            ("default_transaction_read_only", "on"),
        ])
        .disable_statement_logging()
}

/// The cluster's maintenance connection: one at a time. Clones share the
/// permit. `Debug` prints the type name only.
#[derive(Clone)]
pub struct Maintenance {
    /// The maintenance role's options as the product gave them; the session
    /// rules are applied on every connect.
    options: PgConnectOptions,
    /// The object names every operation's role and database must belong to.
    names: ObjectNames,
    /// One permit: at most one maintenance session or pool at a time.
    permit: Arc<Semaphore>,
}

impl Maintenance {
    /// The maintenance connection of `options`, for the tenants of `names`.
    pub fn new(options: PgConnectOptions, names: ObjectNames) -> Self {
        Self {
            options,
            names,
            permit: Arc::new(Semaphore::new(1)),
        }
    }

    /// The maintenance role (the options' user).
    pub fn role(&self) -> &str {
        self.options.get_username()
    }

    /// The maintenance database (the options' database, else the role's
    /// name).
    pub fn database(&self) -> &str {
        self.options
            .get_database()
            .unwrap_or_else(|| self.options.get_username())
    }

    /// The object names of the tenants this connection maintains.
    pub fn names(&self) -> &ObjectNames {
        &self.names
    }

    /// A session on the maintenance database.
    ///
    /// Waits at most 30 s for the permit and 10 s for the session
    /// (`UNAVAILABLE` past either).
    pub async fn connect(&self) -> Result<MaintenanceConn, AppError> {
        self.open(session_options(&self.options)).await
    }

    /// Connected to `database` instead (the handover only).
    pub async fn connect_to(&self, database: &DatabaseName) -> Result<MaintenanceConn, AppError> {
        self.open(session_options(&self.options).database(database.as_str()))
            .await
    }

    /// A pool of one connection on `database` that holds the permit, for a
    /// product's read-only check before a handover. Its sessions start
    /// read-only and keep the database's own `search_path`.
    ///
    /// The pool is lazy: nothing connects before its first use, and the
    /// permit is held from here until [`MaintenancePool::close`].
    pub async fn pool_on(&self, database: &DatabaseName) -> Result<MaintenancePool, AppError> {
        let permit = self.permit().await?;
        let pool = PgPoolOptions::new()
            .min_connections(0)
            .max_connections(1)
            .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
            .connect_lazy_with(pool_options(&self.options, database));
        Ok(MaintenancePool {
            pool,
            _permit: permit,
        })
    }

    /// The permit, waited for at most [`PERMIT_WAIT`].
    async fn permit(&self) -> Result<OwnedSemaphorePermit, AppError> {
        match timeout(PERMIT_WAIT, Arc::clone(&self.permit).acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(AppError::internal("the maintenance permit was closed")),
            Err(_) => Err(AppError::unavailable(BUSY)),
        }
    }

    /// A session with `options`, holding the permit.
    async fn open(&self, options: PgConnectOptions) -> Result<MaintenanceConn, AppError> {
        let permit = self.permit().await?;
        let conn = match timeout(CONNECT_TIMEOUT, PgConnection::connect_with(&options)).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(error)) => return Err(to_app_error(error)),
            Err(_) => return Err(AppError::unavailable(NO_ANSWER)),
        };
        Ok(MaintenanceConn {
            conn: Some(conn),
            role_switched: false,
            broken: false,
            in_flight: false,
            names: self.names.clone(),
            role: self.role().to_owned(),
            _permit: permit,
        })
    }
}

impl fmt::Debug for Maintenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Maintenance(..)")
    }
}

/// One maintenance session. `Debug` prints the type name only.
pub struct MaintenanceConn {
    /// The session; `None` once a timed-out operation dropped it (and on a
    /// connection made by the test constructor).
    conn: Option<PgConnection>,
    /// A session-level `SET ROLE` ran and its `RESET ROLE` has not succeeded.
    role_switched: bool,
    /// Poisoned for good: an operation ran out of time, or did not run to
    /// its end (its future was dropped, or its transaction could not be
    /// ended), so the session's transaction and role state are unknown.
    broken: bool,
    /// An operation started and has not come back: set by
    /// [`MaintenanceConn::live`], cleared by [`MaintenanceConn::bounded`].
    /// It stays set when the operation's future is dropped before it ends.
    in_flight: bool,
    /// The object names of the [`Maintenance`] it came from.
    names: ObjectNames,
    /// The maintenance role (the session user).
    role: String,
    /// The maintenance permit, returned when the session is dropped.
    _permit: OwnedSemaphorePermit,
}

impl MaintenanceConn {
    /// Whether the session may run no further operation and can only be
    /// closed.
    pub fn is_poisoned(&self) -> bool {
        self.role_switched || self.broken || self.in_flight
    }

    /// Closes the session and returns the permit, poisoned or not.
    ///
    /// A connection whose session a timeout already dropped closes `Ok`. A
    /// close that takes longer than 10 s drops the session and is
    /// `UNAVAILABLE`; the permit returns all the same.
    pub async fn close(mut self) -> Result<(), AppError> {
        let Some(conn) = self.conn.take() else {
            return Ok(());
        };
        match timeout(CLOSE_TIMEOUT, conn.close()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(to_app_error(error)),
            Err(_) => Err(AppError::unavailable(NO_ANSWER)),
        }
    }

    /// The session for one operation, or `INTERNAL` when it is poisoned or
    /// was dropped. The operation is in flight from here until
    /// [`MaintenanceConn::bounded`] receives its outcome: an operation whose
    /// future is dropped before that leaves the connection poisoned.
    pub(crate) fn live(&mut self) -> Result<&mut PgConnection, AppError> {
        self.refuse_poisoned()?;
        if self.conn.is_none() {
            return Err(AppError::internal(DETACHED));
        }
        self.start();
        self.session()
    }

    /// `INTERNAL` when the connection is poisoned. An operation still in
    /// flight here never came back, so its poison becomes permanent.
    fn refuse_poisoned(&mut self) -> Result<(), AppError> {
        if self.in_flight {
            self.broken = true;
        }
        if self.is_poisoned() {
            return Err(AppError::internal(POISONED));
        }
        Ok(())
    }

    /// Marks an operation in flight.
    fn start(&mut self) {
        self.in_flight = true;
    }

    /// The session whether poisoned or not, for the statements between a
    /// session-level `SET ROLE` and its `RESET ROLE`. `INTERNAL` when it was
    /// dropped.
    pub(super) fn session(&mut self) -> Result<&mut PgConnection, AppError> {
        self.conn
            .as_mut()
            .ok_or_else(|| AppError::internal(DETACHED))
    }

    /// The result of an operation run under a timeout, which ends the
    /// operation. An elapsed outcome drops the session, poisons the
    /// connection for good and is `UNAVAILABLE`. A failure that left the
    /// session in an unknown state poisons it for good and returns the
    /// operation's own error.
    pub(crate) fn bounded<T, E: Into<Failure>>(
        &mut self,
        outcome: Result<Result<T, E>, Elapsed>,
    ) -> Result<T, AppError> {
        self.in_flight = false;
        let Ok(result) = outcome else {
            self.conn = None;
            self.broken = true;
            return Err(AppError::unavailable(OUT_OF_TIME));
        };
        result.map_err(|failure| {
            let Failure { error, settled } = failure.into();
            if !settled {
                self.broken = true;
            }
            error
        })
    }

    /// The object names of the [`Maintenance`] this session came from.
    pub(crate) fn names(&self) -> &ObjectNames {
        &self.names
    }

    /// The maintenance role, the session user.
    pub(crate) fn maintenance_role(&self) -> &str {
        &self.role
    }

    /// Before a session-level `SET ROLE`: poisoned until
    /// [`MaintenanceConn::mark_role_reset`].
    pub(crate) fn mark_role_switched(&mut self) {
        self.role_switched = true;
    }

    /// After a `RESET ROLE` succeeded. A timeout's poison stays.
    pub(crate) fn mark_role_reset(&mut self) {
        self.role_switched = false;
    }

    /// A connection without a session, for unit tests of the poisoning.
    #[cfg(test)]
    pub(crate) fn detached(names: ObjectNames, role: &str) -> Self {
        let permit = Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("a fresh semaphore has its permit");
        Self {
            conn: None,
            role_switched: false,
            broken: false,
            in_flight: false,
            names,
            role: role.to_owned(),
            _permit: permit,
        }
    }
}

impl fmt::Debug for MaintenanceConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MaintenanceConn(..)")
    }
}

/// A pool of one maintenance connection that holds the permit. `Debug`
/// prints the type name only.
pub struct MaintenancePool {
    /// Lazy, at most one connection, read-only sessions.
    pool: PgPool,
    /// The maintenance permit, held from creation until the pool is closed.
    _permit: OwnedSemaphorePermit,
}

impl MaintenancePool {
    /// The pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Closes the pool and returns the permit. A close that takes longer
    /// than 10 s is abandoned; the permit returns all the same.
    pub async fn close(self) {
        // Past the bound the pool's connection is dropped with the pool.
        let _ = timeout(CLOSE_TIMEOUT, self.pool.close()).await;
    }
}

impl fmt::Debug for MaintenancePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MaintenancePool(..)")
    }
}

/// An operation's error, and whether the session is in a known state after
/// it. Every [`AppError`] converts into a settled failure.
pub(crate) struct Failure {
    /// The operation's answer.
    error: AppError,
    /// The session is idle and outside any transaction.
    settled: bool,
}

impl Failure {
    /// `error`, after which the session's transaction state is unknown: the
    /// connection is poisoned for good.
    pub(crate) fn unsettled(error: AppError) -> Self {
        Self {
            error,
            settled: false,
        }
    }
}

impl From<AppError> for Failure {
    fn from(error: AppError) -> Self {
        Self {
            error,
            settled: true,
        }
    }
}

/// Opens a transaction on `conn`. A failed `BEGIN` leaves the session's
/// transaction state unknown.
pub(crate) async fn begin(conn: &mut PgConnection) -> Result<Transaction<'_, Postgres>, Failure> {
    conn.begin()
        .await
        .map_err(|error| Failure::unsettled(to_app_error(error)))
}

/// Ends `tx` with `result`: commits a success, rolls back on an error. The
/// rollback is awaited, so the session is idle again before the error
/// returns; the error is the answer whether or not the rollback succeeds. A
/// failed commit or rollback leaves the session's state unknown.
pub(crate) async fn end_transaction<T>(
    tx: Transaction<'_, Postgres>,
    result: Result<T, AppError>,
) -> Result<T, Failure> {
    match result {
        Ok(value) => match tx.commit().await {
            Ok(()) => Ok(value),
            Err(error) => Err(Failure::unsettled(to_app_error(error))),
        },
        Err(error) => Err(match roll_back(tx).await {
            Ok(()) => Failure::from(error),
            Err(_) => Failure::unsettled(error),
        }),
    }
}

/// Rolls `tx` back and waits for it. A failed rollback leaves the
/// session's state unknown.
pub(crate) async fn roll_back(tx: Transaction<'_, Postgres>) -> Result<(), Failure> {
    tx.rollback()
        .await
        .map_err(|error| Failure::unsettled(to_app_error(error)))
}

/// `INTERNAL` (no reason) unless `role` is an object name of `names`: a
/// maintenance connection touches only its own product's tenant roles.
pub(crate) fn own_role(names: &ObjectNames, role: &TenantRole) -> Result<(), AppError> {
    if names.is_object_name(role.as_str()) {
        Ok(())
    } else {
        Err(AppError::internal(
            "the role is not an object name of this maintenance connection",
        ))
    }
}

/// `INTERNAL` (no reason) unless `database` is an object name of `names`,
/// for the operations that only ever see a generated database name.
pub(crate) fn own_database(names: &ObjectNames, database: &DatabaseName) -> Result<(), AppError> {
    if names.is_object_name(database.as_str()) {
        Ok(())
    } else {
        Err(AppError::internal(
            "the database is not an object name of this maintenance connection",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::future;

    use sekvent_error::ErrorCode;
    use tokio::time::Instant;
    use uuid::Uuid;

    use super::*;
    use crate::TenantId;

    const TENANT: Uuid = Uuid::from_u128(0x0b9c_2f4e_51a8_4d3b_9e6f_7c1d_2a3b_4c5d);

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("the prefix is valid")
    }

    /// Options of a server nobody listens on, with credentials that must
    /// never be printed.
    fn options() -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(1)
            .username("orders_maintenance")
            .password("correct-horse-battery-staple")
            .database("orders_upkeep")
    }

    fn maintenance() -> Maintenance {
        Maintenance::new(options(), names())
    }

    fn source(error: &AppError) -> String {
        error.source().map(ToString::to_string).unwrap_or_default()
    }

    async fn elapsed() -> Elapsed {
        timeout(Duration::ZERO, future::pending::<()>())
            .await
            .expect_err("a zero timeout over a pending future elapses")
    }

    #[test]
    fn the_role_and_database_come_from_the_options() {
        let maintenance = maintenance();
        assert_eq!(maintenance.role(), "orders_maintenance");
        assert_eq!(maintenance.database(), "orders_upkeep");
        assert_eq!(maintenance.names(), &names());
        let without_database = Maintenance::new(
            PgConnectOptions::new_without_pgpass().username("billing_upkeep"),
            names(),
        );
        assert_eq!(without_database.database(), "billing_upkeep");
    }

    #[tokio::test]
    async fn nothing_secret_is_printed() {
        let maintenance = maintenance();
        let conn = MaintenanceConn::detached(names(), "orders_maintenance");
        let database = DatabaseName::parse("orders").expect("a valid name");
        let pool = maintenance.pool_on(&database).await.expect("a lazy pool");
        for printed in [
            format!("{maintenance:?}"),
            format!("{conn:?}"),
            format!("{pool:?}"),
        ] {
            assert!(!printed.contains("correct-horse"), "{printed}");
            assert!(!printed.contains("orders"), "{printed}");
            assert!(!printed.contains("127.0.0.1"), "{printed}");
        }
        assert_eq!(format!("{maintenance:?}"), "Maintenance(..)");
        assert_eq!(format!("{conn:?}"), "MaintenanceConn(..)");
        assert_eq!(format!("{pool:?}"), "MaintenancePool(..)");
        pool.close().await;
    }

    #[test]
    fn a_session_gets_the_session_rules() {
        let base = options().options([("work_mem", "4MB")]);
        let session = session_options(&base);
        assert_eq!(session.get_host(), "127.0.0.1");
        assert_eq!(session.get_port(), 1);
        assert_eq!(session.get_username(), "orders_maintenance");
        assert_eq!(session.get_database(), Some("orders_upkeep"));
        assert_eq!(session.get_application_name(), Some(APPLICATION_NAME));
        assert_eq!(
            session.get_options(),
            Some(
                "-c work_mem=4MB -c search_path=pg_catalog,pg_temp -c lock_timeout=10s \
                 -c statement_timeout=120s"
            )
        );
    }

    #[test]
    fn a_pool_session_reads_only_and_keeps_the_search_path() {
        let database = DatabaseName::parse("orders_candidate").expect("a valid name");
        let pool = pool_options(&options(), &database);
        assert_eq!(pool.get_database(), Some("orders_candidate"));
        assert_eq!(pool.get_username(), "orders_maintenance");
        assert_eq!(pool.get_application_name(), Some(APPLICATION_NAME));
        assert_eq!(
            pool.get_options(),
            Some(
                "-c lock_timeout=10s -c statement_timeout=120s \
                 -c default_transaction_read_only=on"
            )
        );
    }

    #[tokio::test]
    async fn a_pool_holds_the_permit_until_closed() {
        let maintenance = maintenance();
        let database = DatabaseName::parse("orders_candidate").expect("a valid name");
        let pool = maintenance.pool_on(&database).await.expect("a lazy pool");
        assert_eq!(maintenance.permit.available_permits(), 0);
        assert_eq!(pool.pool().options().get_max_connections(), 1);
        assert_eq!(pool.pool().options().get_min_connections(), 0);
        assert_eq!(
            pool.pool().options().get_acquire_timeout(),
            POOL_ACQUIRE_TIMEOUT
        );
        assert_eq!(
            pool.pool().connect_options().get_database(),
            Some("orders_candidate")
        );
        pool.close().await;
        assert_eq!(maintenance.permit.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_permit_is_busy_after_thirty_seconds() {
        let maintenance = maintenance();
        let held = Arc::clone(&maintenance.permit)
            .try_acquire_owned()
            .expect("the permit is free");
        let started = Instant::now();
        let error = maintenance.connect().await.expect_err("the permit is held");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), BUSY);
        assert!(started.elapsed() >= PERMIT_WAIT);

        let database = DatabaseName::parse("orders").expect("a valid name");
        let error = maintenance
            .pool_on(&database)
            .await
            .expect_err("the permit is held");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), BUSY);

        let clone = maintenance.clone();
        let error = clone
            .connect_to(&database)
            .await
            .expect_err("clones share the permit");
        assert_eq!(error.message(), BUSY);
        std::mem::drop(held);
        assert_eq!(maintenance.permit.available_permits(), 1);
    }

    #[tokio::test]
    async fn a_failed_connect_returns_the_permit() {
        let maintenance = maintenance();
        let outcome = timeout(Duration::from_secs(30), maintenance.connect())
            .await
            .expect("the refused connect finished within its 30 s guard");
        let error = outcome.expect_err("nobody listens on port 1");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(!format!("{error:?}").contains("correct-horse"));
        assert_eq!(maintenance.permit.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_elapsed_bound_poisons_for_good() {
        let mut conn = MaintenanceConn::detached(names(), "orders_maintenance");
        assert!(!conn.is_poisoned());
        let outcome: Result<Result<u8, AppError>, Elapsed> = Err(elapsed().await);
        let error = conn.bounded(outcome).expect_err("the bound elapsed");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), OUT_OF_TIME);
        assert!(conn.is_poisoned());
        let error = conn.live().expect_err("a timed-out connection is refused");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(source(&error), POISONED);
        conn.mark_role_reset();
        assert!(
            conn.is_poisoned(),
            "a reset does not lift a timeout's poison"
        );
        conn.close().await.expect("a dropped session closes Ok");
    }

    #[test]
    fn a_finished_outcome_passes_through() {
        let mut conn = MaintenanceConn::detached(names(), "orders_maintenance");
        assert_eq!(
            conn.bounded::<u8, AppError>(Ok(Ok(7_u8)))
                .expect("finished"),
            7
        );
        let error = conn
            .bounded::<u8, AppError>(Ok(Err(AppError::invalid_argument("refused"))))
            .expect_err("the operation's own error");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(!conn.is_poisoned());
    }

    #[test]
    fn a_role_switch_poisons_until_its_reset() {
        let mut conn = MaintenanceConn::detached(names(), "orders_maintenance");
        let error = conn.live().expect_err("the test connection has no session");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(source(&error), DETACHED);

        conn.mark_role_switched();
        assert!(conn.is_poisoned());
        let error = conn.live().expect_err("a switched connection is refused");
        assert_eq!(source(&error), POISONED);
        let error = conn
            .session()
            .expect_err("the poison-free accessor still needs a session");
        assert_eq!(source(&error), DETACHED);

        conn.mark_role_reset();
        assert!(!conn.is_poisoned());
        assert_eq!(conn.names(), &names());
        assert_eq!(conn.maintenance_role(), "orders_maintenance");
    }

    /// One operation the way `roles`, `databases` and `handover` run
    /// theirs, with `body` for its statements; [`MaintenanceConn::start`]
    /// stands in for the session [`MaintenanceConn::live`] would hand out.
    async fn operation(
        conn: &mut MaintenanceConn,
        body: impl Future<Output = Result<(), Failure>>,
    ) -> Result<(), AppError> {
        let outcome = timeout(OPERATION_TIMEOUT, async {
            conn.refuse_poisoned()?;
            conn.start();
            body.await
        })
        .await;
        conn.bounded(outcome)
    }

    #[tokio::test(start_paused = true)]
    async fn an_operation_dropped_in_flight_poisons_for_good() {
        let mut conn = MaintenanceConn::detached(names(), "orders_maintenance");
        let cancelled = timeout(
            Duration::from_secs(1),
            operation(&mut conn, future::pending()),
        )
        .await;
        assert!(cancelled.is_err(), "the caller gave up first");
        assert!(conn.is_poisoned());
        let error = conn
            .live()
            .expect_err("an interrupted connection is refused");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(source(&error), POISONED);

        let error = operation(&mut conn, future::ready(Ok(())))
            .await
            .expect_err("a later operation is refused before its statements");
        assert_eq!(source(&error), POISONED);
        conn.mark_role_reset();
        assert!(
            conn.is_poisoned(),
            "neither a refused operation nor a reset lifts it"
        );
        conn.close()
            .await
            .expect("a session-less connection closes Ok");
    }

    #[tokio::test(start_paused = true)]
    async fn the_refusal_of_an_operation_dropped_in_flight_stays() {
        let mut conn = MaintenanceConn::detached(names(), "orders_maintenance");
        {
            let pending = operation(&mut conn, future::pending());
            tokio::pin!(pending);
            assert!(futures::poll!(&mut pending).is_pending());
        }
        let error = operation(&mut conn, future::ready(Ok(())))
            .await
            .expect_err("poisoned");
        assert_eq!(source(&error), POISONED);
        let error = operation(&mut conn, future::ready(Ok(())))
            .await
            .expect_err("still poisoned after a refused operation ended");
        assert_eq!(source(&error), POISONED);
    }

    #[tokio::test(start_paused = true)]
    async fn an_operation_that_ends_leaves_the_connection_usable() {
        let mut conn = MaintenanceConn::detached(names(), "orders_maintenance");
        operation(&mut conn, future::ready(Ok(())))
            .await
            .expect("finished");
        assert!(!conn.is_poisoned());

        let refused = Failure::from(AppError::internal("the role has drift"));
        let error = operation(&mut conn, future::ready(Err(refused)))
            .await
            .expect_err("the operation's own refusal");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(source(&error), "the role has drift");
        assert!(
            !conn.is_poisoned(),
            "a refusal whose transaction rolled back is settled"
        );
        operation(&mut conn, future::ready(Ok(())))
            .await
            .expect("the connection still runs operations");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unsettled_failure_poisons_for_good_and_keeps_its_error() {
        let mut conn = MaintenanceConn::detached(names(), "orders_maintenance");
        let failure = Failure::unsettled(AppError::invalid_argument("refused"));
        let error = operation(&mut conn, future::ready(Err(failure)))
            .await
            .expect_err("the operation's own error");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.message(), "refused");
        assert!(conn.is_poisoned());
        let error = conn.live().expect_err("poisoned");
        assert_eq!(source(&error), POISONED);
    }

    #[test]
    fn only_own_object_names_pass() {
        let tenant = TenantId::from_uuid(TENANT);
        let names = names();
        own_role(&names, &names.role(tenant)).expect("an own role");
        own_database(&names, &names.database(tenant)).expect("an own database");

        let other = ObjectNames::new("billing_t_").expect("the prefix is valid");
        let error = own_role(&names, &other.role(tenant)).expect_err("another prefix");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        let error = own_database(&names, &other.database(tenant)).expect_err("another prefix");
        assert_eq!(error.code(), ErrorCode::Internal);
        let adopted = DatabaseName::parse("orders").expect("a valid name");
        let error = own_database(&names, &adopted).expect_err("not generated");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
    }
}
