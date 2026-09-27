use std::fmt;

use sekvent_error::{AppError, ErrorCode};

/// Why a pool could not connect, without the driver's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConnectFailure {
    /// The server could not be reached (DNS, refused, TLS, reset).
    Unreachable,
    /// The server rejected the credentials.
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
        _ => "internal error",
    }
}

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
mod sqlx_impl {
    use sekvent_error::{AppError, ErrorCode};
    use sqlx::error::{DatabaseError, ErrorKind};

    use super::{ConnectFailure, public_message};

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

    fn classify_database(error: &dyn DatabaseError) -> ErrorCode {
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
            sqlx::Error::Database(database) => match database.code().as_deref() {
                // Invalid authorization / invalid password (Postgres and
                // MySQL's SQLSTATE for "access denied").
                Some("28000" | "28P01") => ConnectFailure::Auth,
                _ => ConnectFailure::Other,
            },
            _ => ConnectFailure::Other,
        }
    }

    /// An [`AppError`] with the classified code and a generic message; the
    /// sqlx error is kept only as the internal source.
    pub fn to_app_error(error: sqlx::Error) -> AppError {
        let code = classify(&error);
        AppError::new(code, public_message(code)).with_source(error)
    }
}

#[cfg(any(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
pub use sqlx_impl::{classify, classify_connect, to_app_error};

#[cfg(feature = "sea-orm")]
mod sea_orm_impl {
    use sea_orm::{DbErr, SqlErr};
    use sekvent_error::{AppError, ErrorCode};

    use super::public_message;

    /// The status code for a sea-orm error, consistent with
    /// [`classify`](crate::classify) for the sqlx errors it wraps.
    pub fn classify_db_err(error: &DbErr) -> ErrorCode {
        #[cfg(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql"))]
        if let DbErr::Conn(sea_orm::RuntimeErr::SqlxError(inner))
        | DbErr::Exec(sea_orm::RuntimeErr::SqlxError(inner))
        | DbErr::Query(sea_orm::RuntimeErr::SqlxError(inner)) = error
        {
            let code = crate::classify(inner);
            return if matches!(error, DbErr::Conn(_)) && code == ErrorCode::Internal {
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
    /// sea-orm error is kept only as the internal source.
    pub fn db_err_to_app_error(error: DbErr) -> AppError {
        let code = classify_db_err(&error);
        AppError::new(code, public_message(code)).with_source(error)
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

        use crate::{ConnectFailure, IntoAppError, classify, classify_connect, to_app_error};

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

        fn database(code: Option<&'static str>, kind: ErrorKind) -> sqlx::Error {
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
