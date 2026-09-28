//! Behaviour of `#[derive(EnvConfig)]`, driven through `MapSource`.

use std::time::Duration;

use sekvent_config::{ConfigError, EnvConfig, FromConfig, KeyInfo, MapSource, Secret};

#[allow(clippy::trivially_copy_pass_by_ref)] // validators receive `&T` for any field type.
fn non_zero(port: &u16) -> Result<(), String> {
    if *port == 0 {
        Err("must not be zero".into())
    } else {
        Ok(())
    }
}

#[derive(Debug, EnvConfig)]
struct Database {
    /// Connection string, credentials included.
    #[config(secret)]
    url: Secret,
    /// Pool size.
    #[config(default = "10")]
    max_connections: u32,
}

#[derive(Debug, EnvConfig)]
#[config(prefix = "BILLING_")]
struct Billing {
    /// Port to listen on.
    #[config(default = "8080", validate = non_zero)]
    port: u16,
    #[config(key = "API_TOKEN")]
    token: Secret,
    webhook_secret: Option<Secret>,
    #[config(default = "5s")]
    timeout: Duration,
    retry_after: Option<Duration>,
    #[config(default = "off")]
    verbose: bool,
    dry_run: Option<bool>,
    region: Option<String>,
    #[config(nested)]
    db: Database,
}

fn source(pairs: &[(&str, &str)]) -> MapSource {
    pairs.iter().copied().collect()
}

fn minimal() -> MapSource {
    source(&[
        ("BILLING_API_TOKEN", "token-value"),
        ("BILLING_DB_URL", "postgres://example"),
    ])
}

fn missing(key: &str) -> ConfigError {
    ConfigError::Missing { key: key.into() }
}

#[test]
fn defaults_apply_when_keys_are_unset() {
    let config = Billing::from_config(&minimal()).expect("minimal config is valid");
    assert_eq!(config.port, 8080);
    assert_eq!(config.token.expose(), "token-value");
    assert!(config.webhook_secret.is_none());
    assert_eq!(config.timeout, Duration::from_secs(5));
    assert_eq!(config.retry_after, None);
    assert!(!config.verbose);
    assert_eq!(config.dry_run, None);
    assert_eq!(config.region, None);
    assert_eq!(config.db.url.expose(), "postgres://example");
    assert_eq!(config.db.max_connections, 10);
}

#[test]
fn every_value_is_read() {
    let mut map = minimal();
    for (key, value) in [
        ("BILLING_PORT", "9090"),
        ("BILLING_WEBHOOK_SECRET", "hook"),
        ("BILLING_TIMEOUT", "250ms"),
        ("BILLING_RETRY_AFTER", "30"),
        ("BILLING_VERBOSE", "YES"),
        ("BILLING_DRY_RUN", "0"),
        ("BILLING_REGION", ""),
        ("BILLING_DB_MAX_CONNECTIONS", "3"),
    ] {
        map.set(key, value);
    }
    let config = Billing::from_config(&map).expect("config is valid");
    assert_eq!(config.port, 9090);
    assert_eq!(
        config.webhook_secret.as_ref().map(Secret::expose),
        Some("hook")
    );
    assert_eq!(config.timeout, Duration::from_millis(250));
    assert_eq!(config.retry_after, Some(Duration::from_secs(30)));
    assert!(config.verbose);
    assert_eq!(config.dry_run, Some(false));
    assert_eq!(config.region.as_deref(), Some(""));
    assert_eq!(config.db.max_connections, 3);
}

#[test]
fn all_errors_are_reported_with_full_names() {
    let map = source(&[
        ("BILLING_PORT", "http"),
        ("BILLING_WEBHOOK_SECRET", "   "),
        ("BILLING_TIMEOUT", "soon"),
        ("BILLING_VERBOSE", "perhaps"),
        ("BILLING_DB_MAX_CONNECTIONS", "-1"),
    ]);
    let error = Billing::from_config(&map).expect_err("config is invalid");
    let ConfigError::Multiple(errors) = &error else {
        panic!("expected several errors, got {error:?}");
    };
    let keys: Vec<&str> = errors
        .iter()
        .map(|error| match error {
            ConfigError::Missing { key }
            | ConfigError::EmptySecret { key }
            | ConfigError::Malformed { key, .. }
            | ConfigError::Invalid { key, .. } => key.as_str(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        keys,
        [
            "BILLING_PORT",
            "BILLING_API_TOKEN",
            "BILLING_WEBHOOK_SECRET",
            "BILLING_TIMEOUT",
            "BILLING_VERBOSE",
            "BILLING_DB_URL",
            "BILLING_DB_MAX_CONNECTIONS",
        ]
    );
    assert_eq!(errors[1], missing("BILLING_API_TOKEN"));
    assert_eq!(
        errors[2],
        ConfigError::EmptySecret {
            key: "BILLING_WEBHOOK_SECRET".into()
        }
    );
    assert_eq!(
        errors[6],
        ConfigError::Malformed {
            key: "BILLING_DB_MAX_CONNECTIONS".into(),
            expected: "u32".into()
        }
    );
    let text = error.to_string();
    assert!(text.starts_with("7 configuration errors"), "{text}");
    assert!(
        !text.contains("http") && !text.contains("perhaps"),
        "{text}"
    );
}

#[test]
fn a_single_error_is_not_wrapped() {
    let map = source(&[("BILLING_API_TOKEN", "t")]);
    assert_eq!(
        Billing::from_config(&map).expect_err("db url is missing"),
        missing("BILLING_DB_URL")
    );
}

#[test]
fn validators_turn_failures_into_invalid() {
    let mut map = minimal();
    map.set("BILLING_PORT", "0");
    assert_eq!(
        Billing::from_config(&map).expect_err("port 0 is rejected"),
        ConfigError::Invalid {
            key: "BILLING_PORT".into(),
            reason: "must not be zero".into()
        }
    );
}

#[test]
fn keys_are_listed_prefixed_and_flattened() {
    let keys = Billing::keys();
    let names: Vec<&str> = keys.iter().map(|info| info.key.as_str()).collect();
    assert_eq!(
        names,
        [
            "BILLING_PORT",
            "BILLING_API_TOKEN",
            "BILLING_WEBHOOK_SECRET",
            "BILLING_TIMEOUT",
            "BILLING_RETRY_AFTER",
            "BILLING_VERBOSE",
            "BILLING_DRY_RUN",
            "BILLING_REGION",
            "BILLING_DB_URL",
            "BILLING_DB_MAX_CONNECTIONS",
        ]
    );
    assert_eq!(
        keys[0],
        KeyInfo {
            key: "BILLING_PORT".into(),
            required: false,
            secret: false,
            default: Some("8080".into()),
            doc: Some("Port to listen on.".into()),
        }
    );
    assert_eq!(
        keys[1],
        KeyInfo {
            key: "BILLING_API_TOKEN".into(),
            required: true,
            secret: true,
            default: None,
            doc: None,
        }
    );
    assert!(!keys[2].required && keys[2].secret);
    assert_eq!(keys[5].default.as_deref(), Some("off"));
    assert_eq!(
        keys[8],
        KeyInfo {
            key: "BILLING_DB_URL".into(),
            required: true,
            secret: true,
            default: None,
            doc: Some("Connection string, credentials included.".into()),
        }
    );
    assert_eq!(keys[9].default.as_deref(), Some("10"));
}

#[test]
fn debug_output_redacts_secrets() {
    let config = Billing::from_config(&minimal()).expect("minimal config is valid");
    let debug = format!("{config:?}");
    assert!(!debug.contains("token-value"), "{debug}");
    assert!(!debug.contains("postgres://"), "{debug}");
}

#[derive(Debug, EnvConfig)]
#[allow(non_snake_case)]
struct Required {
    enabled: bool,
    interval: Duration,
    r#type: String,
    #[config(key = "LISTEN")]
    listenAddr: std::net::SocketAddr,
}

#[test]
fn bools_and_durations_without_default_are_required() {
    let error = Required::from_config(&MapSource::new()).expect_err("all keys missing");
    assert_eq!(
        error,
        ConfigError::Multiple(vec![
            missing("ENABLED"),
            missing("INTERVAL"),
            missing("TYPE"),
            missing("LISTEN"),
        ])
    );
    let map = source(&[
        ("ENABLED", "on"),
        ("INTERVAL", "1m"),
        ("TYPE", "batch"),
        ("LISTEN", "127.0.0.1:80"),
    ]);
    let config = Required::from_config(&map).expect("all keys set");
    assert!(config.enabled);
    assert_eq!(config.interval, Duration::from_secs(60));
    assert_eq!(config.r#type, "batch");
    assert_eq!(config.listenAddr.port(), 80);
    assert!(
        Required::keys()
            .iter()
            .all(|info| info.required && info.default.is_none())
    );
}

#[derive(Debug, EnvConfig)]
struct BrokenDefaults {
    #[config(default = "later")]
    wait: Duration,
    #[config(default = "many")]
    count: u8,
}

#[test]
fn unparsable_defaults_are_reported_as_invalid() {
    let error = BrokenDefaults::from_config(&MapSource::new()).expect_err("defaults are bad");
    let ConfigError::Multiple(errors) = error else {
        panic!("expected two errors");
    };
    assert!(
        errors
            .iter()
            .all(|error| matches!(error, ConfigError::Invalid { .. }))
    );
    let config = BrokenDefaults::from_config(&source(&[("WAIT", "1s"), ("COUNT", "2")]))
        .expect("explicit values bypass the defaults");
    assert_eq!((config.wait, config.count), (Duration::from_secs(1), 2));
}

#[derive(Debug, EnvConfig)]
#[config(prefix = "EMPTY_")]
struct Nothing {}

#[test]
fn empty_struct_reads_nothing() {
    assert!(Nothing::from_config(&MapSource::new()).is_ok());
    assert!(Nothing::keys().is_empty());
}

#[derive(Debug, EnvConfig)]
struct Generic<T>
where
    T: std::str::FromStr,
{
    value: T,
}

#[test]
fn generic_structs_are_supported() {
    let config = Generic::<u64>::from_config(&source(&[("VALUE", "42")])).expect("valid");
    assert_eq!(config.value, 42);
    assert_eq!(Generic::<u64>::keys().len(), 1);
}

#[derive(Debug, EnvConfig)]
struct Outer {
    #[config(nested, key = "PRIMARY")]
    primary: Database,
    #[config(nested)]
    replica: Database,
}

#[test]
fn nested_errors_are_flattened_and_keys_use_the_field_key() {
    let error = Outer::from_config(&MapSource::new()).expect_err("urls are missing");
    assert_eq!(
        error,
        ConfigError::Multiple(vec![missing("PRIMARY_URL"), missing("REPLICA_URL")])
    );
    let config = Outer::from_config(&source(&[
        ("PRIMARY_URL", "p"),
        ("REPLICA_URL", "r"),
        ("REPLICA_MAX_CONNECTIONS", "2"),
    ]))
    .expect("both urls are set");
    assert_eq!(config.primary.url.expose(), "p");
    assert_eq!(config.primary.max_connections, 10);
    assert_eq!(config.replica.url.expose(), "r");
    assert_eq!(config.replica.max_connections, 2);
    let names: Vec<String> = Outer::keys().into_iter().map(|info| info.key).collect();
    assert_eq!(
        names,
        [
            "PRIMARY_URL",
            "PRIMARY_MAX_CONNECTIONS",
            "REPLICA_URL",
            "REPLICA_MAX_CONNECTIONS"
        ]
    );
}

#[test]
fn from_env_uses_the_derive() {
    // No process environment key starts with this prefix, so every key is
    // reported missing; the test never mutates the environment.
    #[derive(Debug, EnvConfig)]
    #[config(prefix = "SEKVENT_DERIVE_TEST_UNSET_")]
    struct FromEnv {
        value: u8,
    }
    assert_eq!(
        sekvent_config::from_env::<FromEnv>()
            .map(|config| config.value)
            .expect_err("unset"),
        missing("SEKVENT_DERIVE_TEST_UNSET_VALUE")
    );
}

#[derive(Debug, EnvConfig)]
#[config(prefix = "SEKVENT_APP_")]
struct Reserved {
    #[config(default = "1")]
    workers: u8,
    #[config(nested)]
    db: Database,
}

#[test]
fn load_checks_reserved_keys_by_their_full_names() {
    let mut map = source(&[("SEKVENT_APP_DB_URL", "postgres://example")]);
    map.set("SEKVENT_LOG", "debug");
    map.set("SEKVENT_APP_DB_MAX_CONNECTIONS", "3");
    let config = sekvent_config::load::<Reserved>(&map).expect("every key is known");
    assert_eq!((config.workers, config.db.max_connections), (1, 3));

    map.set("SEKVENT_APP_DB_MAX_CONECTIONS", "5");
    map.set("SEKVENT_APP_WORKRES", "2");
    assert_eq!(
        sekvent_config::load::<Reserved>(&map).expect_err("two typos"),
        ConfigError::UnknownKeys {
            keys: vec![
                "SEKVENT_APP_DB_MAX_CONECTIONS".into(),
                "SEKVENT_APP_WORKRES".into(),
            ],
            suggestions: vec![
                (
                    "SEKVENT_APP_DB_MAX_CONECTIONS".into(),
                    "SEKVENT_APP_DB_MAX_CONNECTIONS".into(),
                ),
                ("SEKVENT_APP_WORKRES".into(), "SEKVENT_APP_WORKERS".into()),
            ],
        }
    );
}

#[test]
fn load_leaves_keys_outside_the_reserved_namespace_alone() {
    let mut map = minimal();
    map.set("BILLING_UNUSED", "x");
    assert!(sekvent_config::load::<Billing>(&map).is_ok());
    map.set("SEKVENT_LOG_FORMT", "json");
    let error = sekvent_config::load::<Billing>(&map).expect_err("reserved typo");
    assert!(
        error
            .to_string()
            .ends_with("SEKVENT_LOG_FORMT (did you mean SEKVENT_LOG_FORMAT?)"),
        "{error}"
    );
}
