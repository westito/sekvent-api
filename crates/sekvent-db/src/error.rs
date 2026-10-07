use std::fmt;

use sekvent_error::{AppError, ErrorCode};

/// Why a pool could not connect, without the driver's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConnectFailure {
    /// The server could not be reached (DNS, refused, TLS, reset).
    Unreachable,
    /// The server rejected the credentials or denied access to the database.
    Auth,
    /// No connection became available in time.
    Timeout,
    /// The URL or connect options are malformed.
    BadUrl,
    /// Anything else.
    Other,
}

impl ConnectFailure {
    /// The stable, log-friendly name: `unreachable`, `auth`, `timeout`,
    /// `bad-url` or `other`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::Auth => "auth",
            Self::Timeout => "timeout",
            Self::BadUrl => "bad-url",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A pool setup or migration failure. Messages name pools and settings,
/// never URLs or credentials.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbError {
    /// No pool with this name was declared.
    #[error("database pool `{0}` is not registered")]
    UnknownPool(String),
    /// The pool was declared optional and has no URL.
    #[error("database pool `{0}` is not configured")]
    NotConfigured(String),
    /// Two specs share a name.
    #[error("database pool `{0}` is declared more than once")]
    DuplicatePool(String),
    /// Connecting failed.
    #[error("database pool `{pool}` failed to connect: {reason}")]
    Connect {
        /// Pool name.
        pool: String,
        /// Classified cause.
        reason: ConnectFailure,
    },
    /// The URL is malformed.
    #[error("database pool `{pool}` has an invalid URL: {reason}")]
    BadUrl {
        /// Pool name.
        pool: String,
        /// What is wrong, without quoting the URL.
        reason: &'static str,
    },
    /// The URL's scheme names a backend this build does not support.
    #[error("database pool `{pool}` uses scheme `{scheme}`, which this build does not support")]
    UnsupportedScheme {
        /// Pool name.
        pool: String,
        /// The scheme, e.g. `sqlite`.
        scheme: String,
    },
    /// A pool setting is invalid for its backend.
    #[error("database pool `{pool}`: {reason}")]
    InvalidSetting {
        /// Pool name.
        pool: String,
        /// What is wrong.
        reason: String,
    },
    /// Two pools point at the same database.
    #[error("database pools `{first}` and `{second}` point at the same database")]
    SameTarget {
        /// The first pool.
        first: String,
        /// The second pool.
        second: String,
    },
    /// Running migrations failed.
    #[error("migrations for `{target}` failed")]
    Migrate {
        /// The pool name or migrations directory.
        target: String,
        /// The migrator's error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl DbError {
    /// The status code a caller would see.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Connect { .. } | Self::NotConfigured(_) => ErrorCode::Unavailable,
            _ => ErrorCode::Internal,
        }
    }
}

impl From<DbError> for AppError {
    fn from(error: DbError) -> Self {
        let code = error.code();
        AppError::new(code, public_message(code)).with_source(error)
    }
}

/// The generic, caller-safe message used for a database failure with
/// `code`. It never describes the query, the table or the driver error.
pub fn public_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::NotFound => "not found",
        ErrorCode::AlreadyExists => "already exists",
        ErrorCode::FailedPrecondition => "a related record is missing or still in use",
        ErrorCode::Aborted => "conflicting concurrent update; retry",
        ErrorCode::Unavailable => "database unavailable",
        ErrorCode::PermissionDenied => "permission denied",
        _ => "internal error",
    }
}

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
pub(crate) mod sqlx_impl {
    use sekvent_error::{AppError, ErrorCode};
    use sqlx::error::{DatabaseError, ErrorKind};

    use super::{ConnectFailure, app_error};

    /// SQLSTATE classes and codes that mean the connection, not the query,
    /// failed: `08xxx` connection exceptions, `57P01..03` shutdowns,
    /// `53300` too many connections.
    fn sqlstate_unavailable(code: &str) -> bool {
        code.starts_with("08") || matches!(code, "57P01" | "57P02" | "57P03" | "53300")
    }

    /// Serialization failure and deadlock: safe to retry the transaction.
    fn sqlstate_aborted(code: &str) -> bool {
        matches!(code, "40001" | "40P01")
    }

    /// A server error that refused us access, decided before the error's
    /// kind or SQLSTATE class.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Access {
        /// The server rejected the connection's own credentials: the
        /// service is misconfigured, so the code is `INTERNAL`.
        CredentialsRejected,
        /// A statement lacks a grant: `PERMISSION_DENIED`.
        Denied,
    }

    impl Access {
        pub(crate) fn code(self) -> ErrorCode {
            match self {
                Self::CredentialsRejected => ErrorCode::Internal,
                Self::Denied => ErrorCode::PermissionDenied,
            }
        }

        pub(crate) fn reason(self) -> &'static str {
            match self {
                Self::CredentialsRejected => crate::reasons::DB_CREDENTIALS_REJECTED,
                Self::Denied => crate::reasons::DB_PERMISSION_DENIED,
            }
        }
    }

    /// Invalid authorization (`28000`, `28P01`) rejects the credentials;
    /// Postgres's insufficient privilege (`42501`) denies a statement. Not
    /// `42000`: MySQL reports syntax errors with it.
    fn sqlstate_access(code: &str) -> Option<Access> {
        match code {
            "28000" | "28P01" => Some(Access::CredentialsRejected),
            "42501" => Some(Access::Denied),
            _ => None,
        }
    }

    /// The access refusal a MySQL server error number means, where the
    /// number alone decides it.
    ///
    /// | errno | meaning | access |
    /// |---|---|---|
    /// | 1045 | access denied for the user | credentials rejected |
    /// | 1044 | access denied to the database | denied |
    /// | 1142 | command denied on a table | denied |
    /// | 1143 | command denied on a column | denied |
    /// | 1227 | the statement needs a privilege | denied |
    /// | 1370 | command denied on a routine | denied |
    #[cfg(feature = "sqlx-mysql")]
    pub(crate) fn mysql_errno_access(number: u16) -> Option<Access> {
        match number {
            1045 => Some(Access::CredentialsRejected),
            1044 | 1142 | 1143 | 1227 | 1370 => Some(Access::Denied),
            _ => None,
        }
    }

    fn database_access(error: &dyn DatabaseError) -> Option<Access> {
        #[cfg(feature = "sqlx-mysql")]
        if let Some(access) = mysql_errno(error).and_then(mysql_errno_access) {
            return Some(access);
        }
        error.code().as_deref().and_then(sqlstate_access)
    }

    /// The access refusal `error` reports, if any.
    pub(crate) fn access(error: &sqlx::Error) -> Option<Access> {
        match error {
            sqlx::Error::Database(database) => database_access(database.as_ref()),
            _ => None,
        }
    }

    /// The MySQL server error number, when `error` came from MySQL.
    #[cfg(feature = "sqlx-mysql")]
    fn mysql_errno(error: &dyn DatabaseError) -> Option<u16> {
        error
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .map(sqlx::mysql::MySqlDatabaseError::number)
    }

    fn classify_database(error: &dyn DatabaseError) -> ErrorCode {
        if let Some(access) = database_access(error) {
            return access.code();
        }
        match error.kind() {
            ErrorKind::UniqueViolation => ErrorCode::AlreadyExists,
            ErrorKind::ForeignKeyViolation => ErrorCode::FailedPrecondition,
            _ => match error.code().as_deref() {
                Some(code) if sqlstate_unavailable(code) => ErrorCode::Unavailable,
                Some(code) if sqlstate_aborted(code) => ErrorCode::Aborted,
                _ => ErrorCode::Internal,
            },
        }
    }

    /// The status code for a sqlx error.
    ///
    /// | error | code |
    /// |---|---|
    /// | I/O, TLS, pool timeout, closed pool, crashed worker | `UNAVAILABLE` |
    /// | unique violation | `ALREADY_EXISTS` |
    /// | foreign-key violation | `FAILED_PRECONDITION` |
    /// | serialization failure, deadlock | `ABORTED` |
    /// | MySQL errno 1044, 1142, 1143, 1227, 1370; SQLSTATE `42501` (a missing grant) | `PERMISSION_DENIED` |
    /// | MySQL errno 1045; SQLSTATE `28000`, `28P01` (the service's own credentials rejected) | `INTERNAL` |
    /// | `RowNotFound` | `NOT_FOUND` |
    /// | anything else | `INTERNAL` |
    pub fn classify(error: &sqlx::Error) -> ErrorCode {
        match error {
            sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::WorkerCrashed => ErrorCode::Unavailable,
            sqlx::Error::RowNotFound => ErrorCode::NotFound,
            sqlx::Error::Database(database) => classify_database(database.as_ref()),
            _ => ErrorCode::Internal,
        }
    }

    /// The class of a failed connect, for logs that must not quote the
    /// driver's message.
    pub fn classify_connect(error: &sqlx::Error) -> ConnectFailure {
        match error {
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) => ConnectFailure::Unreachable,
            sqlx::Error::PoolTimedOut => ConnectFailure::Timeout,
            sqlx::Error::Configuration(_) => ConnectFailure::BadUrl,
            sqlx::Error::Database(database) => {
                // MySQL's "access denied to the database" carries SQLSTATE
                // 42000, which alone also means a syntax error.
                #[cfg(feature = "sqlx-mysql")]
                if mysql_errno(database.as_ref()) == Some(1044) {
                    return ConnectFailure::Auth;
                }
                match database.code().as_deref() {
                    // Invalid authorization / invalid password (Postgres and
                    // MySQL's SQLSTATE for "access denied").
                    Some("28000" | "28P01") => ConnectFailure::Auth,
                    _ => ConnectFailure::Other,
                }
            }
            _ => ConnectFailure::Other,
        }
    }

    /// An [`AppError`] with the classified code and a generic message; the
    /// sqlx error is kept only as the internal source. A missing grant
    /// carries the reason `DB_PERMISSION_DENIED`, rejected credentials
    /// `DB_CREDENTIALS_REJECTED`.
    pub fn to_app_error(error: sqlx::Error) -> AppError {
        let code = classify(&error);
        let reason = access(&error).map(Access::reason);
        app_error(code, reason).with_source(error)
    }
}

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
pub use sqlx_impl::{classify, classify_connect, to_app_error};

/// The generic error for `code`, with `reason` when there is one.
#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql", feature = "sea-orm"))]
fn app_error(code: ErrorCode, reason: Option<&'static str>) -> AppError {
    let error = AppError::new(code, public_message(code));
    match reason {
        Some(reason) => error.with_reason(reason),
        None => error,
    }
}

#[cfg(feature = "sea-orm")]
mod sea_orm_impl {
    use sea_orm::{DbErr, SqlErr};
    use sekvent_error::{AppError, ErrorCode};

    use super::app_error;

    /// The sqlx error a sea-orm error wraps, if any.
    #[cfg(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql"))]
    fn wrapped_sqlx(error: &DbErr) -> Option<&sqlx::Error> {
        match error {
            DbErr::Conn(sea_orm::RuntimeErr::SqlxError(inner))
            | DbErr::Exec(sea_orm::RuntimeErr::SqlxError(inner))
            | DbErr::Query(sea_orm::RuntimeErr::SqlxError(inner)) => Some(inner.as_ref()),
            _ => None,
        }
    }

    /// The status code for a sea-orm error, consistent with
    /// [`classify`](crate::classify) for the sqlx errors it wraps. A failed
    /// connection is `UNAVAILABLE` unless the server rejected the
    /// credentials (`INTERNAL`).
    pub fn classify_db_err(error: &DbErr) -> ErrorCode {
        #[cfg(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql"))]
        if let Some(inner) = wrapped_sqlx(error) {
            let code = crate::classify(inner);
            let unclassified_connect = matches!(error, DbErr::Conn(_))
                && code == ErrorCode::Internal
                && super::sqlx_impl::access(inner).is_none();
            return if unclassified_connect {
                ErrorCode::Unavailable
            } else {
                code
            };
        }
        match error {
            DbErr::ConnectionAcquire(_) | DbErr::Conn(_) => ErrorCode::Unavailable,
            DbErr::RecordNotFound(_) | DbErr::RecordNotUpdated => ErrorCode::NotFound,
            _ => match error.sql_err() {
                Some(SqlErr::UniqueConstraintViolation(_)) => ErrorCode::AlreadyExists,
                Some(SqlErr::ForeignKeyConstraintViolation(_)) => ErrorCode::FailedPrecondition,
                _ => ErrorCode::Internal,
            },
        }
    }

    /// An [`AppError`] with the classified code and a generic message; the
    /// sea-orm error is kept only as the internal source. Wrapped sqlx
    /// errors carry the same reasons as `to_app_error`.
    pub fn db_err_to_app_error(error: DbErr) -> AppError {
        let code = classify_db_err(&error);
        #[cfg(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql"))]
        let reason = wrapped_sqlx(&error)
            .and_then(super::sqlx_impl::access)
            .map(super::sqlx_impl::Access::reason);
        #[cfg(not(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql")))]
        let reason = None;
        app_error(code, reason).with_source(error)
    }
}

#[cfg(feature = "sea-orm")]
pub use sea_orm_impl::{classify_db_err, db_err_to_app_error};

/// Convert a database result into an [`AppError`] result with a generic
/// caller message: `query.await.into_app_error()?`.
#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql", feature = "sea-orm"))]
pub trait IntoAppError<T> {
    /// Map the error with [`to_app_error`](crate::to_app_error) or
    /// [`db_err_to_app_error`](crate::db_err_to_app_error).
    fn into_app_error(self) -> Result<T, AppError>;
}

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
impl<T> IntoAppError<T> for Result<T, sqlx::Error> {
    fn into_app_error(self) -> Result<T, AppError> {
        self.map_err(to_app_error)
    }
}

#[cfg(feature = "sea-orm")]
impl<T> IntoAppError<T> for Result<T, sea_orm::DbErr> {
    fn into_app_error(self) -> Result<T, AppError> {
        self.map_err(db_err_to_app_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_failures_have_stable_names() {
        let names: Vec<String> = [
            ConnectFailure::Unreachable,
            ConnectFailure::Auth,
            ConnectFailure::Timeout,
            ConnectFailure::BadUrl,
            ConnectFailure::Other,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            names,
            ["unreachable", "auth", "timeout", "bad-url", "other"]
        );
    }

    #[test]
    fn public_messages_are_generic() {
        assert_eq!(public_message(ErrorCode::NotFound), "not found");
        assert_eq!(public_message(ErrorCode::AlreadyExists), "already exists");
        assert_eq!(
            public_message(ErrorCode::FailedPrecondition),
            "a related record is missing or still in use"
        );
        assert_eq!(
            public_message(ErrorCode::Aborted),
            "conflicting concurrent update; retry"
        );
        assert_eq!(
            public_message(ErrorCode::Unavailable),
            "database unavailable"
        );
        assert_eq!(public_message(ErrorCode::Internal), "internal error");
        assert_eq!(public_message(ErrorCode::DataLoss), "internal error");
    }

    #[test]
    fn db_errors_name_pools_only() {
        let error = DbError::Connect {
            pool: "billing".to_owned(),
            reason: ConnectFailure::Auth,
        };
        assert_eq!(
            error.to_string(),
            "database pool `billing` failed to connect: auth"
        );
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(
            DbError::NotConfigured("x".to_owned()).code(),
            ErrorCode::Unavailable
        );
        assert_eq!(
            DbError::UnknownPool("x".to_owned()).code(),
            ErrorCode::Internal
        );

        let app: AppError = DbError::SameTarget {
            first: "a".to_owned(),
            second: "b".to_owned(),
        }
        .into();
        assert_eq!(app.code(), ErrorCode::Internal);
        assert_eq!(app.message(), "internal error");
        assert!(std::error::Error::source(&app).is_some());
    }

    #[test]
    fn every_db_error_renders() {
        let rendered = [
            DbError::UnknownPool("a".to_owned()).to_string(),
            DbError::DuplicatePool("a".to_owned()).to_string(),
            DbError::BadUrl {
                pool: "a".to_owned(),
                reason: "missing scheme",
            }
            .to_string(),
            DbError::UnsupportedScheme {
                pool: "a".to_owned(),
                scheme: "sqlite".to_owned(),
            }
            .to_string(),
            DbError::InvalidSetting {
                pool: "a".to_owned(),
                reason: "bad role".to_owned(),
            }
            .to_string(),
            DbError::Migrate {
                target: "a".to_owned(),
                source: "boom".into(),
            }
            .to_string(),
        ];
        assert_eq!(
            rendered,
            [
                "database pool `a` is not registered",
                "database pool `a` is declared more than once",
                "database pool `a` has an invalid URL: missing scheme",
                "database pool `a` uses scheme `sqlite`, which this build does not support",
                "database pool `a`: bad role",
                "migrations for `a` failed",
            ]
        );
    }

    #[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
    mod sqlx_tests {
        use std::borrow::Cow;
        use std::error::Error as StdError;
        use std::fmt;

        use sekvent_error::ErrorCode;
        use sqlx::error::{DatabaseError, ErrorKind};

        use crate::error::sqlx_impl::{Access, access};
        use crate::{
            ConnectFailure, IntoAppError, classify, classify_connect, reasons, to_app_error,
        };

        #[derive(Debug)]
        struct FakeDbError {
            code: Option<&'static str>,
            kind: ErrorKind,
        }

        impl fmt::Display for FakeDbError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("fake database error")
            }
        }

        impl StdError for FakeDbError {}

        impl DatabaseError for FakeDbError {
            fn message(&self) -> &'static str {
                "fake database error"
            }
            fn code(&self) -> Option<Cow<'_, str>> {
                self.code.map(Cow::Borrowed)
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
                match self.kind {
                    ErrorKind::UniqueViolation => ErrorKind::UniqueViolation,
                    ErrorKind::ForeignKeyViolation => ErrorKind::ForeignKeyViolation,
                    _ => ErrorKind::Other,
                }
            }
        }

        pub(super) fn database(code: Option<&'static str>, kind: ErrorKind) -> sqlx::Error {
            sqlx::Error::Database(Box::new(FakeDbError { code, kind }))
        }

        fn io() -> sqlx::Error {
            sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
        }

        #[test]
        fn transport_failures_are_unavailable() {
            for error in [
                io(),
                sqlx::Error::Tls("handshake".into()),
                sqlx::Error::PoolTimedOut,
                sqlx::Error::PoolClosed,
                sqlx::Error::WorkerCrashed,
                database(Some("08006"), ErrorKind::Other),
                database(Some("57P01"), ErrorKind::Other),
                database(Some("53300"), ErrorKind::Other),
            ] {
                assert_eq!(classify(&error), ErrorCode::Unavailable, "{error:?}");
            }
        }

        #[test]
        fn constraint_violations_map_to_caller_codes() {
            assert_eq!(
                classify(&database(Some("23505"), ErrorKind::UniqueViolation)),
                ErrorCode::AlreadyExists
            );
            assert_eq!(
                classify(&database(Some("23503"), ErrorKind::ForeignKeyViolation)),
                ErrorCode::FailedPrecondition
            );
            assert_eq!(
                classify(&database(Some("40001"), ErrorKind::Other)),
                ErrorCode::Aborted
            );
            assert_eq!(
                classify(&database(Some("40P01"), ErrorKind::Other)),
                ErrorCode::Aborted
            );
            assert_eq!(
                classify(&database(Some("23514"), ErrorKind::CheckViolation)),
                ErrorCode::Internal
            );
            assert_eq!(
                classify(&database(None, ErrorKind::Other)),
                ErrorCode::Internal
            );
        }

        #[test]
        fn a_missing_privilege_is_permission_denied() {
            assert_eq!(
                classify(&database(Some("42501"), ErrorKind::Other)),
                ErrorCode::PermissionDenied
            );
            assert_eq!(
                access(&database(Some("42501"), ErrorKind::Other)),
                Some(Access::Denied)
            );
            // MySQL uses 42000 for syntax errors too: never enough alone.
            assert_eq!(
                classify(&database(Some("42000"), ErrorKind::Other)),
                ErrorCode::Internal
            );
            assert_eq!(access(&database(Some("42000"), ErrorKind::Other)), None);
            assert_eq!(access(&database(None, ErrorKind::Other)), None);
            assert_eq!(access(&sqlx::Error::PoolClosed), None);
        }

        #[test]
        fn rejected_credentials_are_internal() {
            for code in ["28000", "28P01"] {
                let error = database(Some(code), ErrorKind::Other);
                assert_eq!(classify(&error), ErrorCode::Internal, "{code}");
                assert_eq!(access(&error), Some(Access::CredentialsRejected), "{code}");
                assert_eq!(classify_connect(&error), ConnectFailure::Auth, "{code}");
            }
        }

        #[test]
        fn permission_errors_keep_a_generic_message() {
            let app = to_app_error(database(Some("42501"), ErrorKind::Other));
            assert_eq!(app.code(), ErrorCode::PermissionDenied);
            assert_eq!(app.message(), "permission denied");
            assert_eq!(app.reason(), Some(reasons::DB_PERMISSION_DENIED));
            assert!(!app.to_wire().message.contains("fake"));
            assert_eq!(
                crate::public_message(ErrorCode::PermissionDenied),
                "permission denied"
            );
        }

        #[test]
        fn rejected_credentials_keep_a_generic_message() {
            let app = to_app_error(database(Some("28P01"), ErrorKind::Other));
            assert_eq!(app.code(), ErrorCode::Internal);
            assert_eq!(app.message(), "internal error");
            assert_eq!(app.reason(), Some(reasons::DB_CREDENTIALS_REJECTED));
            assert!(!app.to_wire().message.contains("fake"));
            assert!(std::error::Error::source(&app).is_some());
        }

        #[test]
        fn other_errors_carry_no_reason() {
            assert_eq!(to_app_error(sqlx::Error::RowNotFound).reason(), None);
            assert_eq!(
                to_app_error(database(Some("42000"), ErrorKind::Other)).reason(),
                None
            );
        }

        #[cfg(feature = "sqlx-mysql")]
        #[test]
        fn mysql_errnos_split_grants_from_credentials() {
            use crate::error::sqlx_impl::mysql_errno_access;
            for number in [1044, 1142, 1143, 1227, 1370] {
                let access = mysql_errno_access(number);
                assert_eq!(access, Some(Access::Denied), "{number}");
                assert_eq!(access.map(Access::code), Some(ErrorCode::PermissionDenied));
            }
            assert_eq!(mysql_errno_access(1045), Some(Access::CredentialsRejected));
            assert_eq!(Access::CredentialsRejected.code(), ErrorCode::Internal);
            assert_eq!(mysql_errno_access(1064), None);
            assert_eq!(mysql_errno_access(1146), None);
        }

        #[test]
        fn missing_rows_are_not_found_and_the_rest_internal() {
            assert_eq!(classify(&sqlx::Error::RowNotFound), ErrorCode::NotFound);
            assert_eq!(
                classify(&sqlx::Error::Protocol("x".to_owned())),
                ErrorCode::Internal
            );
            assert_eq!(
                classify(&sqlx::Error::ColumnNotFound("x".to_owned())),
                ErrorCode::Internal
            );
        }

        #[test]
        fn connect_failures_are_classified_without_text() {
            assert_eq!(classify_connect(&io()), ConnectFailure::Unreachable);
            assert_eq!(
                classify_connect(&sqlx::Error::Tls("x".into())),
                ConnectFailure::Unreachable
            );
            assert_eq!(
                classify_connect(&sqlx::Error::PoolTimedOut),
                ConnectFailure::Timeout
            );
            assert_eq!(
                classify_connect(&sqlx::Error::Configuration("x".into())),
                ConnectFailure::BadUrl
            );
            assert_eq!(
                classify_connect(&database(Some("28P01"), ErrorKind::Other)),
                ConnectFailure::Auth
            );
            assert_eq!(
                classify_connect(&database(Some("28000"), ErrorKind::Other)),
                ConnectFailure::Auth
            );
            assert_eq!(
                classify_connect(&database(Some("3D000"), ErrorKind::Other)),
                ConnectFailure::Other
            );
            assert_eq!(
                classify_connect(&sqlx::Error::Protocol("x".to_owned())),
                ConnectFailure::Other
            );
        }

        #[test]
        fn app_errors_keep_the_driver_error_internal() {
            let app = to_app_error(database(Some("23505"), ErrorKind::UniqueViolation));
            assert_eq!(app.code(), ErrorCode::AlreadyExists);
            assert_eq!(app.message(), "already exists");
            assert!(!app.to_wire().message.contains("fake"));
            assert!(std::error::Error::source(&app).is_some());

            let result: Result<(), sqlx::Error> = Err(sqlx::Error::RowNotFound);
            assert_eq!(
                result.into_app_error().unwrap_err().code(),
                ErrorCode::NotFound
            );
            let ok: Result<u8, sqlx::Error> = Ok(1);
            assert_eq!(ok.into_app_error().unwrap(), 1);
        }
    }

    #[cfg(feature = "sea-orm")]
    mod sea_orm_tests {
        use sea_orm::{ConnAcquireErr, DbErr, RuntimeErr};
        use sekvent_error::ErrorCode;

        use crate::{IntoAppError, classify_db_err, db_err_to_app_error};

        #[test]
        fn sea_orm_errors_are_classified() {
            assert_eq!(
                classify_db_err(&DbErr::ConnectionAcquire(ConnAcquireErr::Timeout)),
                ErrorCode::Unavailable
            );
            assert_eq!(
                classify_db_err(&DbErr::Conn(RuntimeErr::Internal("x".to_owned()))),
                ErrorCode::Unavailable
            );
            assert_eq!(
                classify_db_err(&DbErr::RecordNotFound("x".to_owned())),
                ErrorCode::NotFound
            );
            assert_eq!(
                classify_db_err(&DbErr::RecordNotUpdated),
                ErrorCode::NotFound
            );
            assert_eq!(
                classify_db_err(&DbErr::Custom("x".to_owned())),
                ErrorCode::Internal
            );
            assert_eq!(
                classify_db_err(&DbErr::Exec(RuntimeErr::Internal("x".to_owned()))),
                ErrorCode::Internal
            );
        }

        #[cfg(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql"))]
        #[test]
        fn wrapped_sqlx_errors_use_the_sqlx_classification() {
            use std::sync::Arc;
            let wrapped = |error| RuntimeErr::SqlxError(Arc::new(error));
            assert_eq!(
                classify_db_err(&DbErr::Query(wrapped(sqlx::Error::RowNotFound))),
                ErrorCode::NotFound
            );
            assert_eq!(
                classify_db_err(&DbErr::Exec(wrapped(sqlx::Error::PoolTimedOut))),
                ErrorCode::Unavailable
            );
            assert_eq!(
                classify_db_err(&DbErr::Conn(wrapped(sqlx::Error::Protocol("x".to_owned())))),
                ErrorCode::Unavailable
            );
        }

        #[cfg(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql"))]
        #[test]
        fn wrapped_access_errors_keep_their_codes_and_reasons() {
            use std::sync::Arc;

            use sqlx::error::ErrorKind;

            use super::sqlx_tests::database;
            use crate::reasons;

            let wrapped =
                |code| RuntimeErr::SqlxError(Arc::new(database(Some(code), ErrorKind::Other)));

            // Rejected credentials while connecting are the service's own
            // misconfiguration, not an outage.
            let credentials = db_err_to_app_error(DbErr::Conn(wrapped("28P01")));
            assert_eq!(credentials.code(), ErrorCode::Internal);
            assert_eq!(credentials.message(), "internal error");
            assert_eq!(credentials.reason(), Some(reasons::DB_CREDENTIALS_REJECTED));
            assert_eq!(
                classify_db_err(&DbErr::Query(wrapped("28000"))),
                ErrorCode::Internal
            );

            let denied = db_err_to_app_error(DbErr::Exec(wrapped("42501")));
            assert_eq!(denied.code(), ErrorCode::PermissionDenied);
            assert_eq!(denied.message(), "permission denied");
            assert_eq!(denied.reason(), Some(reasons::DB_PERMISSION_DENIED));
            assert_eq!(
                classify_db_err(&DbErr::Conn(wrapped("42501"))),
                ErrorCode::PermissionDenied
            );

            // Any other failed connection stays an outage, without a reason.
            let other = db_err_to_app_error(DbErr::Conn(wrapped("3D000")));
            assert_eq!(other.code(), ErrorCode::Unavailable);
            assert_eq!(other.reason(), None);
            assert_eq!(
                db_err_to_app_error(DbErr::Custom("x".to_owned())).reason(),
                None
            );
        }

        #[test]
        fn sea_orm_app_errors_are_generic() {
            let app = db_err_to_app_error(DbErr::RecordNotFound("secret table".to_owned()));
            assert_eq!(app.message(), "not found");
            let result: Result<(), DbErr> = Err(DbErr::Custom("x".to_owned()));
            assert_eq!(
                result.into_app_error().unwrap_err().code(),
                ErrorCode::Internal
            );
        }
    }
}
