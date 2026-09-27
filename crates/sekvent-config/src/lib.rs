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

#![forbid(unsafe_code)]

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
    #[error("unknown configuration keys under {RESERVED_PREFIX}: {}", keys.join(", "))]
    UnknownKeys {
        /// The offending full key names.
        keys: Vec<String>,
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

/// Read a type from the process environment.
pub fn from_env<T: FromConfig>() -> Result<T, ConfigError> {
    T::from_config(&EnvSource)
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
/// names an operator actually sets. The error lists the keys sorted.
pub fn check_reserved(source: &dyn ConfigSource, known: &[String]) -> Result<(), ConfigError> {
    let mut unknown: Vec<String> = source
        .keys()
        .iter()
        .map(|key| source.describe(key))
        .filter(|full| full.starts_with(RESERVED_PREFIX) && !known.contains(full))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();
    unknown.dedup();
    Err(ConfigError::UnknownKeys { keys: unknown })
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
                keys: vec!["SEKVENT_ALPHA".into(), "SEKVENT_ZED".into()]
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
                keys: vec!["SEKVENT_DB_TYPO".into()]
            })
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
        assert!(from_env::<Nothing>().is_ok());
        assert!(Nothing::keys().is_empty());
    }
}
