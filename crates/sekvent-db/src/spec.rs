use std::path::PathBuf;
use std::time::Duration;

use sekvent_config::{ConfigError, ConfigSource, Secret};

const DEFAULT_MAX_CONNECTIONS: u32 = 10;
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_MAX_LIFETIME: Duration = Duration::from_mins(30);

/// The key suffixes [`PoolSpec::from_config`] reads.
const KEYS: [&str; 13] = [
    "URL",
    "MAX_CONNECTIONS",
    "MIN_CONNECTIONS",
    "ACQUIRE_TIMEOUT",
    "IDLE_TIMEOUT",
    "MAX_LIFETIME",
    "LAZY",
    "REQUIRED",
    "SEARCH_PATH",
    "SCHEMA",
    "ROLE",
    "READ_ONLY",
    "MIGRATIONS",
];

/// How to build one named pool.
///
/// `Debug` shows the URL as `[redacted]`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PoolSpec {
    /// Name used in errors, logs and [`PoolRegistry::get`](crate::PoolRegistry).
    pub name: String,
    /// Connection URL; blank means "not configured".
    pub url: Secret,
    /// Upper bound of open connections.
    pub max_connections: u32,
    /// Connections kept open even when idle.
    pub min_connections: u32,
    /// How long `acquire` waits for a connection.
    pub acquire_timeout: Duration,
    /// Idle connections are closed after this long; `None` keeps them.
    pub idle_timeout: Option<Duration>,
    /// Connections are recycled after this long; `None` keeps them.
    pub max_lifetime: Option<Duration>,
    /// Open no connection at startup; connect on first use.
    pub lazy: bool,
    /// A required pool without a URL is a startup error; an optional one is
    /// skipped and reported as not configured.
    pub required: bool,
    /// Postgres `search_path` (comma-separated schema names). For MySQL, set
    /// the database in the URL instead.
    pub search_path: Option<String>,
    /// Role assumed on every connection (`SET ROLE`).
    pub role: Option<String>,
    /// Make every transaction read-only by default. A hint for replicas; it
    /// is not a security boundary.
    pub read_only: bool,
    /// Directory of sqlx migrations to run on boot.
    pub migrations: Option<PathBuf>,
}

impl PoolSpec {
    /// A required, eager pool with default limits: 10 connections, 5 s
    /// acquire timeout, 10 min idle timeout, 30 min maximum lifetime.
    pub fn new(name: impl Into<String>, url: Secret) -> Self {
        Self {
            name: name.into(),
            url,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            min_connections: 0,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
            idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
            max_lifetime: Some(DEFAULT_MAX_LIFETIME),
            lazy: false,
            required: true,
            search_path: None,
            role: None,
            read_only: false,
            migrations: None,
        }
    }

    /// Whether a URL is set.
    pub fn is_configured(&self) -> bool {
        !self.url.is_blank()
    }

    /// Mark the pool optional.
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    /// Connect lazily.
    #[must_use]
    pub fn lazy(mut self) -> Self {
        self.lazy = true;
        self
    }

    /// Set the connection limits.
    #[must_use]
    pub fn with_connections(mut self, min: u32, max: u32) -> Self {
        self.min_connections = min;
        self.max_connections = max;
        self
    }

    /// Set the acquire timeout.
    #[must_use]
    pub fn with_acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }

    /// Set the idle timeout and maximum lifetime.
    #[must_use]
    pub fn with_lifetimes(mut self, idle: Option<Duration>, max: Option<Duration>) -> Self {
        self.idle_timeout = idle;
        self.max_lifetime = max;
        self
    }

    /// Set the Postgres `search_path`.
    #[must_use]
    pub fn with_search_path(mut self, search_path: impl Into<String>) -> Self {
        self.search_path = Some(search_path.into());
        self
    }

    /// Set the role.
    #[must_use]
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }

    /// Make transactions read-only by default.
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Run the migrations in `dir` on boot.
    #[must_use]
    pub fn with_migrations(mut self, dir: impl Into<PathBuf>) -> Self {
        self.migrations = Some(dir.into());
        self
    }

    /// Check limits and identifiers. Names the pool and the setting, never
    /// the value of the URL.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_connections == 0 {
            return Err("max connections must be at least 1".to_owned());
        }
        if self.min_connections > self.max_connections {
            return Err("min connections exceeds max connections".to_owned());
        }
        if let Some(path) = &self.search_path
            && !path.split(',').map(str::trim).all(is_identifier)
        {
            return Err(
                "search path must be a comma-separated list of plain identifiers".to_owned(),
            );
        }
        if let Some(role) = &self.role
            && !is_identifier(role)
        {
            return Err("role must be a plain identifier".to_owned());
        }
        Ok(())
    }

    /// Read a spec from `source`, with every key under `prefix`:
    ///
    /// | key | default |
    /// |---|---|
    /// | `<P>URL` | required unless `<P>REQUIRED=false` |
    /// | `<P>MAX_CONNECTIONS` | 10 |
    /// | `<P>MIN_CONNECTIONS` | 0 |
    /// | `<P>ACQUIRE_TIMEOUT` | 5s |
    /// | `<P>IDLE_TIMEOUT` | 10m (`0` disables) |
    /// | `<P>MAX_LIFETIME` | 30m (`0` disables) |
    /// | `<P>LAZY` | false |
    /// | `<P>REQUIRED` | true |
    /// | `<P>SEARCH_PATH` or `<P>SCHEMA` | unset |
    /// | `<P>ROLE` | unset |
    /// | `<P>READ_ONLY` | false |
    /// | `<P>MIGRATIONS` | unset |
    ///
    /// The pool is named after the prefix: `BILLING_DB_` gives `billing_db`.
    /// An optional pool whose URL is unset or blank is returned with an empty
    /// URL, which the registry treats as not configured. All problems are
    /// reported together.
    pub fn from_config(source: &dyn ConfigSource, prefix: &str) -> Result<Self, ConfigError> {
        let key = |suffix: &str| format!("{prefix}{suffix}");
        let mut errors = Vec::new();
        let mut spec = Self::new(pool_name(prefix), Secret::default());

        spec.required = collect(
            &mut errors,
            sekvent_config::opt_bool(source, &key("REQUIRED"), true),
        )
        .unwrap_or(true);
        match source.get(&key("URL")) {
            Some(url) if !url.trim().is_empty() => spec.url = Secret::new(url),
            Some(_) if spec.required => errors.push(ConfigError::EmptySecret {
                key: source.describe(&key("URL")),
            }),
            None if spec.required => errors.push(ConfigError::Missing {
                key: source.describe(&key("URL")),
            }),
            _ => {}
        }
        if let Some(max) = collect(
            &mut errors,
            sekvent_config::opt_parse(source, &key("MAX_CONNECTIONS"), DEFAULT_MAX_CONNECTIONS),
        ) {
            spec.max_connections = max;
        }
        if let Some(min) = collect(
            &mut errors,
            sekvent_config::opt_parse(source, &key("MIN_CONNECTIONS"), 0),
        ) {
            spec.min_connections = min;
        }
        if let Some(timeout) = collect(
            &mut errors,
            sekvent_config::opt_duration(source, &key("ACQUIRE_TIMEOUT"), DEFAULT_ACQUIRE_TIMEOUT),
        ) {
            spec.acquire_timeout = timeout;
        }
        if let Some(idle) = collect(
            &mut errors,
            sekvent_config::opt_duration(source, &key("IDLE_TIMEOUT"), DEFAULT_IDLE_TIMEOUT),
        ) {
            spec.idle_timeout = non_zero(idle);
        }
        if let Some(lifetime) = collect(
            &mut errors,
            sekvent_config::opt_duration(source, &key("MAX_LIFETIME"), DEFAULT_MAX_LIFETIME),
        ) {
            spec.max_lifetime = non_zero(lifetime);
        }
        spec.lazy = collect(
            &mut errors,
            sekvent_config::opt_bool(source, &key("LAZY"), false),
        )
        .unwrap_or(false);
        spec.read_only = collect(
            &mut errors,
            sekvent_config::opt_bool(source, &key("READ_ONLY"), false),
        )
        .unwrap_or(false);
        spec.search_path = non_blank(sekvent_config::opt(source, &key("SEARCH_PATH")))
            .or_else(|| non_blank(sekvent_config::opt(source, &key("SCHEMA"))));
        spec.role = non_blank(sekvent_config::opt(source, &key("ROLE")));
        spec.migrations =
            non_blank(sekvent_config::opt(source, &key("MIGRATIONS"))).map(PathBuf::from);

        if errors.is_empty()
            && let Err(reason) = spec.validate()
        {
            errors.push(ConfigError::Invalid {
                key: source.describe(&key(invalid_key(&spec))),
                reason,
            });
        }
        match errors.len() {
            0 => Ok(spec),
            1 => Err(errors.remove(0)),
            _ => Err(ConfigError::Multiple(errors)),
        }
    }

    /// Every key [`PoolSpec::from_config`] reads under `prefix`, for
    /// unknown-key checks and documentation.
    pub fn config_keys(prefix: &str) -> Vec<String> {
        KEYS.iter()
            .map(|suffix| format!("{prefix}{suffix}"))
            .collect()
    }
}

fn collect<T>(errors: &mut Vec<ConfigError>, result: Result<T, ConfigError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

fn non_zero(duration: Duration) -> Option<Duration> {
    (!duration.is_zero()).then_some(duration)
}

fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The key a validation failure is reported against.
fn invalid_key(spec: &PoolSpec) -> &'static str {
    if spec.max_connections == 0 {
        "MAX_CONNECTIONS"
    } else if spec.min_connections > spec.max_connections {
        "MIN_CONNECTIONS"
    } else if spec
        .role
        .as_deref()
        .is_some_and(|role| !is_identifier(role))
    {
        "ROLE"
    } else {
        "SEARCH_PATH"
    }
}

/// `BILLING_DB_` becomes `billing_db`; an empty prefix gives `default`.
fn pool_name(prefix: &str) -> String {
    let name = prefix.trim_matches('_').to_ascii_lowercase();
    if name.is_empty() {
        "default".to_owned()
    } else {
        name
    }
}

/// A plain SQL identifier: ASCII letter or `_`, then letters, digits, `_`
/// or `$`, at most 63 bytes. Such names are safe to embed quoted.
pub(crate) fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 63
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

#[cfg(test)]
mod tests {
    use sekvent_config::MapSource;

    use super::*;

    #[test]
    fn defaults_are_required_and_eager() {
        let spec = PoolSpec::new("orders", Secret::new("postgres://h/orders"));
        assert!(spec.required);
        assert!(!spec.lazy);
        assert!(spec.is_configured());
        assert_eq!(spec.max_connections, 10);
        assert_eq!(spec.min_connections, 0);
        assert_eq!(spec.acquire_timeout, Duration::from_secs(5));
        assert_eq!(spec.idle_timeout, Some(Duration::from_secs(600)));
        assert_eq!(spec.max_lifetime, Some(Duration::from_mins(30)));
        assert!(spec.validate().is_ok());
        assert!(!format!("{spec:?}").contains("postgres://"));
    }

    #[test]
    fn builders_set_every_field() {
        let spec = PoolSpec::new("orders", Secret::default())
            .optional()
            .lazy()
            .with_connections(2, 20)
            .with_acquire_timeout(Duration::from_secs(1))
            .with_lifetimes(None, Some(Duration::from_secs(60)))
            .with_search_path("orders, public")
            .with_role("orders_rw")
            .read_only()
            .with_migrations("migrations/orders");
        assert!(!spec.required && spec.lazy && spec.read_only);
        assert!(!spec.is_configured());
        assert_eq!((spec.min_connections, spec.max_connections), (2, 20));
        assert_eq!(spec.acquire_timeout, Duration::from_secs(1));
        assert_eq!(spec.idle_timeout, None);
        assert_eq!(spec.max_lifetime, Some(Duration::from_secs(60)));
        assert_eq!(spec.search_path.as_deref(), Some("orders, public"));
        assert_eq!(spec.role.as_deref(), Some("orders_rw"));
        assert_eq!(spec.migrations, Some(PathBuf::from("migrations/orders")));
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validation_rejects_bad_limits_and_identifiers() {
        let base = || PoolSpec::new("a", Secret::new("postgres://h/a"));
        assert!(base().with_connections(0, 0).validate().is_err());
        assert!(base().with_connections(5, 2).validate().is_err());
        assert!(
            base()
                .with_search_path("a; DROP TABLE x")
                .validate()
                .is_err()
        );
        assert!(base().with_search_path("a,,b").validate().is_err());
        assert!(base().with_role("r\"x").validate().is_err());
        assert!(base().with_role("r".repeat(64)).validate().is_err());
    }

    #[test]
    fn identifiers_are_plain() {
        assert!(is_identifier("orders_v2$"));
        assert!(is_identifier("_x"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("1a"));
        assert!(!is_identifier("a-b"));
    }

    #[test]
    fn pool_names_come_from_the_prefix() {
        assert_eq!(pool_name("BILLING_DB_"), "billing_db");
        assert_eq!(pool_name(""), "default");
        assert_eq!(pool_name("_"), "default");
    }

    #[test]
    fn config_keys_cover_every_setting() {
        let keys = PoolSpec::config_keys("ORDERS_DB_");
        assert_eq!(keys.len(), 13);
        assert!(keys.contains(&"ORDERS_DB_URL".to_owned()));
        assert!(keys.contains(&"ORDERS_DB_MIGRATIONS".to_owned()));
    }

    #[test]
    fn reads_every_key_from_config() {
        let source = MapSource::new()
            .with("ORDERS_DB_URL", "postgres://u:p@db/orders")
            .with("ORDERS_DB_MAX_CONNECTIONS", "25")
            .with("ORDERS_DB_MIN_CONNECTIONS", "3")
            .with("ORDERS_DB_ACQUIRE_TIMEOUT", "2s")
            .with("ORDERS_DB_IDLE_TIMEOUT", "0")
            .with("ORDERS_DB_MAX_LIFETIME", "5m")
            .with("ORDERS_DB_LAZY", "true")
            .with("ORDERS_DB_SEARCH_PATH", "orders")
            .with("ORDERS_DB_ROLE", "orders_ro")
            .with("ORDERS_DB_READ_ONLY", "yes")
            .with("ORDERS_DB_MIGRATIONS", "migrations");
        let spec = PoolSpec::from_config(&source, "ORDERS_DB_").unwrap();
        assert_eq!(spec.name, "orders_db");
        assert_eq!(spec.url.expose(), "postgres://u:p@db/orders");
        assert_eq!((spec.min_connections, spec.max_connections), (3, 25));
        assert_eq!(spec.acquire_timeout, Duration::from_secs(2));
        assert_eq!(spec.idle_timeout, None);
        assert_eq!(spec.max_lifetime, Some(Duration::from_secs(300)));
        assert!(spec.lazy && spec.read_only && spec.required);
        assert_eq!(spec.search_path.as_deref(), Some("orders"));
        assert_eq!(spec.role.as_deref(), Some("orders_ro"));
        assert_eq!(spec.migrations, Some(PathBuf::from("migrations")));
    }

    #[test]
    fn schema_is_an_alias_for_search_path() {
        let source = MapSource::new()
            .with("DB_URL", "postgres://db/x")
            .with("DB_SCHEMA", "tenant_a");
        let spec = PoolSpec::from_config(&source, "DB_").unwrap();
        assert_eq!(spec.search_path.as_deref(), Some("tenant_a"));
    }

    #[test]
    fn a_required_pool_needs_a_url() {
        let missing = PoolSpec::from_config(&MapSource::new(), "DB_").unwrap_err();
        assert_eq!(
            missing,
            ConfigError::Missing {
                key: "DB_URL".to_owned()
            }
        );
        let blank =
            PoolSpec::from_config(&MapSource::new().with("DB_URL", "  "), "DB_").unwrap_err();
        assert_eq!(
            blank,
            ConfigError::EmptySecret {
                key: "DB_URL".to_owned()
            }
        );
    }

    #[test]
    fn an_optional_pool_may_be_unconfigured() {
        for source in [
            MapSource::new().with("REPORTS_DB_REQUIRED", "false"),
            MapSource::new()
                .with("REPORTS_DB_REQUIRED", "false")
                .with("REPORTS_DB_URL", ""),
        ] {
            let spec = PoolSpec::from_config(&source, "REPORTS_DB_").unwrap();
            assert!(!spec.required);
            assert!(!spec.is_configured());
        }
    }

    #[test]
    fn every_problem_is_reported_and_values_are_not() {
        let source = MapSource::new()
            .with("DB_URL", "postgres://user:hunter2@db/x")
            .with("DB_MAX_CONNECTIONS", "many")
            .with("DB_LAZY", "maybe");
        let error = PoolSpec::from_config(&source, "DB_").unwrap_err();
        let ConfigError::Multiple(errors) = &error else {
            panic!("expected several errors, got {error:?}");
        };
        assert_eq!(errors.len(), 2);
        assert!(!error.to_string().contains("hunter2"));
    }

    #[test]
    fn invalid_combinations_name_the_key() {
        let cases = [
            ("DB_MAX_CONNECTIONS", "0", "DB_MAX_CONNECTIONS"),
            ("DB_MIN_CONNECTIONS", "50", "DB_MIN_CONNECTIONS"),
            ("DB_ROLE", "a b", "DB_ROLE"),
            ("DB_SEARCH_PATH", "a;b", "DB_SEARCH_PATH"),
        ];
        for (key, value, named) in cases {
            let source = MapSource::new()
                .with("DB_URL", "postgres://db/x")
                .with(key, value);
            match PoolSpec::from_config(&source, "DB_").unwrap_err() {
                ConfigError::Invalid { key, .. } => assert_eq!(key, named),
                other => panic!("expected an invalid-value error, got {other:?}"),
            }
        }
    }
}
