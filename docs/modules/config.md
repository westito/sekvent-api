# Configuration (`sekvent::config`)

`sekvent::config` turns environment variables into one typed, validated
config struct per service. Reading is fail-fast and fail-closed: a missing
required key, a malformed value, a blank secret or a misspelt key in the
reserved `SEKVENT_` namespace stops the service at startup with an error that
names the key and never its value. Every problem is reported at once, not
just the first. Values that must never be logged are held in `Secret`, which
formats as `[redacted]`.

## Enable it

| | |
|---|---|
| Facade feature | `config` (on by default) |
| Module | `use sekvent::config::…;` |
| Derive | `sekvent::EnvConfig` (also in `sekvent::prelude`) |
| Internal crate | `sekvent-config` (derive from `sekvent-macros`, behind its default `derive` feature) |

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master" }
```

`sekvent::prelude::*` brings `EnvConfig`, `FromConfig` and `Secret`.

## Quick example

```rust
use std::time::Duration;

use sekvent::prelude::*;

#[allow(clippy::trivially_copy_pass_by_ref)] // validators receive `&T`.
fn non_zero(port: &u16) -> Result<(), String> {
    if *port == 0 { Err("must not be zero".into()) } else { Ok(()) }
}

/// Settings of the billing service.
#[derive(Debug, EnvConfig)]
#[config(prefix = "BILLING_")]
pub struct BillingConfig {
    /// Port to listen on.
    #[config(default = "8080", validate = non_zero)]
    pub port: u16,
    /// Token presented to the orders service.
    #[config(key = "API_TOKEN")]
    pub token: Secret,
    /// Optional webhook signing secret.
    pub webhook_secret: Option<Secret>,
    /// Per-call timeout (`500ms`, `5s`, `2m` or whole seconds).
    #[config(default = "5s")]
    pub timeout: Duration,
    /// Extra logging.
    #[config(default = "off")]
    pub verbose: bool,
    /// Database settings, read under `BILLING_DB_`.
    #[config(nested)]
    pub db: DatabaseConfig,
}

/// Database settings.
#[derive(Debug, EnvConfig)]
pub struct DatabaseConfig {
    /// Connection string, credentials included.
    pub url: Secret,
    /// Pool size.
    #[config(default = "10")]
    pub max_connections: u32,
}

// In main: read the process environment and reject unknown SEKVENT_* keys.
let config: BillingConfig = sekvent::config::from_env()?;
```

That struct reads `BILLING_PORT`, `BILLING_API_TOKEN`,
`BILLING_WEBHOOK_SECRET`, `BILLING_TIMEOUT`, `BILLING_VERBOSE`,
`BILLING_DB_URL` and `BILLING_DB_MAX_CONNECTIONS`. `format!("{config:?}")`
never contains the token or the URL.

## Concepts

- **`ConfigSource`**: where raw values come from. `EnvSource` is the process
  environment, `MapSource` an in-memory map (tests, layered defaults),
  `Prefixed` a scoped view of another source.
- **`FromConfig`**: a type that reads itself from a source and lists the keys
  it reads (`keys()` returns `KeyInfo`s). `#[derive(EnvConfig)]` implements
  it.
- **Readers** (`req`, `opt_parse`, `req_secret`, …): the building blocks the
  derive uses; call them directly for hand-written config.
- **`ConfigError`**: every failure names the full key (prefix included) and
  never the value.
- **The reserved namespace**: keys starting with `SEKVENT_` belong to the
  framework. `load` and `from_env` reject a set `SEKVENT_*` key that neither
  your type nor the framework reads, and suggest the key you probably meant.
  Keys outside the namespace are never checked.

## How to declare a config struct with `#[derive(EnvConfig)]`

The derive works on structs with named fields (generic structs included;
unit structs, tuple structs, enums and unions are rejected at compile time).
The field type decides how a key is read:

| Field type | Read with | Unset key | Set key |
|---|---|---|---|
| `Secret` | `req_secret` | `Missing` | blank or whitespace-only is `EmptySecret` |
| `Option<Secret>` | `opt_secret` | `None` | blank is `EmptySecret` (never silently `None`) |
| `Duration` | `req_duration`, or the default | `Missing` unless a default is set | humantime (`250ms`, `5s`, `2m`, `1h 30m`) or bare whole seconds |
| `Option<Duration>` | | `None` | as above |
| `bool` | `req_bool`, or `opt_bool` with the default | `Missing` unless a default is set | `true/false/1/0/yes/no/on/off`, case-insensitive, trimmed |
| `Option<bool>` | | `None` | as above |
| `Option<T>` (`T: FromStr`) | | `None` | parsed with `FromStr`; an empty string is a set value |
| any other `T: FromStr` | `req_parse`, or the default | `Missing` unless a default is set | parsed with `FromStr` (`String`, integers, `SocketAddr`, …) |
| `#[config(nested)]` field | the nested type's `FromConfig` | per nested field | per nested field |

The type is matched by its last path segment, so `std::time::Duration` and
`Duration` both count.

### Attributes

On the struct:

| Attribute | Meaning |
|---|---|
| `#[config(prefix = "BILLING_")]` | Prepended verbatim to every key (include the separator). |
| `#[config(crate = "path")]` | Where the `sekvent_config` runtime lives. Rarely needed: the derive finds it through the `sekvent-api` dependency as `sekvent::config`, or through `sekvent-config` directly. |

On a field:

| Attribute | Meaning |
|---|---|
| `key = "API_TOKEN"` | Use this key instead of the field name. Must not be empty. |
| `default = "8080"` | Textual default used when the key is unset; parsed like a set value. |
| `secret` | Asserts the field is a secret; only allowed on `Secret` / `Option<Secret>`. A `Secret` field is secret without it. |
| `nested` | Read the field's type (itself `FromConfig`) under `<KEY>_`. |
| `validate = path::to_fn` | `fn(&T) -> Result<(), String>`, run after parsing; an `Err` becomes `ConfigError::Invalid` with that reason. |

Several attributes go in one list: `#[config(default = "8080", validate = non_zero)]`.

The default key is the field name in `UPPER_SNAKE_CASE`: `max_connections`
becomes `MAX_CONNECTIONS`, `listenAddr` becomes `LISTEN_ADDR`, and a raw
identifier `r#type` becomes `TYPE`. The field's doc comment becomes
`KeyInfo::doc`.

### Compile-time checks

The derive rejects at compile time:

- an unknown or repeated attribute (`expected one of key, default, secret, nested, validate`);
- `default` on a secret ("a secret cannot have a default; it must come from the environment");
- `default` on an `Option` field (it would have no effect);
- `default` together with `nested` (declare defaults on the nested fields);
- `secret` on a non-secret type, or together with `nested`;
- a `bool` default that is not one of the accepted words.

A default for any other type is parsed at runtime; one that does not parse
(`default = "later"` on a `Duration`) is reported as
`ConfigError::Invalid { reason: "the declared default is not …" }` when the
key is unset.

### Nested structs

```rust
#[derive(Debug, EnvConfig)]
struct Topology {
    #[config(nested, key = "PRIMARY")]
    primary: DatabaseConfig,   // PRIMARY_URL, PRIMARY_MAX_CONNECTIONS
    #[config(nested)]
    replica: DatabaseConfig,   // REPLICA_URL, REPLICA_MAX_CONNECTIONS
}
```

The nested prefix is the field key plus `_`, appended to the outer prefix.
Errors from nested structs are flattened into the outer report with their
full names.

## How to load configuration

| Function | Use |
|---|---|
| `from_env::<T>() -> Result<T, ConfigError>` | Production: `load(&EnvSource)`. |
| `load::<T>(source: &dyn ConfigSource) -> Result<T, ConfigError>` | Read `T` and reject unknown `SEKVENT_*` keys in `source`; read errors and unknown keys are reported together. |
| `T::from_config(source)` (`FromConfig`) | Read without the unknown-key check, e.g. one of several structs sharing a source. |
| `check_unknown_keys(source, known: &[String])` | The check `load` runs: accepts `known` (full names) plus the framework's keys. Run it once with every struct's keys when you read several structs with `from_config`. |
| `check_reserved(source, known)` | Like `check_unknown_keys` but accepts only `known`, not the framework keys. |
| `is_framework_key(key) -> bool` | Whether the framework itself reads `key`. |

Several structs from one source:

```rust
use sekvent::config::{self, ConfigSource, EnvSource, FromConfig};

let env = EnvSource;
let billing = BillingConfig::from_config(&env)?;
let orders = OrdersConfig::from_config(&env)?;
let known: Vec<String> = BillingConfig::keys()
    .into_iter()
    .chain(OrdersConfig::keys())
    .map(|info| env.describe(&info.key))
    .collect();
config::check_unknown_keys(&env, &known)?;
```

## How to read values by hand

The readers take a `&dyn ConfigSource` and a key relative to it. Errors name
the full key through `ConfigSource::describe`.

| Reader | Returns | Notes |
|---|---|---|
| `req(source, key)` | `Result<String, _>` | `Missing` when unset; an empty value is returned as `""`. |
| `opt(source, key)` | `Option<String>` | An empty value counts as set. |
| `req_parse::<T>(source, key)` | `Result<T, _>` | `T: FromStr`; `Malformed { expected: "u16" }` names the type. |
| `opt_parse(source, key, default)` | `Result<T, _>` | Unset gives the default; a malformed value is an error, never a silent default. |
| `req_duration(source, key)` / `opt_duration(source, key, default)` | `Result<Duration, _>` | humantime or whole seconds. |
| `req_bool(source, key)` / `opt_bool(source, key, default)` | `Result<bool, _>` | `true/false/1/0/yes/no/on/off`. |
| `req_secret(source, key)` | `Result<Secret, _>` | Unset is `Missing`, blank is `EmptySecret`. |
| `opt_secret(source, key)` | `Result<Option<Secret>, _>` | Unset is `None`, blank is `EmptySecret`. |
| `collect(results)` | `Result<(), ConfigError>` | Merge independent checks: no error is `Ok`, one is returned as is, more become `Multiple` (nested `Multiple`s are flattened). |

A hand-written `FromConfig`:

```rust
use std::time::Duration;

use sekvent::config::{self, ConfigError, ConfigSource, FromConfig, KeyInfo, Prefixed, Secret};

struct InventoryClient {
    base_url: String,
    api_key: Secret,
    timeout: Duration,
}

impl FromConfig for InventoryClient {
    fn from_config(source: &dyn ConfigSource) -> Result<Self, ConfigError> {
        let source = Prefixed::new(source, "INVENTORY_");
        let base_url = config::req(&source, "BASE_URL");
        let api_key = config::req_secret(&source, "API_KEY");
        let timeout = config::opt_duration(&source, "TIMEOUT", Duration::from_secs(2));
        match (base_url, api_key, timeout) {
            (Ok(base_url), Ok(api_key), Ok(timeout)) => Ok(Self { base_url, api_key, timeout }),
            (base_url, api_key, timeout) => Err(config::collect([
                base_url.map(drop),
                api_key.map(drop),
                timeout.map(drop),
            ])
            .expect_err("at least one read failed")),
        }
    }

    fn keys() -> Vec<KeyInfo> {
        let key = |key: &str, required, secret| KeyInfo {
            key: format!("INVENTORY_{key}"),
            required,
            secret,
            default: None,
            doc: None,
        };
        vec![
            key("BASE_URL", true, false),
            key("API_KEY", true, true),
            KeyInfo { default: Some("2s".into()), ..key("TIMEOUT", false, false) },
        ]
    }
}
```

## How to scope and layer sources

```rust
use sekvent::config::{ConfigSource, EnvSource, MapSource, Prefixed};

let env = EnvSource;
let billing = Prefixed::new(&env, "BILLING_");   // "DB_URL" reads BILLING_DB_URL
let db = Prefixed::new(&billing, "DB_");          // "URL" reads BILLING_DB_URL
assert_eq!(db.describe("URL"), "BILLING_DB_URL");
assert_eq!(billing.full_key("PORT"), "BILLING_PORT");
```

- `ConfigSource` (`trait ConfigSource: Send + Sync`, so an implementation
  must be shareable across threads) has three methods: `get(&self, key) -> Option<String>`,
  `keys(&self) -> Vec<String>` (every key set, for unknown-key detection) and
  `describe(&self, key) -> String` (the full name for error messages;
  defaults to the key itself). Implement it for other stores (a file, a
  secrets manager) and pass it to `load`.
- `Prefixed::keys()` returns only the keys under the prefix, with the prefix
  stripped; `describe` returns the full name through every layer.
- `MapSource::new()`, `.with(key, value)` (builder), `.set(key, value)`
  (replace), and `FromIterator<(K, V)>`:
  `let map: MapSource = [("A", "1")].into_iter().collect();`. Its `Debug`
  lists key names and the count, never values.

## How to handle secrets

```rust
use sekvent::config::Secret;

let token = Secret::new("t0ken");          // also From<String>, From<&str>, Default
assert_eq!(format!("{token}"), "[redacted]");
assert_eq!(format!("{token:?}"), "[redacted]");
let header = format!("Bearer {}", token.expose()); // every expose() is a review point
assert!(!token.is_blank());
```

`Secret` is `Clone` and zeroes its memory on drop. `expose()` is the only way
to the value; redaction does not make the exposed value safe to log anywhere
else.

## Configuration keys

Your own keys are whatever your structs declare. Under `SEKVENT_` the
framework reads or reserves these (`FRAMEWORK_KEYS`), and `load` accepts
them in every source:

| Key | Read by |
|---|---|
| `SEKVENT_LOG`, `SEKVENT_LOG_FORMAT` | telemetry: log filter and output format ([telemetry](telemetry.md)) |
| `SEKVENT_LINK_TRUSTED` | link: links that may assert end-user identity ([link](link.md)) |
| `SEKVENT_DOCKER_TESTS` | testing: opt in to container-backed tests ([testing](testing.md)) |
| `SEKVENT_HARNESS_NAMESPACE` | testing: container label namespace |
| `SEKVENT_LOCAL`, `SEKVENT_NO_UPDATE_CHECK` | `cargo sekvent` ([CLI](../cli.md)) |
| `SEKVENT_GIT_URL`, `SEKVENT_BRANCH` | nothing: reserved names with no runtime effect. `cargo sekvent` has the repository URL and branch built in and does not read these variables; `load` only accepts them so an environment that sets them passes the unknown-key check |
| `SEKVENT_SRC` | a generated workspace's `.sekvent/run.sh`: build the CLI from a checkout |
| `SEKVENT_INSTALL_DIR` | the install script |

Key families accepted by prefix (`FRAMEWORK_PREFIXES`); the owning crate
validates the rest of the name:

| Prefix | Owner |
|---|---|
| `SEKVENT_COMPONENT_` | component bindings and policy overrides ([components](components.md)) |
| `SEKVENT_LINK_INBOUND_`, `SEKVENT_LINK_OUTBOUND_` | one service-link token per link ([link](link.md)) |
| `SEKVENT_POLICY_` | named resilience policies ([components](components.md), [resilience](resilience.md)) |
| `SEKVENT_TEST_` | test-only settings, e.g. `SEKVENT_TEST_RUN_ID`, `SEKVENT_TEST_POSTGRES_IMAGE` |

Constants: `RESERVED_PREFIX` (`"SEKVENT_"`), `FRAMEWORK_KEYS`,
`FRAMEWORK_PREFIXES`.

A config struct may use the `SEKVENT_` namespace itself (for example
`#[config(prefix = "SEKVENT_APP_")]`); `load` then accepts exactly the keys
the struct reads and suggests the nearest one for a typo.

## Errors

`ConfigError` (`PartialEq`, `thiserror`); every message names keys, never
values:

| Variant | Message |
|---|---|
| `Missing { key }` | `missing required configuration key BILLING_DB_URL` |
| `EmptySecret { key }` | `configuration key … is set but empty; a secret needs a value` |
| `Malformed { key, expected }` | `configuration key … is malformed: expected u16` (or a duration / boolean description) |
| `Invalid { key, reason }` | `configuration key … is invalid: must not be zero` (a validator, or an unparsable declared default) |
| `UnknownKeys { keys, suggestions }` | `unknown configuration keys under SEKVENT_: SEKVENT_LOG_FORMT (did you mean SEKVENT_LOG_FORMAT?)` |
| `Multiple(Vec<ConfigError>)` | `7 configuration errors: …; …` (flat, in field order) |

A single failure is returned unwrapped, never as a one-element `Multiple`.
A suggestion is offered when the unknown key is within one edit per three
characters (after `SEKVENT_`) of a known one; an adjacent swap counts as one
edit.

A validator's reason is shown verbatim, so it must not quote the value when
the value may be secret.

## Testing tips

- Never `std::env::set_var` in tests (it is `unsafe` in edition 2024 and
  races other tests). Build a `MapSource` and call `T::from_config(&map)` or
  `sekvent::config::load::<T>(&map)`:

  ```rust
  use sekvent::config::{ConfigError, FromConfig, MapSource};

  let map = MapSource::new()
      .with("BILLING_API_TOKEN", "token-value")
      .with("BILLING_DB_URL", "postgres://example");
  let config = BillingConfig::from_config(&map).unwrap();
  assert_eq!(config.port, 8080);

  let error = BillingConfig::from_config(&MapSource::new()).unwrap_err();
  assert!(matches!(error, ConfigError::Multiple(_)));
  ```

- Assert `T::keys()` to pin the documented key set (names, `required`,
  `secret`, `default`, `doc`); it doubles as the source for an env template.
- Assert that `format!("{config:?}")` does not contain secret values.

## Pitfalls and security rules

- `Secret` is the only type that is redacted. A `String` field holding a
  credential (or a database URL with a password) is printed by `Debug`.
- Never log `secret.expose()`; never interpolate it into an error message.
- A set-but-blank secret fails rather than disabling the credential. Unset
  an optional secret to turn the feature off.
- An empty non-secret value is a set value: `""` for `String`,
  `Some("")` for `Option<String>`, and a parse error for numbers.
- Bare integers are seconds for durations: `TIMEOUT=30` is 30 s, not 30 ms.
- Do not invent `SEKVENT_*` keys for application settings unless your
  struct declares them; `load` rejects unknown ones.
- Reading with `from_config` skips the unknown-key check; use `load`,
  `from_env` or `check_unknown_keys` at least once at startup.

## See also

- [Getting started](../getting-started.md) and [features](../features.md)
- [Telemetry](telemetry.md) for `SEKVENT_LOG` / `SEKVENT_LOG_FORMAT`
- [Components](components.md) for `SEKVENT_COMPONENT_*` and `SEKVENT_POLICY_*`
- [Link](link.md) for `SEKVENT_LINK_*`
- [Server](server.md) and [DB](db.md) for their `from_config` helpers
- [Testing](testing.md)
