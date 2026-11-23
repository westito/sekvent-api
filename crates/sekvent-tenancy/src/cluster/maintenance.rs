//! The cluster's maintenance connection: a role that is not a superuser,
//! one session at a time.
//!
//! The maintenance role has `LOGIN`, `CREATEDB` and `CREATEROLE` and holds
//! every tenant role `WITH INHERIT FALSE, SET TRUE`: it can switch to a
//! tenant role and has none of its privileges until it does. Postgres 16 or
//! later is required.
//!
//! Every session asks for its rules as startup options and is confirmed
//! once when it opens, before anything else runs on it, by one statement
//! that reads them back. Every connection must be on the database it asked
//! for (the options' database, else the one named like the user) and
//! signed in as the options' user, and act as that `session_user` (its
//! `CURRENT_USER` is its session user). A session of
//! [`Maintenance::connect`] and [`Maintenance::connect_to`] must also hold
//! the `search_path` `pg_catalog, pg_temp`, a `lock_timeout` of 10 s, a
//! `statement_timeout` of 120 s and `client_min_messages` `warning`. A
//! connection of a [`MaintenancePool`] must hold the same bounds and
//! `client_min_messages`, and both `transaction_read_only` and
//! `default_transaction_read_only` `on`; it keeps the database's own
//! `search_path`. The read-only rule is confirmed when the connection
//! opens, not enforced for later statements: a statement of the pool's
//! user may still `SET` it off. The statement that reads them back
//! qualifies every function and type and compares nothing in SQL, so it
//! calls no object of the database whatever the `search_path` is; the
//! values are compared in Rust.
//!
//! The operations name the maintenance role by the session user and grant
//! to `CURRENT_USER`, and a role setting at startup (`ALTER ROLE … [IN
//! DATABASE …] SET role`, `ALTER DATABASE … SET role`, a `role` startup
//! option) would make the two differ. Something between the crate and the
//! server that drops startup options would leave a session the database's
//! own `search_path` and no server-side bounds. The startup preflight sees
//! only the maintenance database at startup; a session of the handover
//! opens on another database, and any session may open after the settings
//! changed.
//!
//! A refused session is dropped without waiting for a close, so a stalled
//! close cannot outlast the 10 s connect bound, and its connect is
//! `INTERNAL` without a reason, what it held in the error's source (each
//! shown value cut and its control characters escaped). A refused pool
//! connection is closed and the refusal logged by the driver, which tries
//! again until the pool's 10 s acquire bound; the acquire then fails with
//! the driver's timeout, which names nothing.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use sekvent_db::to_app_error;
use sekvent_error::AppError;
use sekvent_telemetry::truncate_for_log;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, PgConnection, Postgres, Transaction};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::error::Elapsed;
use tokio::time::timeout;

use crate::cluster::preflight;
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

/// The default application name of every maintenance session and pool.
const APPLICATION_NAME: &str = "sekvent-tenancy-maintenance";
/// The longest `application_name` Postgres keeps, in bytes (`NAMEDATALEN`
/// less one).
const APPLICATION_NAME_MAX: usize = 63;
/// The server-side `lock_timeout` of every maintenance session.
const LOCK_TIMEOUT: &str = "10s";
/// The server-side `statement_timeout` of every maintenance session.
const STATEMENT_TIMEOUT: &str = "120s";
/// The `client_min_messages` of every maintenance session: a role or
/// database setting of `log` together with statement logging would send a
/// statement (a `CREATE ROLE … PASSWORD` among them) back as a message.
const CLIENT_MIN_MESSAGES: &str = "warning";
/// No object of the connected database stands in for a catalog operator or
/// function.
const SAFE_SEARCH_PATH: &str = "pg_catalog,pg_temp";
/// The `transaction_read_only` and `default_transaction_read_only` of a
/// [`MaintenancePool`] connection.
const READ_ONLY: &str = "on";

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

/// What a session reports about itself when it opens: the role it signed in
/// as, the role its statements act as, its database, and the settings its
/// rules name. Every function and type is qualified and nothing is compared
/// in SQL, so no object of the database is called whatever the
/// `search_path` is.
const SESSION_STATE: &str = "SELECT session_user::pg_catalog.text, \
     current_user::pg_catalog.text, \
     pg_catalog.current_database()::pg_catalog.text, \
     pg_catalog.current_setting('search_path'), \
     pg_catalog.current_setting('lock_timeout'), \
     pg_catalog.current_setting('statement_timeout'), \
     pg_catalog.current_setting('client_min_messages'), \
     pg_catalog.current_setting('transaction_read_only'), \
     pg_catalog.current_setting('default_transaction_read_only')";

/// Every catalog statement of this module, by name, for the scan of
/// `cluster::tests`.
#[cfg(test)]
pub(super) const STATEMENTS: [(&str, &str); 1] = [("SESSION_STATE", SESSION_STATE)];

/// The options of a maintenance session: the product's server settings with
/// the session rules applied (the safe `search_path`, the server-side
/// bounds, `client_min_messages`, the application name, no statement
/// logging).
fn session_options(base: &PgConnectOptions, application_name: &str) -> PgConnectOptions {
    base.clone()
        .application_name(application_name)
        .options([
            ("search_path", SAFE_SEARCH_PATH),
            ("lock_timeout", LOCK_TIMEOUT),
            ("statement_timeout", STATEMENT_TIMEOUT),
            ("client_min_messages", CLIENT_MIN_MESSAGES),
        ])
        .disable_statement_logging()
}

/// The options of a [`MaintenancePool`] session on `database`: the server
/// bounds, read-only transactions and `client_min_messages`, the
/// database's own `search_path`.
fn pool_options(
    base: &PgConnectOptions,
    database: &DatabaseName,
    application_name: &str,
) -> PgConnectOptions {
    base.clone()
        .database(database.as_str())
        .application_name(application_name)
        .options([
            ("lock_timeout", LOCK_TIMEOUT),
            ("statement_timeout", STATEMENT_TIMEOUT),
            ("default_transaction_read_only", READ_ONLY),
            ("client_min_messages", CLIENT_MIN_MESSAGES),
        ])
        .disable_statement_logging()
}

/// `name` as an `application_name` Postgres shows unchanged: every character
/// outside printable ASCII becomes `?`, and the result is cut to 63 bytes.
fn clamp_application_name(name: &str) -> String {
    let mut clamped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect();
    clamped.truncate(APPLICATION_NAME_MAX);
    clamped
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
    /// The `application_name` of every session and pool, already clamped.
    application_name: String,
}

impl Maintenance {
    /// The maintenance connection of `options`, for the tenants of `names`.
    pub fn new(options: PgConnectOptions, names: ObjectNames) -> Self {
        Self {
            options,
            names,
            permit: Arc::new(Semaphore::new(1)),
            application_name: APPLICATION_NAME.to_owned(),
        }
    }

    /// The `application_name` of every maintenance session and pool, by
    /// default `sekvent-tenancy-maintenance`.
    ///
    /// Never fails: the name is clamped to what Postgres shows unchanged,
    /// every character outside printable ASCII replaced by `?` and the
    /// result cut to 63 bytes, so `pg_stat_activity` shows what
    /// [`Maintenance::application_name`] returns. It is not a secret.
    #[must_use]
    pub fn with_application_name(mut self, name: &str) -> Self {
        self.application_name = clamp_application_name(name);
        self
    }

    /// The `application_name` of every maintenance session and pool.
    pub fn application_name(&self) -> &str {
        &self.application_name
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
        self.open(session_options(&self.options, &self.application_name))
            .await
    }

    /// Connected to `database` instead (the handover only).
    pub async fn connect_to(&self, database: &DatabaseName) -> Result<MaintenanceConn, AppError> {
        self.open(
            session_options(&self.options, &self.application_name).database(database.as_str()),
        )
        .await
    }

    /// A pool of one connection on `database` that holds the permit, for a
    /// product's read-only check before a handover. Its sessions start
    /// read-only and keep the database's own `search_path`.
    ///
    /// The pool is lazy: nothing connects before its first use, and the
    /// permit is held from here until [`MaintenancePool::close`]. Each
    /// connection is confirmed when it opens (the module's rules); one that
    /// is refused is closed and logged by the driver, and the acquire fails
    /// with the driver's timeout after 10 s.
    pub async fn pool_on(&self, database: &DatabaseName) -> Result<MaintenancePool, AppError> {
        let permit = self.permit().await?;
        let name = database.as_str().to_owned();
        let user = self.role().to_owned();
        let pool = PgPoolOptions::new()
            .min_connections(0)
            .max_connections(1)
            .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
            .after_connect(move |conn, _meta| {
                let database = name.clone();
                let user = user.clone();
                Box::pin(async move { confirm_pool_connection(conn, &database, &user).await })
            })
            .connect_lazy_with(pool_options(
                &self.options,
                database,
                &self.application_name,
            ));
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
        let conn = match timeout(CONNECT_TIMEOUT, connect_checked(&options)).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(error)) => return Err(error),
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

/// A session with `options` that holds the session rules, on the database
/// the options name (else the database named like the user) as the
/// options' user; any other is dropped and refused `INTERNAL`, what it held
/// in the error's source.
async fn connect_checked(options: &PgConnectOptions) -> Result<PgConnection, AppError> {
    let mut conn = PgConnection::connect_with(options)
        .await
        .map_err(to_app_error)?;
    let state = read_state(&mut conn).await.map_err(to_app_error)?;
    let expected = Expected {
        database: options
            .get_database()
            .unwrap_or_else(|| options.get_username()),
        user: options.get_username(),
    };
    if let Some(refusal) = refusal(&expected, &state, Rules::Session) {
        // Dropped, not closed: a close that stalls would outlast the connect
        // bound and turn the refusal into `UNAVAILABLE`.
        std::mem::drop(conn);
        return Err(AppError::internal(refusal));
    }
    Ok(conn)
}

/// The `after_connect` of a [`MaintenancePool`] on `database` as `user`:
/// `Ok` when the connection holds the pool's rules. A refusal is an error
/// the driver logs before it closes the connection; it never reaches the
/// caller of an acquire.
async fn confirm_pool_connection(
    conn: &mut PgConnection,
    database: &str,
    user: &str,
) -> Result<(), sqlx::Error> {
    let state = read_state(conn).await?;
    match refusal(&Expected { database, user }, &state, Rules::Pool) {
        None => Ok(()),
        Some(refusal) => Err(sqlx::Error::Configuration(refusal.into())),
    }
}

/// Who a connection must be: the database it asked for and the user it
/// signed in as.
struct Expected<'a> {
    /// The database the connection asked for.
    database: &'a str,
    /// The user the connection signed in as.
    user: &'a str,
}

/// What a session reported about itself when it opened ([`SESSION_STATE`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionState {
    /// The role it signed in as.
    session_user: String,
    /// The role its statements act as.
    current_user: String,
    /// The database it is connected to.
    database: String,
    /// `search_path` as Postgres shows it.
    search_path: String,
    /// `lock_timeout` as Postgres shows it (`10s`).
    lock_timeout: String,
    /// `statement_timeout` as Postgres shows it (`2min`).
    statement_timeout: String,
    /// `client_min_messages`.
    client_min_messages: String,
    /// `transaction_read_only`, `on` or `off`.
    transaction_read_only: String,
    /// `default_transaction_read_only`, `on` or `off`.
    default_transaction_read_only: String,
}

/// One row of [`SESSION_STATE`].
type SessionRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

/// The one statement of [`SESSION_STATE`] on `conn`.
async fn read_state(conn: &mut PgConnection) -> Result<SessionState, sqlx::Error> {
    let (
        session_user,
        current_user,
        database,
        search_path,
        lock_timeout,
        statement_timeout,
        client_min_messages,
        transaction_read_only,
        default_transaction_read_only,
    ) = sqlx::query_as::<_, SessionRow>(SESSION_STATE)
        .fetch_one(conn)
        .await?;
    Ok(SessionState {
        session_user,
        current_user,
        database,
        search_path,
        lock_timeout,
        statement_timeout,
        client_min_messages,
        transaction_read_only,
        default_transaction_read_only,
    })
}

/// The rules a kind of maintenance connection is held to, besides the role
/// rule, the server-side bounds and `client_min_messages` every kind holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rules {
    /// A session of `connect` or `connect_to`: the safe `search_path`.
    Session,
    /// A connection of a [`MaintenancePool`]: read-only transactions by
    /// default and in the session's first, the database's own
    /// `search_path`.
    Pool,
}

impl Rules {
    /// The subject of a refusal.
    const fn subject(self) -> &'static str {
        match self {
            Self::Session => "the maintenance session",
            Self::Pool => "the maintenance pool session",
        }
    }
}

/// The text of the refusal of a connection that asked for `expected` and
/// whose `state` breaks `rules`, naming each broken rule; `None` when it
/// holds them.
fn refusal(expected: &Expected<'_>, state: &SessionState, rules: Rules) -> Option<String> {
    let found = session_refusals(expected, state, rules);
    if found.is_empty() {
        return None;
    }
    Some(format!(
        "{} on database {} is refused: {}",
        rules.subject(),
        expected.database,
        found.join("; ")
    ))
}

/// Pure: every rule of `rules` that `state` breaks, described, in order:
/// the session user and the database `expected` names, the role switch,
/// the `search_path`, the two bounds, `client_min_messages`, read-only
/// transactions (the session's first, then the default). Values are
/// compared as Postgres shows them: the names exactly, the `search_path`
/// as a list, the bounds in milliseconds, the words without regard to
/// case.
fn session_refusals(expected: &Expected<'_>, state: &SessionState, rules: Rules) -> Vec<String> {
    let mut found = Vec::new();
    if state.session_user != expected.user {
        found.push(differs("session_user", &state.session_user, expected.user));
    }
    if state.database != expected.database {
        found.push(differs("database", &state.database, expected.database));
    }
    found.extend(
        preflight::session_findings(&state.session_user, &state.current_user)
            .iter()
            .map(preflight::describe),
    );
    if rules == Rules::Session && !same_search_path(&state.search_path, SAFE_SEARCH_PATH) {
        found.push(differs("search_path", &state.search_path, SAFE_SEARCH_PATH));
    }
    if !same_duration(&state.lock_timeout, LOCK_TIMEOUT) {
        found.push(differs("lock_timeout", &state.lock_timeout, LOCK_TIMEOUT));
    }
    if !same_duration(&state.statement_timeout, STATEMENT_TIMEOUT) {
        found.push(differs(
            "statement_timeout",
            &state.statement_timeout,
            STATEMENT_TIMEOUT,
        ));
    }
    if !state
        .client_min_messages
        .eq_ignore_ascii_case(CLIENT_MIN_MESSAGES)
    {
        found.push(differs(
            "client_min_messages",
            &state.client_min_messages,
            CLIENT_MIN_MESSAGES,
        ));
    }
    if rules == Rules::Pool {
        for (setting, shown) in [
            ("transaction_read_only", &state.transaction_read_only),
            (
                "default_transaction_read_only",
                &state.default_transaction_read_only,
            ),
        ] {
            if !shown.eq_ignore_ascii_case(READ_ONLY) {
                found.push(differs(setting, shown, READ_ONLY));
            }
        }
    }
    found
}

/// `its <setting> is <shown>, not <asked>`, the shown value cut for the
/// log and every control character in it escaped (`\n`, `\u{1b}`), so that
/// it cannot start a line of its own.
fn differs(setting: &str, shown: &str, asked: &str) -> String {
    let mut escaped = String::new();
    for c in truncate_for_log(shown, 128).chars() {
        if c.is_control() {
            escaped.extend(c.escape_default());
        } else {
            escaped.push(c);
        }
    }
    format!("its {setting} is {escaped}, not {asked}")
}

/// Whether the `search_path` Postgres shows names the schemas of `asked`,
/// in order: entries split at commas, the whitespace Postgres's scanner
/// skips trimmed, a quoted entry unquoted, an unquoted one folded to lower
/// case as Postgres folds it.
fn same_search_path(shown: &str, asked: &str) -> bool {
    search_path_entries(shown) == search_path_entries(asked)
}

/// Whitespace as Postgres's scanner reads it: space, tab, newline, carriage
/// return, form feed and vertical tab. Other Unicode spaces are part of a
/// name.
fn is_sql_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b')
}

/// The schema names of a `search_path` value.
fn search_path_entries(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|entry| {
            let entry = entry.trim_matches(is_sql_whitespace);
            match entry
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
            {
                Some(quoted) => quoted.replace("\"\"", "\""),
                None => entry.to_ascii_lowercase(),
            }
        })
        .collect()
}

/// Whether two time settings as Postgres shows them are the same number of
/// milliseconds; a value that does not parse is never the same.
fn same_duration(shown: &str, asked: &str) -> bool {
    matches!(
        (duration_ms(shown), duration_ms(asked)),
        (Some(shown), Some(asked)) if shown == asked
    )
}

/// A time setting of millisecond base unit in milliseconds: a whole number
/// with no unit (milliseconds) or one of `ms`, `s`, `min`, `h`, `d`,
/// spaces allowed around the unit; `None` for anything else.
fn duration_ms(value: &str) -> Option<u64> {
    let value = value.trim();
    let digits = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(digits);
    let number: u64 = number.parse().ok()?;
    let factor: u64 = match unit.trim() {
        "" | "ms" => 1,
        "s" => 1_000,
        "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    number.checked_mul(factor)
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

    /// The operation's answer, for tests of an operation's body.
    #[cfg(test)]
    pub(crate) fn error(&self) -> &AppError {
        &self.error
    }

    /// Whether the session is idle and outside any transaction, for tests
    /// of an operation's body.
    #[cfg(test)]
    pub(crate) fn is_settled(&self) -> bool {
        self.settled
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
    use crate::cluster::testing;

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
        let session = session_options(&base, APPLICATION_NAME);
        assert_eq!(session.get_host(), "127.0.0.1");
        assert_eq!(session.get_port(), 1);
        assert_eq!(session.get_username(), "orders_maintenance");
        assert_eq!(session.get_database(), Some("orders_upkeep"));
        assert_eq!(session.get_application_name(), Some(APPLICATION_NAME));
        assert_eq!(
            session.get_options(),
            Some(
                "-c work_mem=4MB -c search_path=pg_catalog,pg_temp -c lock_timeout=10s \
                 -c statement_timeout=120s -c client_min_messages=warning"
            )
        );
    }

    #[test]
    fn a_pool_session_reads_only_and_keeps_the_search_path() {
        let database = DatabaseName::parse("orders_candidate").expect("a valid name");
        let pool = pool_options(&options(), &database, APPLICATION_NAME);
        assert_eq!(pool.get_database(), Some("orders_candidate"));
        assert_eq!(pool.get_username(), "orders_maintenance");
        assert_eq!(pool.get_application_name(), Some(APPLICATION_NAME));
        assert_eq!(
            pool.get_options(),
            Some(
                "-c lock_timeout=10s -c statement_timeout=120s \
                 -c default_transaction_read_only=on -c client_min_messages=warning"
            )
        );
    }

    #[tokio::test]
    async fn the_application_name_is_set_on_sessions_and_pools() {
        assert_eq!(maintenance().application_name(), APPLICATION_NAME);
        let named = maintenance().with_application_name("orders-upkeep");
        assert_eq!(named.application_name(), "orders-upkeep");
        assert_eq!(
            session_options(&options(), named.application_name()).get_application_name(),
            Some("orders-upkeep")
        );
        let database = DatabaseName::parse("orders").expect("a valid name");
        let pool = named.pool_on(&database).await.expect("a lazy pool");
        assert_eq!(
            pool.pool().connect_options().get_application_name(),
            Some("orders-upkeep")
        );
        pool.close().await;
        assert_eq!(format!("{named:?}"), "Maintenance(..)");
    }

    #[test]
    fn an_application_name_is_clamped_as_postgres_keeps_it() {
        assert_eq!(clamp_application_name("billing jobs ~1"), "billing jobs ~1");
        assert_eq!(clamp_application_name(""), "");
        assert_eq!(clamp_application_name("orders\tupkeep\n"), "orders?upkeep?");
        assert_eq!(clamp_application_name("café"), "caf?");
        let long = "a".repeat(80);
        assert_eq!(
            clamp_application_name(&long),
            "a".repeat(APPLICATION_NAME_MAX)
        );
        let wide = format!("{}é", "b".repeat(62));
        assert_eq!(
            clamp_application_name(&wide),
            format!("{}?", "b".repeat(62))
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

    /// The user every test connection asks to sign in as.
    const USER: &str = "orders_maintenance";

    /// What a connection to `database` as [`USER`] asks to be.
    fn expected(database: &str) -> Expected<'_> {
        Expected {
            database,
            user: USER,
        }
    }

    /// What `orders_upkeep` as [`USER`] asks to be.
    fn upkeep() -> Expected<'static> {
        expected("orders_upkeep")
    }

    /// What a session that holds every rule of both kinds reports, as
    /// Postgres shows it, on `orders_upkeep`.
    fn held() -> SessionState {
        SessionState {
            session_user: USER.to_owned(),
            current_user: USER.to_owned(),
            database: "orders_upkeep".to_owned(),
            search_path: "pg_catalog, pg_temp".to_owned(),
            lock_timeout: "10s".to_owned(),
            statement_timeout: "2min".to_owned(),
            client_min_messages: "warning".to_owned(),
            transaction_read_only: "on".to_owned(),
            default_transaction_read_only: "on".to_owned(),
        }
    }

    #[test]
    fn a_session_acting_as_another_role_is_refused() {
        assert_eq!(refusal(&upkeep(), &held(), Rules::Session), None);
        let switched = SessionState {
            current_user: "orders_reader".to_owned(),
            ..held()
        };
        assert_eq!(
            refusal(&upkeep(), &switched, Rules::Session).as_deref(),
            Some(
                "the maintenance session on database orders_upkeep is refused: the maintenance \
                 session runs as role orders_reader, not as its session role orders_maintenance"
            )
        );
        let candidate = SessionState {
            database: "orders_candidate".to_owned(),
            ..switched
        };
        assert_eq!(
            refusal(&expected("orders_candidate"), &candidate, Rules::Pool).as_deref(),
            Some(
                "the maintenance pool session on database orders_candidate is refused: the \
                 maintenance session runs as role orders_reader, not as its session role \
                 orders_maintenance"
            )
        );
    }

    #[test]
    fn a_session_on_another_database_or_as_another_user_is_refused() {
        for rules in [Rules::Session, Rules::Pool] {
            assert_eq!(
                session_refusals(&upkeep(), &held(), rules),
                [] as [String; 0]
            );
            assert_eq!(
                session_refusals(&expected("orders_candidate"), &held(), rules),
                ["its database is orders_upkeep, not orders_candidate"],
                "{rules:?}"
            );
            let other_user = Expected {
                user: "billing_maintenance",
                ..upkeep()
            };
            assert_eq!(
                session_refusals(&other_user, &held(), rules),
                ["its session_user is orders_maintenance, not billing_maintenance"],
                "{rules:?}"
            );
        }
        // Names are compared exactly: Postgres folds neither a startup user
        // nor a startup database name.
        let shouting = SessionState {
            session_user: "ORDERS_MAINTENANCE".to_owned(),
            current_user: "ORDERS_MAINTENANCE".to_owned(),
            database: "Orders_upkeep".to_owned(),
            ..held()
        };
        assert_eq!(
            refusal(&upkeep(), &shouting, Rules::Session).as_deref(),
            Some(
                "the maintenance session on database orders_upkeep is refused: its session_user \
                 is ORDERS_MAINTENANCE, not orders_maintenance; its database is Orders_upkeep, \
                 not orders_upkeep"
            )
        );
    }

    #[test]
    fn a_session_that_holds_its_rules_passes_in_any_display_form() {
        for (search_path, lock_timeout, statement_timeout, client_min_messages) in [
            ("pg_catalog,pg_temp", "10s", "120s", "warning"),
            ("pg_catalog, pg_temp", "10000ms", "2min", "WARNING"),
            (
                "\"pg_catalog\", \"pg_temp\"",
                "10000",
                "120000ms",
                "Warning",
            ),
            ("PG_CATALOG , pg_temp", "10 s", "120000", "warning"),
            ("pg_catalog,\t\npg_temp\r\x0b\x0c", "10s", "120s", "warning"),
        ] {
            let state = SessionState {
                search_path: search_path.to_owned(),
                lock_timeout: lock_timeout.to_owned(),
                statement_timeout: statement_timeout.to_owned(),
                client_min_messages: client_min_messages.to_owned(),
                ..held()
            };
            assert_eq!(
                session_refusals(&upkeep(), &state, Rules::Session),
                [] as [String; 0],
                "{state:?}"
            );
        }
    }

    #[test]
    fn every_broken_rule_of_a_session_is_named_in_order() {
        let state = SessionState {
            session_user: "orders_reader".to_owned(),
            current_user: "orders_writer".to_owned(),
            database: "orders_archive".to_owned(),
            search_path: "public, pg_catalog".to_owned(),
            lock_timeout: "0".to_owned(),
            statement_timeout: "1min".to_owned(),
            client_min_messages: "log".to_owned(),
            transaction_read_only: "off".to_owned(),
            default_transaction_read_only: "off".to_owned(),
        };
        assert_eq!(
            session_refusals(&upkeep(), &state, Rules::Session),
            [
                "its session_user is orders_reader, not orders_maintenance",
                "its database is orders_archive, not orders_upkeep",
                "the maintenance session runs as role orders_writer, not as its session role \
                 orders_reader",
                "its search_path is public, pg_catalog, not pg_catalog,pg_temp",
                "its lock_timeout is 0, not 10s",
                "its statement_timeout is 1min, not 120s",
                "its client_min_messages is log, not warning",
            ]
        );
        assert_eq!(
            session_refusals(&upkeep(), &state, Rules::Pool),
            [
                "its session_user is orders_reader, not orders_maintenance",
                "its database is orders_archive, not orders_upkeep",
                "the maintenance session runs as role orders_writer, not as its session role \
                 orders_reader",
                "its lock_timeout is 0, not 10s",
                "its statement_timeout is 1min, not 120s",
                "its client_min_messages is log, not warning",
                "its transaction_read_only is off, not on",
                "its default_transaction_read_only is off, not on",
            ]
        );
    }

    #[test]
    fn a_pool_session_keeps_its_search_path_and_must_read_only() {
        let own_path = SessionState {
            search_path: "\"$user\", public".to_owned(),
            ..held()
        };
        assert_eq!(
            session_refusals(&upkeep(), &own_path, Rules::Pool),
            [] as [String; 0]
        );
        assert_eq!(
            session_refusals(&upkeep(), &own_path, Rules::Session),
            ["its search_path is \"$user\", public, not pg_catalog,pg_temp"]
        );
        let writable = SessionState {
            transaction_read_only: "off".to_owned(),
            ..own_path.clone()
        };
        assert_eq!(
            session_refusals(&upkeep(), &writable, Rules::Pool),
            ["its transaction_read_only is off, not on"]
        );
        assert_eq!(
            session_refusals(&upkeep(), &writable, Rules::Session),
            ["its search_path is \"$user\", public, not pg_catalog,pg_temp"],
            "a session's transactions may write"
        );
        // A first transaction read-only by a `SET` of the session's own, its
        // later ones not: the default is what decides them.
        let writable_later = SessionState {
            default_transaction_read_only: "off".to_owned(),
            ..own_path
        };
        assert_eq!(
            session_refusals(&upkeep(), &writable_later, Rules::Pool),
            ["its default_transaction_read_only is off, not on"]
        );
        assert_eq!(
            session_refusals(&upkeep(), &writable_later, Rules::Session),
            ["its search_path is \"$user\", public, not pg_catalog,pg_temp"]
        );
    }

    #[test]
    fn a_long_shown_value_is_cut() {
        let state = SessionState {
            search_path: "s".repeat(500),
            ..held()
        };
        let found = session_refusals(&upkeep(), &state, Rules::Session);
        assert_eq!(found.len(), 1);
        assert!(found[0].len() < 300, "{}", found[0]);
    }

    #[test]
    fn a_shown_value_cannot_start_a_line_of_its_own() {
        assert_eq!(
            differs(
                "search_path",
                "public\nERROR forged line\r\x1b[31m\u{85}",
                SAFE_SEARCH_PATH
            ),
            "its search_path is public\\nERROR forged line\\r\\u{1b}[31m\\u{85}, not \
             pg_catalog,pg_temp"
        );
        assert_eq!(
            differs("search_path", "\"$user\", caf\u{e9}", SAFE_SEARCH_PATH),
            "its search_path is \"$user\", caf\u{e9}, not pg_catalog,pg_temp",
            "quotes and printable characters are kept"
        );
        let found = session_refusals(
            &upkeep(),
            &SessionState {
                lock_timeout: "1s\nok".to_owned(),
                ..held()
            },
            Rules::Session,
        );
        assert_eq!(found, ["its lock_timeout is 1s\\nok, not 10s"]);
        assert!(!found[0].contains('\n'));
    }

    #[test]
    fn a_search_path_is_compared_as_a_list() {
        assert!(same_search_path("pg_catalog,pg_temp", SAFE_SEARCH_PATH));
        assert!(same_search_path("pg_catalog, pg_temp", SAFE_SEARCH_PATH));
        assert!(same_search_path(
            "\"pg_catalog\",\"pg_temp\"",
            SAFE_SEARCH_PATH
        ));
        assert!(same_search_path(" Pg_Catalog ,PG_TEMP ", SAFE_SEARCH_PATH));
        assert!(same_search_path(
            "\tpg_catalog\x0c,\x0b pg_temp\r\n",
            SAFE_SEARCH_PATH
        ));
        for other in [
            "public, pg_catalog",
            "pg_catalog",
            "pg_temp, pg_catalog",
            "pg_catalog, pg_temp, public",
            "\"PG_CATALOG\", pg_temp",
            "\"\"",
            "",
            "\u{a0}pg_catalog, pg_temp",
            "pg_catalog\u{a0}, pg_temp",
            "pg_catalog, pg_temp\u{2003}",
        ] {
            assert!(!same_search_path(other, SAFE_SEARCH_PATH), "{other:?}");
        }
        assert_eq!(
            search_path_entries("\"$user\", public, \"a\"\"b\""),
            ["$user", "public", "a\"b"]
        );
        assert_eq!(
            search_path_entries("\u{a0}pg_catalog"),
            ["\u{a0}pg_catalog"],
            "a non-breaking space is part of the name"
        );
        for c in [' ', '\t', '\n', '\r', '\x0b', '\x0c'] {
            assert!(is_sql_whitespace(c), "{c:?}");
        }
        for c in ['\u{a0}', '\u{2003}', '\u{85}', 'a'] {
            assert!(!is_sql_whitespace(c), "{c:?}");
        }
    }

    #[test]
    fn a_time_setting_is_read_in_every_unit_postgres_shows() {
        for (value, ms) in [
            ("0", 0),
            ("1500", 1_500),
            ("1500ms", 1_500),
            ("10s", 10_000),
            ("10 s", 10_000),
            ("2min", 120_000),
            ("120000ms", 120_000),
            ("1h", 3_600_000),
            ("2d", 172_800_000),
        ] {
            assert_eq!(duration_ms(value), Some(ms), "{value}");
        }
        for value in [
            "",
            "s",
            "10x",
            "-1",
            "+10s",
            "1.5s",
            "10us",
            "18446744073709551615d",
        ] {
            assert_eq!(duration_ms(value), None, "{value}");
        }
        assert!(same_duration("2min", STATEMENT_TIMEOUT));
        assert!(same_duration("10s", LOCK_TIMEOUT));
        assert!(!same_duration("10s", STATEMENT_TIMEOUT));
        assert!(!same_duration("later", LOCK_TIMEOUT));
        assert_eq!(duration_ms(LOCK_TIMEOUT), Some(10_000));
        assert_eq!(duration_ms(STATEMENT_TIMEOUT), Some(120_000));
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

    /// A `public.current_setting(text)` that raises and a type
    /// `public.text`: what an unqualified read-back would reach first on a
    /// database whose own `search_path` puts `public` first.
    fn hostile_settings(_database: &str) -> String {
        format!(
            "CREATE FUNCTION public.current_setting(text) RETURNS text {}; \
             CREATE TYPE public.text AS (hostile integer)",
            testing::RAISES
        )
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn a_session_without_its_rules_is_refused_when_it_opens() {
        let Some((options, database)) = testing::public_first(hostile_settings).await else {
            return;
        };
        timeout(testing::SCENARIO_TIMEOUT, async {
            // No startup options, as through something that drops them: the
            // database's own settings, read back without calling its objects.
            let error = connect_checked(&options)
                .await
                .expect_err("the database's own settings are refused");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), None);
            assert_eq!(error.message(), "internal error");
            let why = source(&error);
            assert!(
                why.starts_with(&format!(
                    "the maintenance session on database {database} is refused: "
                )),
                "{why}"
            );
            for broken in [
                "its search_path is public",
                "its lock_timeout is ",
                "its statement_timeout is ",
                "its client_min_messages is ",
            ] {
                assert!(why.contains(broken), "{why}");
            }
            assert!(!why.contains("hostile"), "{why}");

            let mut conn = PgConnection::connect_with(&options)
                .await
                .expect("the superuser connected");
            let user = options.get_username();
            let refused = confirm_pool_connection(&mut conn, &database, user)
                .await
                .expect_err("the pool's rules are not held either");
            let text = refused.to_string();
            assert!(
                text.contains(&format!(
                    "the maintenance pool session on database {database} is refused: "
                )),
                "{text}"
            );
            assert!(
                text.contains("its transaction_read_only is off, not on"),
                "{text}"
            );
            assert!(
                text.contains("its default_transaction_read_only is off, not on"),
                "{text}"
            );
            assert!(!text.contains("search_path"), "{text}");
            assert!(!text.contains("its database is"), "{text}");
            assert!(!text.contains("its session_user is"), "{text}");

            // The session reports the database it is on and the user it
            // signed in as, whatever the connection asked for.
            let refused = confirm_pool_connection(&mut conn, "orders_elsewhere", "billing_upkeep")
                .await
                .expect_err("another database and user are refused");
            let text = refused.to_string();
            assert!(
                text.contains(&format!(
                    "its session_user is {user}, not billing_upkeep; its database is \
                     {database}, not orders_elsewhere"
                )),
                "{text}"
            );
            conn.close().await.expect("the session closed");

            let held = connect_checked(&session_options(&options, APPLICATION_NAME))
                .await
                .expect("the asked settings outrank the database's");
            held.close().await.expect("the session closed");
        })
        .await
        .expect("the scenario finished within its 30 s guard");
    }

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn every_kind_of_connection_holds_its_rules_on_a_real_server() {
        let Some((options, database)) = testing::public_first(hostile_settings).await else {
            return;
        };
        timeout(testing::SCENARIO_TIMEOUT, async {
            let database = DatabaseName::parse(&database).expect("a harness name is valid");
            let maintenance = Maintenance::new(options, names());
            let conn = maintenance
                .connect()
                .await
                .expect("a session holds its rules");
            conn.close().await.expect("the session closed");
            let conn = maintenance
                .connect_to(&database)
                .await
                .expect("a handover session holds its rules");
            conn.close().await.expect("the session closed");

            let pool = maintenance.pool_on(&database).await.expect("a lazy pool");
            let mut conn = pool
                .pool()
                .acquire()
                .await
                .expect("the pool's connection holds its rules");
            let (search_path, statement_timeout, read_only): (String, String, String) =
                sqlx::query_as(
                    "SELECT pg_catalog.current_setting('search_path'), \
                     pg_catalog.current_setting('statement_timeout'), \
                     pg_catalog.current_setting('transaction_read_only')",
                )
                .fetch_one(&mut *conn)
                .await
                .expect("the settings were read");
            assert!(search_path.starts_with("public"), "{search_path}");
            assert_eq!(statement_timeout, "2min", "120 s as Postgres shows it");
            assert_eq!(read_only, "on");
            std::mem::drop(conn);
            pool.close().await;
        })
        .await
        .expect("the scenario finished within its 30 s guard");
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
