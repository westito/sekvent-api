use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::{ConnectFailure, DbError, PoolSpec, classify_connect};

/// One connected pool.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Pool {
    /// A sqlx Postgres pool.
    #[cfg(feature = "sqlx-postgres")]
    Postgres(sqlx::PgPool),
    /// A sqlx MySQL pool.
    #[cfg(feature = "sqlx-mysql")]
    MySql(sqlx::MySqlPool),
}

impl Pool {
    /// The Postgres pool, if this is one.
    #[cfg(feature = "sqlx-postgres")]
    pub fn postgres(&self) -> Option<&sqlx::PgPool> {
        match self {
            Self::Postgres(pool) => Some(pool),
            #[cfg(feature = "sqlx-mysql")]
            Self::MySql(_) => None,
        }
    }

    /// The MySQL pool, if this is one.
    #[cfg(feature = "sqlx-mysql")]
    pub fn mysql(&self) -> Option<&sqlx::MySqlPool> {
        match self {
            Self::MySql(pool) => Some(pool),
            #[cfg(feature = "sqlx-postgres")]
            Self::Postgres(_) => None,
        }
    }

    /// A sea-orm connection over the same pool (cheap: it shares the
    /// underlying connections). `None` when sea-orm support for this
    /// backend is not compiled in.
    #[cfg(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql"))]
    pub fn sea_orm(&self) -> Option<sea_orm::DatabaseConnection> {
        match self {
            #[cfg(feature = "sea-orm-postgres")]
            Self::Postgres(pool) => Some(sea_orm::DatabaseConnection::from(pool.clone())),
            #[cfg(all(feature = "sqlx-postgres", not(feature = "sea-orm-postgres")))]
            Self::Postgres(_) => None,
            #[cfg(feature = "sea-orm-mysql")]
            Self::MySql(pool) => Some(sea_orm::DatabaseConnection::from(pool.clone())),
            #[cfg(all(feature = "sqlx-mysql", not(feature = "sea-orm-mysql")))]
            Self::MySql(_) => None,
        }
    }

    /// Close every connection and wait for them to finish.
    pub async fn close(&self) {
        match self {
            #[cfg(feature = "sqlx-postgres")]
            Self::Postgres(pool) => pool.close().await,
            #[cfg(feature = "sqlx-mysql")]
            Self::MySql(pool) => pool.close().await,
        }
    }
}

/// Named pools, built once at startup.
///
/// ```ignore
/// let registry = PoolRegistry::build(vec![
///     PoolSpec::from_config(&env, "ORDERS_DB_")?,
///     PoolSpec::from_config(&env, "REPORTS_DB_")?,
/// ])
/// .await?;
/// let orders = registry.get("orders_db")?.postgres().expect("postgres");
/// ```
#[derive(Debug, Default)]
pub struct PoolRegistry {
    pools: BTreeMap<String, Pool>,
    unconfigured: BTreeSet<String>,
    migrations: BTreeMap<String, PathBuf>,
}

impl PoolRegistry {
    /// Build every pool.
    ///
    /// Fails, naming the pool, when two specs share a name, when two
    /// configured pools point at the same database, when a required pool has
    /// no URL, or when an eager pool cannot connect. An optional pool with a
    /// blank URL is recorded as not configured. A lazy pool opens no
    /// connection here.
    pub async fn build(specs: Vec<PoolSpec>) -> Result<Self, DbError> {
        let mut names = BTreeSet::new();
        for spec in &specs {
            if !names.insert(spec.name.as_str()) {
                return Err(DbError::DuplicatePool(spec.name.clone()));
            }
        }
        let urls: Vec<(&str, &sekvent_config::Secret)> = specs
            .iter()
            .map(|spec| (spec.name.as_str(), &spec.url))
            .collect();
        crate::assert_distinct_targets(&urls)?;

        let mut registry = Self::default();
        for spec in specs {
            if !spec.is_configured() {
                if spec.required {
                    return Err(DbError::NotConfigured(spec.name));
                }
                tracing::info!(pool = %spec.name, "optional database pool not configured");
                registry.unconfigured.insert(spec.name);
                continue;
            }
            let pool = connect(&spec).await?;
            if let Some(dir) = spec.migrations {
                registry.migrations.insert(spec.name.clone(), dir);
            }
            registry.pools.insert(spec.name, pool);
        }
        Ok(registry)
    }

    /// Register an already-built pool (tests, custom setups).
    pub fn insert(&mut self, name: impl Into<String>, pool: Pool) {
        let name = name.into();
        self.unconfigured.remove(&name);
        self.pools.insert(name, pool);
    }

    /// The pool called `name`. The error names the pool and says whether it
    /// is unknown or merely not configured.
    pub fn get(&self, name: &str) -> Result<&Pool, DbError> {
        if let Some(pool) = self.pools.get(name) {
            return Ok(pool);
        }
        if self.unconfigured.contains(name) {
            Err(DbError::NotConfigured(name.to_owned()))
        } else {
            Err(DbError::UnknownPool(name.to_owned()))
        }
    }

    /// The pool called `name`, or `None` when it is not configured or not
    /// declared.
    pub fn get_optional(&self, name: &str) -> Option<&Pool> {
        self.pools.get(name)
    }

    /// Whether `name` was declared optional and left without a URL.
    pub fn is_unconfigured(&self, name: &str) -> bool {
        self.unconfigured.contains(name)
    }

    /// Names of the connected pools, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.pools.keys().map(String::as_str)
    }

    /// The migrations directory declared for `name`, if any.
    pub fn migrations_dir(&self, name: &str) -> Option<&std::path::Path> {
        self.migrations.get(name).map(PathBuf::as_path)
    }

    /// Close every pool.
    pub async fn close(&self) {
        for pool in self.pools.values() {
            pool.close().await;
        }
    }

    /// Run each pool's declared migrations, in name order.
    #[cfg(feature = "migrate")]
    pub async fn migrate_all(&self) -> Result<(), DbError> {
        for (name, dir) in &self.migrations {
            let Some(pool) = self.pools.get(name) else {
                continue;
            };
            let result = match pool {
                #[cfg(feature = "sqlx-postgres")]
                Pool::Postgres(pool) => crate::migrate::migrate_on_boot(pool, dir).await,
                #[cfg(feature = "sqlx-mysql")]
                Pool::MySql(pool) => crate::migrate::migrate_on_boot(pool, dir).await,
            };
            result.map_err(|error| match error {
                DbError::Migrate { source, .. } => DbError::Migrate {
                    target: name.clone(),
                    source,
                },
                other => other,
            })?;
            tracing::info!(pool = %name, "migrations applied");
        }
        Ok(())
    }
}

/// Backend chosen from the URL scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Postgres,
    MySql,
}

#[cfg(not(all(feature = "sqlx-postgres", feature = "sqlx-mysql")))]
fn unsupported(spec: &PoolSpec, scheme: &str) -> DbError {
    DbError::UnsupportedScheme {
        pool: spec.name.clone(),
        scheme: scheme.to_owned(),
    }
}

fn backend_of(spec: &PoolSpec) -> Result<Backend, DbError> {
    let url = spec.url.expose().trim_start();
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .ok_or_else(|| DbError::BadUrl {
            pool: spec.name.clone(),
            reason: "missing scheme",
        })?;
    match scheme.as_str() {
        "postgres" | "postgresql" => Ok(Backend::Postgres),
        "mysql" | "mariadb" => Ok(Backend::MySql),
        _ => Err(DbError::UnsupportedScheme {
            pool: spec.name.clone(),
            scheme,
        }),
    }
}

fn connect_failed(spec: &PoolSpec, reason: ConnectFailure) -> DbError {
    // Only the name and the class: the driver's message can quote the URL.
    tracing::warn!(pool = %spec.name, reason = reason.as_str(), "database pool connect failed");
    DbError::Connect {
        pool: spec.name.clone(),
        reason,
    }
}

async fn connect(spec: &PoolSpec) -> Result<Pool, DbError> {
    spec.validate().map_err(|reason| DbError::InvalidSetting {
        pool: spec.name.clone(),
        reason,
    })?;
    let pool = match backend_of(spec)? {
        #[cfg(feature = "sqlx-postgres")]
        Backend::Postgres => connect_postgres(spec).await?,
        #[cfg(feature = "sqlx-mysql")]
        Backend::MySql => connect_mysql(spec).await?,
        #[cfg(not(feature = "sqlx-postgres"))]
        Backend::Postgres => {
            return Err(unsupported(spec, "postgres (enable feature sqlx-postgres)"));
        }
        #[cfg(not(feature = "sqlx-mysql"))]
        Backend::MySql => return Err(unsupported(spec, "mysql (enable feature sqlx-mysql)")),
    };
    tracing::info!(pool = %spec.name, lazy = spec.lazy, "database pool ready");
    Ok(pool)
}

fn pool_options<DB: sqlx::Database>(spec: &PoolSpec) -> sqlx::pool::PoolOptions<DB> {
    sqlx::pool::PoolOptions::<DB>::new()
        .max_connections(spec.max_connections)
        .min_connections(spec.min_connections)
        .acquire_timeout(spec.acquire_timeout)
        .idle_timeout(spec.idle_timeout)
        .max_lifetime(spec.max_lifetime)
}

/// Postgres startup parameters for the spec's session settings. Values are
/// validated identifiers, so they need no further escaping.
#[cfg(feature = "sqlx-postgres")]
fn postgres_session_options(spec: &PoolSpec) -> Vec<(&'static str, String)> {
    let mut options = Vec::new();
    if let Some(path) = &spec.search_path {
        let schemas: Vec<&str> = path.split(',').map(str::trim).collect();
        options.push(("search_path", schemas.join(",")));
    }
    if let Some(role) = &spec.role {
        options.push(("role", role.clone()));
    }
    if spec.read_only {
        options.push(("default_transaction_read_only", "on".to_owned()));
    }
    options
}

#[cfg(feature = "sqlx-postgres")]
async fn connect_postgres(spec: &PoolSpec) -> Result<Pool, DbError> {
    let options: sqlx::postgres::PgConnectOptions = spec
        .url
        .expose()
        .parse()
        .map_err(|error| connect_failed(spec, classify_connect(&error)))?;
    let session = postgres_session_options(spec);
    let options = if session.is_empty() {
        options
    } else {
        options.options(session)
    };
    let pool_options = pool_options::<sqlx::Postgres>(spec);
    let pool = if spec.lazy {
        pool_options.connect_lazy_with(options)
    } else {
        pool_options
            .connect_with(options)
            .await
            .map_err(|error| connect_failed(spec, classify_connect(&error)))?
    };
    Ok(Pool::Postgres(pool))
}

/// MySQL session statements for the spec's settings, run on every new
/// connection. Identifiers are validated and quoted.
#[cfg(feature = "sqlx-mysql")]
fn mysql_session_statements(spec: &PoolSpec) -> Result<Vec<String>, DbError> {
    if spec.search_path.is_some() {
        return Err(DbError::InvalidSetting {
            pool: spec.name.clone(),
            reason: "search path is Postgres-only; put the database in the MySQL URL".to_owned(),
        });
    }
    let mut statements = Vec::new();
    if let Some(role) = &spec.role {
        statements.push(format!("SET ROLE `{role}`"));
    }
    if spec.read_only {
        statements.push("SET SESSION TRANSACTION READ ONLY".to_owned());
    }
    Ok(statements)
}

#[cfg(feature = "sqlx-mysql")]
async fn connect_mysql(spec: &PoolSpec) -> Result<Pool, DbError> {
    let options: sqlx::mysql::MySqlConnectOptions = spec
        .url
        .expose()
        .parse()
        .map_err(|error| connect_failed(spec, classify_connect(&error)))?;
    let statements = std::sync::Arc::new(mysql_session_statements(spec)?);
    let mut pool_options = pool_options::<sqlx::MySql>(spec);
    if !statements.is_empty() {
        pool_options = pool_options.after_connect(move |conn, _meta| {
            let statements = std::sync::Arc::clone(&statements);
            Box::pin(async move {
                for statement in statements.iter() {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(statement.clone()))
                        .execute(&mut *conn)
                        .await?;
                }
                Ok(())
            })
        });
    }
    let pool = if spec.lazy {
        pool_options.connect_lazy_with(options)
    } else {
        pool_options
            .connect_with(options)
            .await
            .map_err(|error| connect_failed(spec, classify_connect(&error)))?
    };
    Ok(Pool::MySql(pool))
}

#[cfg(test)]
mod tests {
    use sekvent_config::Secret;

    use super::*;

    fn spec(name: &str, url: &str) -> PoolSpec {
        PoolSpec::new(name, Secret::new(url))
    }

    #[test]
    fn backends_follow_the_scheme() {
        assert_eq!(
            backend_of(&spec("a", "postgres://h/a")).unwrap(),
            Backend::Postgres
        );
        assert_eq!(
            backend_of(&spec("a", "PostgreSQL://h/a")).unwrap(),
            Backend::Postgres
        );
        assert_eq!(
            backend_of(&spec("a", "mysql://h/a")).unwrap(),
            Backend::MySql
        );
        assert_eq!(
            backend_of(&spec("a", "mariadb://h/a")).unwrap(),
            Backend::MySql
        );
        assert!(matches!(
            backend_of(&spec("a", "sqlite://x")).unwrap_err(),
            DbError::UnsupportedScheme { scheme, .. } if scheme == "sqlite"
        ));
        assert!(matches!(
            backend_of(&spec("a", "h/a")).unwrap_err(),
            DbError::BadUrl {
                reason: "missing scheme",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn duplicate_names_are_rejected() {
        let error = PoolRegistry::build(vec![
            spec("orders", "postgres://h/orders"),
            spec("orders", "postgres://h/audit"),
        ])
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "database pool `orders` is declared more than once"
        );
    }

    #[tokio::test]
    async fn pools_on_the_same_database_are_rejected() {
        let error = PoolRegistry::build(vec![
            spec("orders", "postgres://h/orders").lazy(),
            spec("audit", "postgres://h:5432/orders").lazy(),
        ])
        .await
        .unwrap_err();
        assert!(matches!(error, DbError::SameTarget { .. }));
    }

    #[tokio::test]
    async fn a_required_pool_without_url_fails_and_an_optional_one_is_skipped() {
        let error = PoolRegistry::build(vec![PoolSpec::new("orders", Secret::default())])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "database pool `orders` is not configured"
        );

        let registry =
            PoolRegistry::build(vec![PoolSpec::new("reports", Secret::new("")).optional()])
                .await
                .unwrap();
        assert!(registry.get_optional("reports").is_none());
        assert!(registry.is_unconfigured("reports"));
        assert!(
            matches!(registry.get("reports"), Err(DbError::NotConfigured(name)) if name == "reports")
        );
        assert!(matches!(registry.get("nope"), Err(DbError::UnknownPool(name)) if name == "nope"));
        assert_eq!(registry.names().count(), 0);
    }

    #[tokio::test]
    async fn invalid_settings_fail_before_connecting() {
        let error = PoolRegistry::build(vec![spec("a", "postgres://h/a").with_connections(3, 1)])
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "database pool `a`: min connections exceeds max connections"
        );
    }

    #[cfg(feature = "sqlx-postgres")]
    #[tokio::test]
    async fn lazy_postgres_pools_open_no_connection() {
        // Port 1 on a reserved documentation address: an eager connect would
        // fail, a lazy pool must not try.
        let registry = PoolRegistry::build(vec![
            spec("orders", "postgres://u:hunter2@192.0.2.1:1/orders")
                .lazy()
                .with_search_path("orders,public")
                .with_role("orders_rw")
                .read_only()
                .with_migrations("migrations"),
        ])
        .await
        .unwrap();
        let pool = registry.get("orders").unwrap();
        assert_eq!(pool.postgres().unwrap().size(), 0);
        assert_eq!(registry.names().collect::<Vec<_>>(), ["orders"]);
        assert_eq!(
            registry.migrations_dir("orders"),
            Some(std::path::Path::new("migrations"))
        );
        registry.close().await;
    }

    #[cfg(feature = "sqlx-postgres")]
    #[test]
    fn postgres_session_settings_become_startup_options() {
        let spec = spec("a", "postgres://h/a")
            .with_search_path("orders, public")
            .with_role("orders_rw")
            .read_only();
        assert_eq!(
            postgres_session_options(&spec),
            [
                ("search_path", "orders,public".to_owned()),
                ("role", "orders_rw".to_owned()),
                ("default_transaction_read_only", "on".to_owned()),
            ]
        );
        assert!(postgres_session_options(&self::spec("a", "postgres://h/a")).is_empty());
    }

    #[cfg(feature = "sqlx-postgres")]
    #[tokio::test]
    async fn an_unreachable_eager_pool_names_the_pool_and_class_only() {
        let error = PoolRegistry::build(vec![
            spec("orders", "postgres://u:hunter2@127.0.0.1:1/orders")
                .with_acquire_timeout(std::time::Duration::from_secs(2)),
        ])
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(message.starts_with("database pool `orders` failed to connect: "));
        assert!(!message.contains("hunter2") && !message.contains("127.0.0.1"));
        assert!(matches!(
            error,
            DbError::Connect {
                reason: ConnectFailure::Unreachable | ConnectFailure::Timeout,
                ..
            }
        ));
    }

    #[cfg(feature = "sqlx-postgres")]
    #[tokio::test]
    async fn a_malformed_postgres_url_is_a_bad_url() {
        let error = PoolRegistry::build(vec![spec("orders", "postgres://h:notaport/x").lazy()])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbError::BadUrl { .. }
                | DbError::Connect {
                    reason: ConnectFailure::BadUrl,
                    ..
                }
        ));
    }

    #[cfg(feature = "sqlx-mysql")]
    #[test]
    fn mysql_session_settings_become_statements() {
        let spec = spec("a", "mysql://h/a").with_role("reader").read_only();
        assert_eq!(
            mysql_session_statements(&spec).unwrap(),
            ["SET ROLE `reader`", "SET SESSION TRANSACTION READ ONLY"]
        );
        let error = mysql_session_statements(&self::spec("a", "mysql://h/a").with_search_path("x"))
            .unwrap_err();
        assert!(error.to_string().contains("Postgres-only"));
    }

    #[cfg(feature = "sqlx-mysql")]
    #[tokio::test]
    async fn lazy_mysql_pools_open_no_connection() {
        let registry = PoolRegistry::build(vec![
            spec("legacy", "mysql://u:hunter2@192.0.2.1:1/legacy")
                .lazy()
                .with_role("reader")
                .read_only(),
        ])
        .await
        .unwrap();
        let pool = registry.get("legacy").unwrap();
        assert_eq!(pool.mysql().unwrap().size(), 0);
        registry.close().await;
    }

    #[cfg(all(feature = "sqlx-postgres", feature = "sqlx-mysql"))]
    #[tokio::test]
    async fn typed_accessors_reject_the_other_backend() {
        let pg = sqlx::PgPool::connect_lazy("postgres://u@192.0.2.1:1/a").unwrap();
        let my = sqlx::MySqlPool::connect_lazy("mysql://u@192.0.2.1:1/a").unwrap();
        let mut registry = PoolRegistry::default();
        registry.insert("pg", Pool::Postgres(pg));
        registry.insert("my", Pool::MySql(my));
        assert!(registry.get("pg").unwrap().mysql().is_none());
        assert!(registry.get("my").unwrap().postgres().is_none());
        #[cfg(any(feature = "sea-orm-postgres", feature = "sea-orm-mysql"))]
        {
            let _ = registry.get("pg").unwrap().sea_orm();
            let _ = registry.get("my").unwrap().sea_orm();
        }
        registry.close().await;
    }
}
