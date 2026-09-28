//! Environment-driven configuration.
//!
//! Each service owns one config struct built from a [`ConfigSource`] (the
//! process environment in production, a [`MapSource`] in tests, since
//! `std::env::set_var` is `unsafe` in edition 2024). Reading is fail-fast: a
//! missing required value, a malformed value or — inside the reserved
//! `SEKVENT_` namespace — an unknown key is a startup error that names the
//! variable and never its value.
//!
//! The reader functions ([`req`], [`opt_parse`], [`req_secret`], …) are the
//! building blocks; `#[derive(EnvConfig)]` generates a [`FromConfig`] impl
//! out of them and reports every problem at once, the way [`collect`] does.
//!
//! # The reserved namespace
//!
//! Keys starting with [`RESERVED_PREFIX`] (`SEKVENT_`) belong to the
//! framework. [`from_env`] and [`load`] reject any such key that is set but
//! neither read by the config type nor listed in [`FRAMEWORK_KEYS`] or
//! [`FRAMEWORK_PREFIXES`], so a typo such as `SEKVENT_LOG_FORMT` fails at
//! startup instead of being ignored. The error names the key and, when one
//! is close, the key that was probably meant. Keys outside the namespace are
//! never checked: the process environment holds plenty of unrelated
//! variables.

#![forbid(unsafe_code)]

// Lets `#[derive(EnvConfig)]` name this crate `::sekvent_config` from inside
// it (unit tests and doctests).
extern crate self as sekvent_config;

mod secret;
mod source;

pub use secret::Secret;
pub use source::{ConfigSource, EnvSource, MapSource, Prefixed};

#[cfg(feature = "derive")]
pub use sekvent_macros::EnvConfig;

use std::str::FromStr;
use std::time::Duration;

/// The reserved key namespace. Unknown keys under it are rejected.
pub const RESERVED_PREFIX: &str = "SEKVENT_";

/// Keys under [`RESERVED_PREFIX`] that the framework reads itself, outside
/// any application config struct. [`load`] accepts them in every source.
///
/// | Key | Read by |
/// |---|---|
/// | `SEKVENT_LOG`, `SEKVENT_LOG_FORMAT` | `sekvent-telemetry`: log filter and output format |
/// | `SEKVENT_LINK_TRUSTED` | `sekvent-link`: links that may assert end-user identity |
/// | `SEKVENT_DOCKER_TESTS` | `sekvent-testing`: opt in to container-backed tests |
/// | `SEKVENT_HARNESS_NAMESPACE` | `sekvent-testing`: container label namespace |
/// | `SEKVENT_LOCAL`, `SEKVENT_NO_UPDATE_CHECK`, `SEKVENT_GIT_URL`, `SEKVENT_BRANCH` | `cargo sekvent` |
/// | `SEKVENT_SRC` | a generated workspace's `.sekvent/run.sh`: build the CLI from a checkout |
/// | `SEKVENT_INSTALL_DIR` | the install script |
///
/// Keep this list in step with the crates: a framework key missing here is
/// rejected as unknown by [`load`].
pub const FRAMEWORK_KEYS: &[&str] = &[
    "SEKVENT_BRANCH",
    "SEKVENT_DOCKER_TESTS",
    "SEKVENT_GIT_URL",
    "SEKVENT_HARNESS_NAMESPACE",
    "SEKVENT_INSTALL_DIR",
    "SEKVENT_LINK_TRUSTED",
    "SEKVENT_LOCAL",
    "SEKVENT_LOG",
    "SEKVENT_LOG_FORMAT",
    "SEKVENT_NO_UPDATE_CHECK",
    "SEKVENT_SRC",
];

/// Key families under [`RESERVED_PREFIX`] that the framework reads by
/// prefix. [`load`] accepts every key that starts with one of them.
///
/// - `SEKVENT_COMPONENT_`: component bindings and policy overrides
///   (`sekvent-component` validates the names against the installed
///   components);
/// - `SEKVENT_LINK_INBOUND_` and `SEKVENT_LINK_OUTBOUND_`: one service-link
///   token per link name (`sekvent-link` validates the names and tokens);
/// - `SEKVENT_TEST_`: test-only settings such as `SEKVENT_TEST_RUN_ID` and
///   `SEKVENT_TEST_POSTGRES_IMAGE` (`sekvent-testing`, `cargo sekvent`).
pub const FRAMEWORK_PREFIXES: &[&str] = &[
    "SEKVENT_COMPONENT_",
    "SEKVENT_LINK_INBOUND_",
    "SEKVENT_LINK_OUTBOUND_",
    "SEKVENT_TEST_",
];

/// Whether the framework itself reads `key` (a full key name), per
/// [`FRAMEWORK_KEYS`] and [`FRAMEWORK_PREFIXES`].
pub fn is_framework_key(key: &str) -> bool {
    FRAMEWORK_KEYS.contains(&key) || FRAMEWORK_PREFIXES.iter().any(|p| key.starts_with(p))
}

const DURATION_EXPECTED: &str = "a duration such as 500ms, 5s or 2m, or whole seconds";
const BOOL_EXPECTED: &str = "a boolean (true/false, 1/0, yes/no, on/off)";

/// A configuration error. Messages name keys, never values.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    /// A required key is unset.
    #[error("missing required configuration key {key}")]
    Missing {
        /// Full key name.
        key: String,
    },
    /// A secret key is set but blank or whitespace-only.
    #[error("configuration key {key} is set but empty; a secret needs a value")]
    EmptySecret {
        /// Full key name.
        key: String,
    },
    /// A value failed to parse.
    #[error("configuration key {key} is malformed: expected {expected}")]
    Malformed {
        /// Full key name.
        key: String,
        /// Human description of the expected shape, e.g. `a duration like 5s`.
        expected: String,
    },
    /// A value parsed but failed validation.
    #[error("configuration key {key} is invalid: {reason}")]
    Invalid {
        /// Full key name.
        key: String,
        /// Why (must not quote the value if it may be secret).
        reason: String,
    },
    /// Keys under [`RESERVED_PREFIX`] that no component declared.
    #[error(
        "unknown configuration keys under {RESERVED_PREFIX}: {}",
        unknown_list(keys, suggestions)
    )]
    UnknownKeys {
        /// The offending full key names, sorted.
        keys: Vec<String>,
        /// `(unknown key, closest known key)` for every unknown key with a
        /// close known match; probably a typo.
        suggestions: Vec<(String, String)>,
    },
    /// Several errors at once (all problems are reported, not just the first).
    #[error("{} configuration errors: {}", .0.len(), .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Multiple(Vec<ConfigError>),
}

/// A config struct that can be read from a [`ConfigSource`].
///
/// Usually derived with `#[derive(EnvConfig)]`.
pub trait FromConfig: Sized {
    /// Read and validate from `source`. Keys are relative to `source`
    /// (wrap it in [`Prefixed`] for scoping).
    fn from_config(source: &dyn ConfigSource) -> Result<Self, ConfigError>;

    /// The keys this type reads, relative to its source, for unknown-key
    /// detection and documentation.
    fn keys() -> Vec<KeyInfo>;
}

/// Description of one key a config type reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// Key name relative to the source.
    pub key: String,
    /// Whether the key must be present.
    pub required: bool,
    /// Whether the value is secret.
    pub secret: bool,
    /// Default rendered as text, if any (never for secrets).
    pub default: Option<String>,
    /// Doc comment of the field, if any.
    pub doc: Option<String>,
}

/// Read a type from the process environment, rejecting unknown keys under
/// [`RESERVED_PREFIX`]; see [`load`].
pub fn from_env<T: FromConfig>() -> Result<T, ConfigError> {
    load(&EnvSource)
}

/// Read a type from `source` and reject every key under [`RESERVED_PREFIX`]
/// that is set in `source` but read neither by `T` (per
/// [`FromConfig::keys`]) nor by the framework ([`is_framework_key`]).
///
/// Read failures and unknown keys are reported together. Use
/// [`FromConfig::from_config`] directly to read without the check, for
/// example for one of several config structs sharing a source; then run
/// [`check_unknown_keys`] once with every struct's keys.
pub fn load<T: FromConfig>(source: &dyn ConfigSource) -> Result<T, ConfigError> {
    let known: Vec<String> = T::keys()
        .iter()
        .map(|info| source.describe(&info.key))
        .collect();
    let reserved = check_unknown_keys(source, &known);
    match T::from_config(source) {
        Ok(value) => reserved.map(|()| value),
        Err(error) => {
            let mut errors = Vec::new();
            flatten_into(&mut errors, error);
            if let Err(error) = reserved {
                flatten_into(&mut errors, error);
            }
            Err(__private::finish(errors))
        }
    }
}

/// Required string.
pub fn req(source: &dyn ConfigSource, key: &str) -> Result<String, ConfigError> {
    source.get(key).ok_or_else(|| ConfigError::Missing {
        key: source.describe(key),
    })
}

/// Optional string. An empty value counts as set and is returned as `""`.
pub fn opt(source: &dyn ConfigSource, key: &str) -> Option<String> {
    source.get(key)
}

/// Required, parsed. A malformed value is an error (never silently defaulted).
pub fn req_parse<T: FromStr>(source: &dyn ConfigSource, key: &str) -> Result<T, ConfigError> {
    let raw = req(source, key)?;
    parse_raw(source, key, &raw)
}

/// Optional, parsed, with a default. An unset key yields the default; a set
/// but malformed value is an error.
pub fn opt_parse<T: FromStr>(
    source: &dyn ConfigSource,
    key: &str,
    default: T,
) -> Result<T, ConfigError> {
    match source.get(key) {
        None => Ok(default),
        Some(raw) => parse_raw(source, key, &raw),
    }
}

/// Required duration in humantime syntax (`500ms`, `5s`, `2m`) or bare
/// integer seconds.
pub fn req_duration(source: &dyn ConfigSource, key: &str) -> Result<Duration, ConfigError> {
    let raw = req(source, key)?;
    parse_duration(source, key, &raw)
}

/// Optional duration in humantime syntax (`500ms`, `5s`, `2m`) or bare
/// integer seconds, with a default.
pub fn opt_duration(
    source: &dyn ConfigSource,
    key: &str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    match source.get(key) {
        None => Ok(default),
        Some(raw) => parse_duration(source, key, &raw),
    }
}

/// Required boolean: `true/false/1/0/yes/no/on/off`, case-insensitive.
pub fn req_bool(source: &dyn ConfigSource, key: &str) -> Result<bool, ConfigError> {
    let raw = req(source, key)?;
    parse_bool(source, key, &raw)
}

/// Optional boolean: `true/false/1/0/yes/no/on/off`, case-insensitive.
pub fn opt_bool(source: &dyn ConfigSource, key: &str, default: bool) -> Result<bool, ConfigError> {
    match source.get(key) {
        None => Ok(default),
        Some(raw) => parse_bool(source, key, &raw),
    }
}

/// Required secret. Unset, empty or whitespace-only is an error.
pub fn req_secret(source: &dyn ConfigSource, key: &str) -> Result<Secret, ConfigError> {
    non_blank(source, key, Secret::new(req(source, key)?))
}

/// Optional secret. Unset is `None`; a set-but-blank value is an error, not
/// `None` — silently disabling a credential is worse than failing.
pub fn opt_secret(source: &dyn ConfigSource, key: &str) -> Result<Option<Secret>, ConfigError> {
    source
        .get(key)
        .map(|raw| non_blank(source, key, Secret::new(raw)))
        .transpose()
}

/// Reject keys under [`RESERVED_PREFIX`] present in `source` but absent from
/// `known` (full key names).
///
/// Keys are compared by their full name as reported by
/// [`ConfigSource::describe`], so a [`Prefixed`] view is checked against the
/// names an operator actually sets. The error lists the keys sorted and
/// suggests the closest entry of `known` for likely typos. Only `known` is
/// accepted here; [`check_unknown_keys`] accepts the framework's own keys
/// too.
pub fn check_reserved(source: &dyn ConfigSource, known: &[String]) -> Result<(), ConfigError> {
    unknown_reserved(source, known, &[])
}

/// [`check_reserved`] that also accepts the framework's own keys
/// ([`FRAMEWORK_KEYS`] and [`FRAMEWORK_PREFIXES`]); the check [`load`] runs.
///
/// `known` holds full key names, e.g. every config struct's
/// [`FromConfig::keys`] passed through [`ConfigSource::describe`].
pub fn check_unknown_keys(source: &dyn ConfigSource, known: &[String]) -> Result<(), ConfigError> {
    let mut all = known.to_vec();
    all.extend(FRAMEWORK_KEYS.iter().map(|key| (*key).to_owned()));
    unknown_reserved(source, &all, FRAMEWORK_PREFIXES)
}

fn unknown_reserved(
    source: &dyn ConfigSource,
    known: &[String],
    known_prefixes: &[&str],
) -> Result<(), ConfigError> {
    let mut unknown: Vec<String> = source
        .keys()
        .iter()
        .map(|key| source.describe(key))
        .filter(|full| {
            full.starts_with(RESERVED_PREFIX)
                && !known.contains(full)
                && !known_prefixes.iter().any(|prefix| full.starts_with(prefix))
        })
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();
    unknown.dedup();
    let mut candidates: Vec<&str> = known
        .iter()
        .map(String::as_str)
        .filter(|key| key.starts_with(RESERVED_PREFIX))
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    let suggestions = unknown
        .iter()
        .filter_map(|key| closest(key, &candidates).map(|near| (key.clone(), near.to_owned())))
        .collect();
    Err(ConfigError::UnknownKeys {
        keys: unknown,
        suggestions,
    })
}

/// The candidate closest to `key` by edit distance (a swap of two adjacent
/// characters counts once), if it is close enough to be a plausible typo:
/// at most one edit per three characters after [`RESERVED_PREFIX`], and at
/// least one. Ties go to the first candidate.
fn closest<'a>(key: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let tail = key.strip_prefix(RESERVED_PREFIX).unwrap_or(key);
    let limit = (tail.chars().count() / 3).max(1);
    let mut best: Option<(usize, &'a str)> = None;
    for &candidate in candidates {
        let distance = edit_distance(key, candidate);
        if distance <= limit && best.is_none_or(|(least, _)| distance < least) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// Optimal string alignment distance: insertions, deletions, substitutions
/// and adjacent transpositions each cost one.
fn edit_distance(from: &str, to: &str) -> usize {
    let from: Vec<char> = from.chars().collect();
    let to: Vec<char> = to.chars().collect();
    let mut table = vec![vec![0_usize; to.len() + 1]; from.len() + 1];
    for (i, row) in table.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in table[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=from.len() {
        for j in 1..=to.len() {
            let cost = usize::from(from[i - 1] != to[j - 1]);
            let mut distance = (table[i - 1][j] + 1)
                .min(table[i][j - 1] + 1)
                .min(table[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && from[i - 1] == to[j - 2] && from[i - 2] == to[j - 1] {
                distance = distance.min(table[i - 2][j - 2] + 1);
            }
            table[i][j] = distance;
        }
    }
    table[from.len()][to.len()]
}

fn unknown_list(keys: &[String], suggestions: &[(String, String)]) -> String {
    keys.iter()
        .map(
            |key| match suggestions.iter().find(|(unknown, _)| unknown == key) {
                Some((_, near)) => format!("{key} (did you mean {near}?)"),
                None => key.clone(),
            },
        )
        .collect::<Vec<_>>()
        .join(", ")
}

/// Merge the outcome of several independent checks into one result.
///
/// Every error is kept (nested [`ConfigError::Multiple`] values are
/// flattened). No error yields `Ok(())`, exactly one is returned as is, and
/// more than one become [`ConfigError::Multiple`].
pub fn collect(
    results: impl IntoIterator<Item = Result<(), ConfigError>>,
) -> Result<(), ConfigError> {
    let mut errors = Vec::new();
    for result in results {
        if let Err(error) = result {
            flatten_into(&mut errors, error);
        }
    }
    combine(errors).map_or(Ok(()), Err)
}

fn flatten_into(out: &mut Vec<ConfigError>, error: ConfigError) {
    match error {
        ConfigError::Multiple(inner) => {
            for error in inner {
                flatten_into(out, error);
            }
        }
        other => out.push(other),
    }
}

fn combine(mut errors: Vec<ConfigError>) -> Option<ConfigError> {
    match errors.len() {
        0 | 1 => errors.pop(),
        _ => Some(ConfigError::Multiple(errors)),
    }
}

fn non_blank(source: &dyn ConfigSource, key: &str, secret: Secret) -> Result<Secret, ConfigError> {
    if secret.is_blank() {
        Err(ConfigError::EmptySecret {
            key: source.describe(key),
        })
    } else {
        Ok(secret)
    }
}

fn malformed(source: &dyn ConfigSource, key: &str, expected: impl Into<String>) -> ConfigError {
    ConfigError::Malformed {
        key: source.describe(key),
        expected: expected.into(),
    }
}

fn parse_raw<T: FromStr>(
    source: &dyn ConfigSource,
    key: &str,
    raw: &str,
) -> Result<T, ConfigError> {
    raw.parse()
        .map_err(|_| malformed(source, key, type_label::<T>()))
}

fn parse_duration(
    source: &dyn ConfigSource,
    key: &str,
    raw: &str,
) -> Result<Duration, ConfigError> {
    duration_value(raw).ok_or_else(|| malformed(source, key, DURATION_EXPECTED))
}

fn parse_bool(source: &dyn ConfigSource, key: &str, raw: &str) -> Result<bool, ConfigError> {
    bool_value(raw).ok_or_else(|| malformed(source, key, BOOL_EXPECTED))
}

fn duration_value(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    humantime::parse_duration(raw).ok()
}

fn bool_value(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// A readable name for `T`: module paths are dropped, generic arguments are
/// kept (`alloc::vec::Vec<alloc::string::String>` becomes `Vec<String>`).
fn type_label<T>() -> String {
    short_type_name(std::any::type_name::<T>())
}

fn short_type_name(full: &str) -> String {
    let mut out = String::with_capacity(full.len());
    let mut rest = full;
    while let Some(pos) = rest.find("::") {
        let head = &rest[..pos];
        let keep = head
            .char_indices()
            .rev()
            .find(|&(_, c)| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        out.push_str(&head[..keep]);
        rest = &rest[pos + 2..];
    }
    out.push_str(rest);
    out
}

/// Support code for `#[derive(EnvConfig)]`. Not a stable API.
#[doc(hidden)]
pub mod __private {
    use std::str::FromStr;
    use std::time::Duration;

    use crate::{ConfigError, ConfigSource, combine, flatten_into};

    /// Keep the value of a successful read; record the error otherwise.
    pub fn take<T>(errors: &mut Vec<ConfigError>, result: Result<T, ConfigError>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                flatten_into(errors, error);
                None
            }
        }
    }

    /// The error to return once at least one read failed.
    pub fn finish(errors: Vec<ConfigError>) -> ConfigError {
        combine(errors).unwrap_or_else(|| ConfigError::Multiple(Vec::new()))
    }

    /// Optional parsed value without a default.
    pub fn opt_value<T: FromStr>(
        source: &dyn ConfigSource,
        key: &str,
    ) -> Result<Option<T>, ConfigError> {
        source
            .get(key)
            .map(|raw| crate::parse_raw(source, key, &raw))
            .transpose()
    }

    /// Optional duration without a default.
    pub fn opt_duration_value(
        source: &dyn ConfigSource,
        key: &str,
    ) -> Result<Option<Duration>, ConfigError> {
        source
            .get(key)
            .map(|raw| crate::parse_duration(source, key, &raw))
            .transpose()
    }

    /// Optional boolean without a default.
    pub fn opt_bool_value(
        source: &dyn ConfigSource,
        key: &str,
    ) -> Result<Option<bool>, ConfigError> {
        source
            .get(key)
            .map(|raw| crate::parse_bool(source, key, &raw))
            .transpose()
    }

    /// Parsed value falling back to a textual default.
    pub fn parse_or<T: FromStr>(
        source: &dyn ConfigSource,
        key: &str,
        default: &str,
    ) -> Result<T, ConfigError> {
        match source.get(key) {
            Some(raw) => crate::parse_raw(source, key, &raw),
            None => default
                .parse()
                .map_err(|_| bad_default(source, key, &crate::type_label::<T>())),
        }
    }

    /// Duration falling back to a textual default.
    pub fn duration_or(
        source: &dyn ConfigSource,
        key: &str,
        default: &str,
    ) -> Result<Duration, ConfigError> {
        match source.get(key) {
            Some(raw) => crate::parse_duration(source, key, &raw),
            None => crate::duration_value(default)
                .ok_or_else(|| bad_default(source, key, crate::DURATION_EXPECTED)),
        }
    }

    /// Run a field validator, turning its message into [`ConfigError::Invalid`].
    pub fn validate<T>(
        source: &dyn ConfigSource,
        key: &str,
        value: T,
        check: fn(&T) -> Result<(), String>,
    ) -> Result<T, ConfigError> {
        match check(&value) {
            Ok(()) => Ok(value),
            Err(reason) => Err(ConfigError::Invalid {
                key: source.describe(key),
                reason,
            }),
        }
    }

    fn bad_default(source: &dyn ConfigSource, key: &str, expected: &str) -> ConfigError {
        ConfigError::Invalid {
            key: source.describe(key),
            reason: format!("the declared default is not {expected}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(pairs: &[(&str, &str)]) -> MapSource {
        pairs.iter().copied().collect()
    }

    fn missing(key: &str) -> ConfigError {
        ConfigError::Missing { key: key.into() }
    }

    fn malformed_err(key: &str, expected: &str) -> ConfigError {
        ConfigError::Malformed {
            key: key.into(),
            expected: expected.into(),
        }
    }

    #[allow(clippy::trivially_copy_pass_by_ref)] // validators receive `&T` for any field type.
    fn positive(v: &u8) -> Result<(), String> {
        if *v > 0 {
            Ok(())
        } else {
            Err("must be positive".into())
        }
    }

    #[test]
    fn req_and_opt() {
        let s = src(&[("A", "x"), ("EMPTY", "")]);
        assert_eq!(req(&s, "A").unwrap(), "x");
        assert_eq!(req(&s, "EMPTY").unwrap(), "");
        assert_eq!(req(&s, "B"), Err(missing("B")));
        assert_eq!(opt(&s, "A").as_deref(), Some("x"));
        assert_eq!(opt(&s, "EMPTY").as_deref(), Some(""));
        assert_eq!(opt(&s, "B"), None);
    }

    #[test]
    fn parse_readers() {
        let s = src(&[("PORT", "8080"), ("BAD", "eighty")]);
        assert_eq!(req_parse::<u16>(&s, "PORT").unwrap(), 8080);
        assert_eq!(
            req_parse::<u16>(&s, "BAD"),
            Err(malformed_err("BAD", "u16"))
        );
        assert_eq!(req_parse::<u16>(&s, "NONE"), Err(missing("NONE")));
        assert_eq!(opt_parse::<u16>(&s, "PORT", 1).unwrap(), 8080);
        assert_eq!(opt_parse::<u16>(&s, "NONE", 1).unwrap(), 1);
        assert_eq!(
            opt_parse::<u16>(&s, "BAD", 1),
            Err(malformed_err("BAD", "u16"))
        );
        assert_eq!(
            req_parse::<std::net::SocketAddr>(&s, "BAD"),
            Err(malformed_err("BAD", "SocketAddr"))
        );
    }

    #[test]
    fn duration_readers() {
        let s = src(&[
            ("MS", "500ms"),
            ("SECS", " 7 "),
            ("BAD", "soon"),
            ("NEG", "-1"),
        ]);
        let d = Duration::from_secs(9);
        assert_eq!(
            opt_duration(&s, "MS", d).unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(opt_duration(&s, "SECS", d).unwrap(), Duration::from_secs(7));
        assert_eq!(opt_duration(&s, "NONE", d).unwrap(), d);
        assert_eq!(
            opt_duration(&s, "BAD", d),
            Err(malformed_err("BAD", DURATION_EXPECTED))
        );
        assert!(opt_duration(&s, "NEG", d).is_err());
        assert_eq!(req_duration(&s, "MS").unwrap(), Duration::from_millis(500));
        assert_eq!(req_duration(&s, "NONE"), Err(missing("NONE")));
        assert!(req_duration(&s, "BAD").is_err());
    }

    #[test]
    fn bool_readers() {
        for (raw, want) in [
            ("true", true),
            ("TRUE", true),
            ("1", true),
            ("Yes", true),
            ("on", true),
            ("false", false),
            ("0", false),
            ("NO", false),
            (" off ", false),
        ] {
            let s = src(&[("B", raw)]);
            assert_eq!(opt_bool(&s, "B", !want).unwrap(), want, "{raw}");
            assert_eq!(req_bool(&s, "B").unwrap(), want, "{raw}");
        }
        let s = src(&[("B", "maybe")]);
        assert_eq!(
            opt_bool(&s, "B", true),
            Err(malformed_err("B", BOOL_EXPECTED))
        );
        assert!(opt_bool(&s, "NONE", true).unwrap());
        assert_eq!(req_bool(&s, "NONE"), Err(missing("NONE")));
    }

    #[test]
    fn secret_readers() {
        let s = src(&[("TOKEN", "abc"), ("EMPTY", ""), ("BLANK", "  \t")]);
        assert_eq!(req_secret(&s, "TOKEN").unwrap().expose(), "abc");
        assert_eq!(req_secret(&s, "NONE").unwrap_err(), missing("NONE"));
        for key in ["EMPTY", "BLANK"] {
            let expected = ConfigError::EmptySecret { key: key.into() };
            assert_eq!(req_secret(&s, key).unwrap_err(), expected);
            assert_eq!(opt_secret(&s, key).unwrap_err(), expected);
        }
        assert_eq!(opt_secret(&s, "TOKEN").unwrap().unwrap().expose(), "abc");
        assert!(opt_secret(&s, "NONE").unwrap().is_none());
    }

    #[test]
    fn errors_use_full_prefixed_names() {
        let base = src(&[("BILLING_PORT", "x"), ("BILLING_TOKEN", " ")]);
        let scoped = Prefixed::new(&base, "BILLING_");
        assert_eq!(req(&scoped, "HOST"), Err(missing("BILLING_HOST")));
        assert_eq!(
            req_parse::<u16>(&scoped, "PORT"),
            Err(malformed_err("BILLING_PORT", "u16"))
        );
        assert_eq!(
            req_secret(&scoped, "TOKEN").unwrap_err(),
            ConfigError::EmptySecret {
                key: "BILLING_TOKEN".into()
            }
        );
        let nested = Prefixed::new(&scoped, "DB_");
        assert_eq!(req(&nested, "URL"), Err(missing("BILLING_DB_URL")));
    }

    #[test]
    fn messages_never_contain_values() {
        let s = src(&[("PORT", "s3cr3t-value"), ("TOKEN", "   ")]);
        let errors = [
            req_parse::<u16>(&s, "PORT").unwrap_err(),
            opt_duration(&s, "PORT", Duration::ZERO).unwrap_err(),
            opt_bool(&s, "PORT", false).unwrap_err(),
            req_secret(&s, "TOKEN").unwrap_err(),
        ];
        for error in errors {
            let text = error.to_string();
            assert!(!text.contains("s3cr3t"), "{text}");
            assert!(text.contains("PORT") || text.contains("TOKEN"), "{text}");
        }
    }

    #[test]
    fn check_reserved_reports_sorted_unknown_keys() {
        let s = src(&[
            ("SEKVENT_ZED", "1"),
            ("SEKVENT_LOG", "1"),
            ("SEKVENT_ALPHA", "1"),
            ("OTHER", "1"),
        ]);
        assert_eq!(
            check_reserved(&s, &["SEKVENT_LOG".into()]),
            Err(ConfigError::UnknownKeys {
                keys: vec!["SEKVENT_ALPHA".into(), "SEKVENT_ZED".into()],
                suggestions: Vec::new(),
            })
        );
        let all = ["SEKVENT_ZED", "SEKVENT_LOG", "SEKVENT_ALPHA"].map(String::from);
        assert_eq!(check_reserved(&s, &all), Ok(()));
        let err = check_reserved(&s, &[]).unwrap_err().to_string();
        assert!(
            err.contains("SEKVENT_ALPHA, SEKVENT_LOG, SEKVENT_ZED"),
            "{err}"
        );
    }

    #[test]
    fn check_reserved_uses_full_names_through_prefix() {
        let base = src(&[("SEKVENT_DB_URL", "x"), ("SEKVENT_DB_TYPO", "x")]);
        let scoped = Prefixed::new(&base, "SEKVENT_DB_");
        assert_eq!(
            check_reserved(&scoped, &["SEKVENT_DB_URL".into()]),
            Err(ConfigError::UnknownKeys {
                keys: vec!["SEKVENT_DB_TYPO".into()],
                suggestions: Vec::new(),
            })
        );
    }

    #[test]
    fn check_reserved_suggests_the_closest_known_key() {
        let s = src(&[
            ("SEKVENT_POTR", "9000"),
            ("SEKVENT_LGO", "debug"),
            ("SEKVENT_UNRELATED", "x"),
        ]);
        let known = ["SEKVENT_PORT", "SEKVENT_LOG", "SEKVENT_HOST", "OTHER_POTR"].map(String::from);
        let error = check_reserved(&s, &known).unwrap_err();
        assert_eq!(
            error,
            ConfigError::UnknownKeys {
                keys: ["SEKVENT_LGO", "SEKVENT_POTR", "SEKVENT_UNRELATED"]
                    .map(String::from)
                    .to_vec(),
                suggestions: vec![
                    ("SEKVENT_LGO".into(), "SEKVENT_LOG".into()),
                    ("SEKVENT_POTR".into(), "SEKVENT_PORT".into()),
                ],
            }
        );
        let text = error.to_string();
        assert_eq!(
            text,
            "unknown configuration keys under SEKVENT_: \
             SEKVENT_LGO (did you mean SEKVENT_LOG?), \
             SEKVENT_POTR (did you mean SEKVENT_PORT?), SEKVENT_UNRELATED"
        );
        assert!(!text.contains("9000") && !text.contains("debug"), "{text}");
    }

    #[test]
    fn framework_keys_are_recognised() {
        for key in FRAMEWORK_KEYS {
            assert!(key.starts_with(RESERVED_PREFIX), "{key}");
            assert!(is_framework_key(key), "{key}");
        }
        for prefix in FRAMEWORK_PREFIXES {
            assert!(prefix.starts_with(RESERVED_PREFIX), "{prefix}");
        }
        assert!(is_framework_key("SEKVENT_LINK_INBOUND_BILLING"));
        assert!(is_framework_key("SEKVENT_LINK_OUTBOUND_ORDERS"));
        assert!(is_framework_key("SEKVENT_TEST_RUN_ID"));
        assert!(is_framework_key("SEKVENT_TEST_POSTGRES_IMAGE"));
        assert!(is_framework_key("SEKVENT_COMPONENT_BINDING"));
        assert!(is_framework_key(
            "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT"
        ));
        assert!(!is_framework_key("SEKVENT_POTR"));
        assert!(!is_framework_key("SEKVENT_LOGS"));
        assert!(!is_framework_key("LOG"));
        assert!(FRAMEWORK_KEYS.is_sorted());
        assert!(FRAMEWORK_PREFIXES.is_sorted());
    }

    #[test]
    fn check_unknown_keys_accepts_the_framework_keys() {
        let s = src(&[
            ("SEKVENT_LOG", "info"),
            ("SEKVENT_TEST_RUN_ID", "7"),
            ("SEKVENT_LINK_OUTBOUND_BILLING", "x"),
            ("SEKVENT_APP_PORT", "80"),
        ]);
        assert_eq!(check_unknown_keys(&s, &["SEKVENT_APP_PORT".into()]), Ok(()));
        assert_eq!(
            check_unknown_keys(&s, &[]),
            Err(ConfigError::UnknownKeys {
                keys: vec!["SEKVENT_APP_PORT".into()],
                suggestions: Vec::new(),
            })
        );
        assert!(check_reserved(&s, &["SEKVENT_APP_PORT".into()]).is_err());
    }

    #[test]
    fn closest_needs_a_plausible_typo() {
        let candidates = ["SEKVENT_LOG", "SEKVENT_LOG_FORMAT", "SEKVENT_PORT"];
        assert_eq!(closest("SEKVENT_LOGS", &candidates), Some("SEKVENT_LOG"));
        assert_eq!(
            closest("SEKVENT_LOG_FROMAT", &candidates),
            Some("SEKVENT_LOG_FORMAT")
        );
        assert_eq!(closest("SEKVENT_PROT", &candidates), Some("SEKVENT_PORT"));
        assert_eq!(closest("SEKVENT_X", &candidates), None);
        assert_eq!(closest("SEKVENT_DATABASE", &candidates), None);
        assert_eq!(closest("SEKVENT_LOG", &[]), None);
        assert_eq!(
            closest("SEKVENT_AB", &["SEKVENT_AC", "SEKVENT_AD"]),
            Some("SEKVENT_AC")
        );
    }

    #[test]
    fn edit_distances() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("", "ab"), 2);
        assert_eq!(edit_distance("PORT", "PORT"), 0);
        assert_eq!(edit_distance("POTR", "PORT"), 1);
        assert_eq!(edit_distance("PORT", "PART"), 1);
        assert_eq!(edit_distance("PORT", "PORTS"), 1);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[derive(Debug)]
    struct Port(u16);

    impl FromConfig for Port {
        fn from_config(source: &dyn ConfigSource) -> Result<Self, ConfigError> {
            req_parse(source, "SEKVENT_PORT").map(Self)
        }
        fn keys() -> Vec<KeyInfo> {
            vec![KeyInfo {
                key: "SEKVENT_PORT".into(),
                required: true,
                secret: false,
                default: None,
                doc: None,
            }]
        }
    }

    #[test]
    fn load_rejects_unknown_reserved_keys() {
        let s = src(&[
            ("SEKVENT_PORT", "80"),
            ("SEKVENT_LOG", "debug"),
            ("SEKVENT_LINK_INBOUND_BILLING", "x"),
            ("SEKVENT_TEST_RUN_ID", "1"),
            ("UNRELATED", "x"),
        ]);
        assert_eq!(load::<Port>(&s).unwrap().0, 80);

        let typo = src(&[("SEKVENT_PORT", "80"), ("SEKVENT_POTR", "9000")]);
        assert_eq!(
            load::<Port>(&typo).unwrap_err(),
            ConfigError::UnknownKeys {
                keys: vec!["SEKVENT_POTR".into()],
                suggestions: vec![("SEKVENT_POTR".into(), "SEKVENT_PORT".into())],
            }
        );

        let near_framework = src(&[("SEKVENT_PORT", "80"), ("SEKVENT_LOG_FORMT", "json")]);
        let text = load::<Port>(&near_framework).unwrap_err().to_string();
        assert!(text.contains("did you mean SEKVENT_LOG_FORMAT?"), "{text}");
    }

    #[test]
    fn load_reports_read_errors_and_unknown_keys_together() {
        let s = src(&[("SEKVENT_POTR", "9000")]);
        assert_eq!(
            load::<Port>(&s).unwrap_err(),
            ConfigError::Multiple(vec![
                missing("SEKVENT_PORT"),
                ConfigError::UnknownKeys {
                    keys: vec!["SEKVENT_POTR".into()],
                    suggestions: vec![("SEKVENT_POTR".into(), "SEKVENT_PORT".into())],
                },
            ])
        );
        assert_eq!(
            load::<Port>(&MapSource::new()).unwrap_err(),
            missing("SEKVENT_PORT")
        );
    }

    #[test]
    fn load_compares_full_names_through_a_prefixed_source() {
        let base = src(&[
            ("SEKVENT_APP_SEKVENT_PORT", "80"),
            ("SEKVENT_APP_SEKVENT_PROT", "81"),
        ]);
        let scoped = Prefixed::new(&base, "SEKVENT_APP_");
        let error = load::<Port>(&scoped).unwrap_err();
        assert_eq!(
            error,
            ConfigError::UnknownKeys {
                keys: vec!["SEKVENT_APP_SEKVENT_PROT".into()],
                suggestions: vec![(
                    "SEKVENT_APP_SEKVENT_PROT".into(),
                    "SEKVENT_APP_SEKVENT_PORT".into()
                )],
            }
        );
    }

    #[cfg(feature = "derive")]
    #[test]
    fn the_derive_works_inside_this_crate() {
        #[derive(Debug, crate::EnvConfig)]
        #[config(prefix = "SEKVENT_")]
        struct Inner {
            #[config(default = "8080")]
            port: u16,
        }
        let config: Inner = load(&src(&[("SEKVENT_PORT", "81")])).unwrap();
        assert_eq!(config.port, 81);
        assert_eq!(Inner::keys()[0].key, "SEKVENT_PORT");
        let error = load::<Inner>(&src(&[("SEKVENT_PROT", "81")])).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("SEKVENT_PROT (did you mean SEKVENT_PORT?)"),
            "{error}"
        );
    }

    #[test]
    fn collect_merges_errors() {
        assert_eq!(collect([Ok(()), Ok(())]), Ok(()));
        assert_eq!(collect(Vec::new()), Ok(()));
        assert_eq!(collect([Ok(()), Err(missing("A"))]), Err(missing("A")));
        assert_eq!(
            collect([
                Err(missing("A")),
                Ok(()),
                Err(ConfigError::Multiple(vec![missing("B"), missing("C")])),
            ]),
            Err(ConfigError::Multiple(vec![
                missing("A"),
                missing("B"),
                missing("C")
            ]))
        );
        assert_eq!(
            collect([Err(ConfigError::Multiple(vec![missing("A")]))]),
            Err(missing("A"))
        );
    }

    #[test]
    fn display_of_every_variant() {
        let multiple = ConfigError::Multiple(vec![
            missing("A"),
            ConfigError::Invalid {
                key: "B".into(),
                reason: "too small".into(),
            },
        ]);
        assert_eq!(
            multiple.to_string(),
            "2 configuration errors: missing required configuration key A; \
             configuration key B is invalid: too small"
        );
        assert_eq!(
            ConfigError::EmptySecret { key: "T".into() }.to_string(),
            "configuration key T is set but empty; a secret needs a value"
        );
        assert_eq!(
            malformed_err("P", "u16").to_string(),
            "configuration key P is malformed: expected u16"
        );
    }

    #[test]
    fn short_type_names() {
        assert_eq!(short_type_name("u16"), "u16");
        assert_eq!(short_type_name("core::net::SocketAddr"), "SocketAddr");
        assert_eq!(
            short_type_name("alloc::vec::Vec<alloc::string::String>"),
            "Vec<String>"
        );
        assert_eq!(
            short_type_name("core::option::Option<(u8, std::path::PathBuf)>"),
            "Option<(u8, PathBuf)>"
        );
        assert_eq!(type_label::<String>(), "String");
    }

    #[test]
    fn private_readers() {
        use __private::{opt_bool_value, opt_duration_value, opt_value};
        let s = src(&[("N", "5"), ("BAD", "x"), ("D", "2s"), ("B", "on")]);
        assert_eq!(opt_value::<u8>(&s, "N").unwrap(), Some(5));
        assert_eq!(opt_value::<u8>(&s, "NONE").unwrap(), None);
        assert!(opt_value::<u8>(&s, "BAD").is_err());
        assert_eq!(
            opt_duration_value(&s, "D").unwrap(),
            Some(Duration::from_secs(2))
        );
        assert_eq!(opt_duration_value(&s, "NONE").unwrap(), None);
        assert!(opt_duration_value(&s, "BAD").is_err());
        assert_eq!(opt_bool_value(&s, "B").unwrap(), Some(true));
        assert_eq!(opt_bool_value(&s, "NONE").unwrap(), None);
        assert!(opt_bool_value(&s, "BAD").is_err());
    }

    #[test]
    fn private_defaults_and_validation() {
        use __private::{duration_or, parse_or, validate};
        let s = src(&[("N", "5"), ("BAD", "x"), ("D", "2s")]);
        assert_eq!(parse_or::<u8>(&s, "N", "1").unwrap(), 5);
        assert_eq!(parse_or::<u8>(&s, "NONE", "1").unwrap(), 1);
        assert!(parse_or::<u8>(&s, "BAD", "1").is_err());
        assert_eq!(
            parse_or::<u8>(&s, "NONE", "many"),
            Err(ConfigError::Invalid {
                key: "NONE".into(),
                reason: "the declared default is not u8".into()
            })
        );
        assert_eq!(duration_or(&s, "D", "1s").unwrap(), Duration::from_secs(2));
        assert_eq!(
            duration_or(&s, "NONE", "1s").unwrap(),
            Duration::from_secs(1)
        );
        assert!(duration_or(&s, "BAD", "1s").is_err());
        assert!(matches!(
            duration_or(&s, "NONE", "later"),
            Err(ConfigError::Invalid { .. })
        ));
        assert_eq!(validate(&s, "N", 3u8, positive).unwrap(), 3);
        assert_eq!(
            validate(&s, "N", 0u8, positive),
            Err(ConfigError::Invalid {
                key: "N".into(),
                reason: "must be positive".into()
            })
        );
    }

    #[test]
    fn private_error_accumulation() {
        use __private::{finish, take};
        let mut errors = Vec::new();
        assert_eq!(take(&mut errors, Ok::<_, ConfigError>(1)), Some(1));
        assert_eq!(take::<u8>(&mut errors, Err(missing("A"))), None);
        assert_eq!(finish(errors), missing("A"));

        let mut errors = Vec::new();
        take::<u8>(
            &mut errors,
            Err(ConfigError::Multiple(vec![missing("A"), missing("B")])),
        );
        take::<u8>(&mut errors, Err(missing("C")));
        assert_eq!(
            finish(errors),
            ConfigError::Multiple(vec![missing("A"), missing("B"), missing("C")])
        );
        assert_eq!(finish(Vec::new()), ConfigError::Multiple(Vec::new()));
    }

    struct Nothing;

    impl FromConfig for Nothing {
        fn from_config(source: &dyn ConfigSource) -> Result<Self, ConfigError> {
            let _ = source.keys();
            Ok(Self)
        }
        fn keys() -> Vec<KeyInfo> {
            Vec::new()
        }
    }

    #[test]
    fn from_env_reads_through_env_source() {
        // Only reserved keys can fail, and the test environment sets none
        // outside the framework's own.
        match from_env::<Nothing>() {
            Ok(Nothing) => {}
            Err(error) => panic!("{error}"),
        }
        assert!(Nothing::keys().is_empty());
    }
}
