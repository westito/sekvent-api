use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sekvent_config::{ConfigError, ConfigSource};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    Backoff, BreakerWindow, Bulkhead, CircuitBreaker, CircuitBreakerConfig, Jitter, Policy,
    PolicyError, RateGate, RetryBudget, RetryPolicy, Timeout,
};

/// The breaker window in a [`PolicySpec`]: a number of calls or a duration.
///
/// As text, bare digits are a call count (`20`) and anything else is a
/// humantime duration (`30s`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerWindowSpec {
    /// The last N calls.
    Calls(u32),
    /// The calls of the last duration.
    Duration(Duration),
}

impl BreakerWindowSpec {
    fn to_window(self) -> BreakerWindow {
        match self {
            Self::Calls(size) => BreakerWindow::Count { size },
            Self::Duration(duration) => BreakerWindow::Time { duration },
        }
    }
}

impl FromStr for BreakerWindowSpec {
    type Err = PolicyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            return value.parse().map(Self::Calls).map_err(|_| {
                PolicyError::new("breaker.window", "call count does not fit in 32 bits")
            });
        }
        humantime::parse_duration(value)
            .map(Self::Duration)
            .map_err(|_| {
                PolicyError::new(
                    "breaker.window",
                    "expected a call count or a duration like 30s",
                )
            })
    }
}

impl fmt::Display for BreakerWindowSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Calls(calls) => write!(f, "{calls}"),
            Self::Duration(duration) => write!(f, "{}", humantime::format_duration(*duration)),
        }
    }
}

impl Serialize for BreakerWindowSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Calls(calls) => serializer.serialize_u32(*calls),
            Self::Duration(_) => serializer.collect_str(self),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NumberOrText {
    Number(u64),
    Text(String),
}

impl<'de> Deserialize<'de> for BreakerWindowSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match NumberOrText::deserialize(deserializer)? {
            NumberOrText::Number(calls) => u32::try_from(calls)
                .map(Self::Calls)
                .map_err(|_| serde::de::Error::custom("call count does not fit in 32 bits")),
            NumberOrText::Text(text) => text.parse().map_err(serde::de::Error::custom),
        }
    }
}

mod humantime_opt {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    use super::{NumberOrText, parse_duration};

    #[allow(clippy::ref_option)]
    pub(super) fn serialize<S: Serializer>(
        value: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(duration) => serializer.collect_str(&humantime::format_duration(*duration)),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        Ok(match Option::<NumberOrText>::deserialize(deserializer)? {
            None => None,
            Some(NumberOrText::Number(secs)) => Some(Duration::from_secs(secs)),
            Some(NumberOrText::Text(text)) => Some(
                parse_duration(&text)
                    .ok_or_else(|| serde::de::Error::custom("expected a duration like 5s"))?,
            ),
        })
    }
}

/// Humantime (`500ms`, `5s`, `2m`) or bare integer seconds.
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    humantime::parse_duration(value).ok()
}

/// A serializable, layerable description of a [`Policy`].
///
/// Every field is optional; an unset field means "inherit". Layers combine
/// with [`PolicySpec::resolve`], where a later layer overrides only the
/// fields it sets. The intended precedence, lowest first, is: attribute
/// default, named policy, component override, method override.
///
/// Durations serialize as humantime strings (`"1s 500ms"`) and accept bare
/// integer seconds.
///
/// A component is enabled by its key field: retries by `retry_max_attempts`
/// above one, the bulkhead by `bulkhead_max_concurrent`, the breaker by
/// `breaker_failure_rate` (or `breaker_enabled = true`, which also turns
/// it off when `false`), the rate gate by `rate_limit_permits`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct PolicySpec {
    /// Per-attempt timeout (also capped by the call deadline).
    #[serde(with = "humantime_opt", skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Total attempts, the first included; 1 disables retries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_max_attempts: Option<u32>,
    /// First backoff delay.
    #[serde(with = "humantime_opt", skip_serializing_if = "Option::is_none")]
    pub retry_initial_backoff: Option<Duration>,
    /// Backoff cap.
    #[serde(with = "humantime_opt", skip_serializing_if = "Option::is_none")]
    pub retry_max_backoff: Option<Duration>,
    /// Backoff growth factor, at least 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_multiplier: Option<f64>,
    /// Backoff jitter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_jitter: Option<Jitter>,
    /// Retry tokens earned per success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_budget_ratio: Option<f64>,
    /// Retries always allowed per second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_budget_min_per_sec: Option<u32>,
    /// Maximum concurrent calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bulkhead_max_concurrent: Option<u32>,
    /// Callers allowed to wait for a slot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bulkhead_max_queue: Option<u32>,
    /// Longest wait for a slot.
    #[serde(with = "humantime_opt", skip_serializing_if = "Option::is_none")]
    pub bulkhead_queue_timeout: Option<Duration>,
    /// Explicitly enable or disable the breaker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaker_enabled: Option<bool>,
    /// Failure fraction that opens the breaker (`0 < rate <= 1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaker_failure_rate: Option<f64>,
    /// Breaker sliding window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaker_window: Option<BreakerWindowSpec>,
    /// Calls needed before the failure rate is evaluated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaker_min_calls: Option<u32>,
    /// How long the breaker stays open.
    #[serde(with = "humantime_opt", skip_serializing_if = "Option::is_none")]
    pub breaker_wait_in_open: Option<Duration>,
    /// Probe calls while half-open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaker_permitted_in_half_open: Option<u32>,
    /// Permits per rate window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_permits: Option<u32>,
    /// Rate window length.
    #[serde(with = "humantime_opt", skip_serializing_if = "Option::is_none")]
    pub rate_limit_window: Option<Duration>,
}

macro_rules! overlay {
    ($base:expr, $over:expr, [$($field:ident),* $(,)?]) => {
        $( $base.$field = $over.$field.or($base.$field); )*
    };
}

impl PolicySpec {
    /// Every configuration key suffix read by [`PolicySpec::from_config`].
    pub const CONFIG_KEYS: &'static [&'static str] = &[
        "TIMEOUT",
        "RETRY_MAX_ATTEMPTS",
        "RETRY_INITIAL_BACKOFF",
        "RETRY_MAX_BACKOFF",
        "RETRY_MULTIPLIER",
        "RETRY_JITTER",
        "RETRY_BUDGET_RATIO",
        "RETRY_BUDGET_MIN_PER_SEC",
        "BULKHEAD_MAX_CONCURRENT",
        "BULKHEAD_MAX_QUEUE",
        "BULKHEAD_QUEUE_TIMEOUT",
        "BREAKER_ENABLED",
        "BREAKER_FAILURE_RATE",
        "BREAKER_WINDOW",
        "BREAKER_MIN_CALLS",
        "BREAKER_WAIT_IN_OPEN",
        "BREAKER_PERMITTED_IN_HALF_OPEN",
        "RATE_LIMIT_PERMITS",
        "RATE_LIMIT_WINDOW",
    ];

    /// A copy of `self` with every field that `over` sets replaced.
    #[must_use]
    pub fn overlay(&self, over: &PolicySpec) -> PolicySpec {
        let mut merged = self.clone();
        overlay!(
            merged,
            over,
            [
                timeout,
                retry_max_attempts,
                retry_initial_backoff,
                retry_max_backoff,
                retry_multiplier,
                retry_jitter,
                retry_budget_ratio,
                retry_budget_min_per_sec,
                bulkhead_max_concurrent,
                bulkhead_max_queue,
                bulkhead_queue_timeout,
                breaker_enabled,
                breaker_failure_rate,
                breaker_window,
                breaker_min_calls,
                breaker_wait_in_open,
                breaker_permitted_in_half_open,
                rate_limit_permits,
                rate_limit_window,
            ]
        );
        merged
    }

    /// Fold layers in order, lowest precedence first:
    /// `PolicySpec::resolve([&defaults, &named, &component, &method])`.
    pub fn resolve<'a>(layers: impl IntoIterator<Item = &'a PolicySpec>) -> PolicySpec {
        layers
            .into_iter()
            .fold(PolicySpec::default(), |merged, layer| merged.overlay(layer))
    }

    /// Read a spec from `source`, keys prefixed with `prefix` (for example
    /// `ORDERS_TIMEOUT` for prefix `ORDERS_`). Unset keys stay unset. Every
    /// malformed or invalid key is reported; messages name keys, never values.
    pub fn from_config(source: &dyn ConfigSource, prefix: &str) -> Result<PolicySpec, ConfigError> {
        let mut reader = Reader {
            source,
            prefix,
            errors: Vec::new(),
        };
        let spec = PolicySpec {
            timeout: reader.duration("TIMEOUT"),
            retry_max_attempts: reader.count("RETRY_MAX_ATTEMPTS", 1),
            retry_initial_backoff: reader.duration("RETRY_INITIAL_BACKOFF"),
            retry_max_backoff: reader.duration("RETRY_MAX_BACKOFF"),
            retry_multiplier: reader.number("RETRY_MULTIPLIER", |value| {
                (value.is_finite() && value >= 1.0)
                    .then_some(value)
                    .ok_or("must be at least 1")
            }),
            retry_jitter: reader.parsed("RETRY_JITTER", "one of none, full, equal"),
            retry_budget_ratio: reader.number("RETRY_BUDGET_RATIO", |value| {
                (value.is_finite() && (0.0..=1000.0).contains(&value))
                    .then_some(value)
                    .ok_or("must be between 0 and 1000")
            }),
            retry_budget_min_per_sec: reader.count("RETRY_BUDGET_MIN_PER_SEC", 0),
            bulkhead_max_concurrent: reader.count("BULKHEAD_MAX_CONCURRENT", 1),
            bulkhead_max_queue: reader.count("BULKHEAD_MAX_QUEUE", 0),
            bulkhead_queue_timeout: reader.duration("BULKHEAD_QUEUE_TIMEOUT"),
            breaker_enabled: reader.boolean("BREAKER_ENABLED"),
            breaker_failure_rate: reader.rate("BREAKER_FAILURE_RATE"),
            breaker_window: reader.window("BREAKER_WINDOW"),
            breaker_min_calls: reader.count("BREAKER_MIN_CALLS", 1),
            breaker_wait_in_open: reader.duration("BREAKER_WAIT_IN_OPEN"),
            breaker_permitted_in_half_open: reader.count("BREAKER_PERMITTED_IN_HALF_OPEN", 1),
            rate_limit_permits: reader.count("RATE_LIMIT_PERMITS", 1),
            rate_limit_window: reader.duration("RATE_LIMIT_WINDOW"),
        };
        let mut errors = reader.errors;
        match errors.len() {
            0 => Ok(spec),
            1 => Err(errors.remove(0)),
            _ => Err(ConfigError::Multiple(errors)),
        }
    }

    /// Build a live [`Policy`] named `name`. Stateful parts (budget, rate
    /// gate, bulkhead, breaker) are created fresh, so build once per
    /// dependency and share the result.
    pub fn build(&self, name: impl Into<String>) -> Result<Policy, PolicyError> {
        let name = name.into();
        let mut policy = Policy::new(name.clone()).with_timeout(
            self.timeout
                .map_or_else(Timeout::deadline_only, Timeout::new),
        );

        let attempts = self.retry_max_attempts.unwrap_or(1);
        if attempts == 0 {
            return Err(PolicyError::new("retry.max_attempts", "must be at least 1"));
        }
        if attempts > 1 {
            let defaults = Backoff::default();
            let backoff = Backoff::new(
                self.retry_initial_backoff.unwrap_or(defaults.initial()),
                self.retry_max_backoff.unwrap_or(defaults.max()),
                self.retry_multiplier.unwrap_or(defaults.multiplier()),
                self.retry_jitter.unwrap_or(defaults.jitter()),
            )?;
            let budget = RetryBudget::new(
                self.retry_budget_ratio.unwrap_or(0.2),
                self.retry_budget_min_per_sec.unwrap_or(10),
            )?;
            policy = policy
                .with_retry(RetryPolicy::new(attempts, backoff).with_budget(Arc::new(budget)));
        }

        if let Some(max_concurrent) = self.bulkhead_max_concurrent {
            let queue = self.bulkhead_max_queue.unwrap_or(0);
            let wait = self.bulkhead_queue_timeout.unwrap_or(if queue > 0 {
                Duration::from_secs(1)
            } else {
                Duration::ZERO
            });
            policy = policy.with_bulkhead(Arc::new(
                Bulkhead::new(max_concurrent)?.with_queue(queue, wait),
            ));
        }

        if self
            .breaker_enabled
            .unwrap_or(self.breaker_failure_rate.is_some())
        {
            let defaults = CircuitBreakerConfig::default();
            let config = CircuitBreakerConfig {
                window: self
                    .breaker_window
                    .map_or(defaults.window, BreakerWindowSpec::to_window),
                failure_rate: self.breaker_failure_rate.unwrap_or(defaults.failure_rate),
                min_calls: self.breaker_min_calls.unwrap_or(defaults.min_calls),
                wait_in_open: self.breaker_wait_in_open.unwrap_or(defaults.wait_in_open),
                permitted_in_half_open: self
                    .breaker_permitted_in_half_open
                    .unwrap_or(defaults.permitted_in_half_open),
            };
            policy = policy.with_breaker(Arc::new(CircuitBreaker::new(name, config)?));
        }

        if let Some(permits) = self.rate_limit_permits {
            let window = self.rate_limit_window.unwrap_or(Duration::from_secs(1));
            policy = policy.with_rate_gate(Arc::new(RateGate::new(permits, window)?));
        }

        Ok(policy)
    }
}

struct Reader<'a> {
    source: &'a dyn ConfigSource,
    prefix: &'a str,
    errors: Vec<ConfigError>,
}

impl Reader<'_> {
    /// The raw value and the display name of `suffix`.
    fn raw(&self, suffix: &str) -> Option<(String, String)> {
        let key = format!("{}{suffix}", self.prefix);
        self.source
            .get(&key)
            .map(|value| (value, self.source.describe(&key)))
    }

    fn malformed(&mut self, key: String, expected: &str) {
        self.errors.push(ConfigError::Malformed {
            key,
            expected: expected.to_owned(),
        });
    }

    fn invalid(&mut self, key: String, reason: &str) {
        self.errors.push(ConfigError::Invalid {
            key,
            reason: reason.to_owned(),
        });
    }

    fn duration(&mut self, suffix: &str) -> Option<Duration> {
        let (value, key) = self.raw(suffix)?;
        let parsed = parse_duration(&value);
        if parsed.is_none() {
            self.malformed(key, "a duration like 500ms, 5s or 2m");
        }
        parsed
    }

    fn count(&mut self, suffix: &str, min: u32) -> Option<u32> {
        let (value, key) = self.raw(suffix)?;
        match value.trim().parse::<u32>() {
            Ok(count) if count >= min => Some(count),
            Ok(_) => {
                self.invalid(
                    key,
                    if min == 1 {
                        "must be at least 1"
                    } else {
                        "out of range"
                    },
                );
                None
            }
            Err(_) => {
                self.malformed(key, "a non-negative integer");
                None
            }
        }
    }

    fn number(
        &mut self,
        suffix: &str,
        check: impl FnOnce(f64) -> Result<f64, &'static str>,
    ) -> Option<f64> {
        let (value, key) = self.raw(suffix)?;
        let Ok(number) = value.trim().parse::<f64>() else {
            self.malformed(key, "a decimal number");
            return None;
        };
        match check(number) {
            Ok(number) => Some(number),
            Err(reason) => {
                self.invalid(key, reason);
                None
            }
        }
    }

    fn rate(&mut self, suffix: &str) -> Option<f64> {
        let (value, key) = self.raw(suffix)?;
        let value = value.trim();
        let parsed = match value.strip_suffix('%') {
            Some(percent) => percent.trim().parse::<f64>().map(|percent| percent / 100.0),
            None => value.parse::<f64>(),
        };
        match parsed {
            Ok(rate) if rate.is_finite() && rate > 0.0 && rate <= 1.0 => Some(rate),
            Ok(_) => {
                self.invalid(key, "must be above 0 and at most 1 (or 100%)");
                None
            }
            Err(_) => {
                self.malformed(key, "a fraction like 0.5 or a percentage like 50%");
                None
            }
        }
    }

    fn boolean(&mut self, suffix: &str) -> Option<bool> {
        let (value, key) = self.raw(suffix)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => {
                self.malformed(key, "a boolean (true/false/1/0/yes/no/on/off)");
                None
            }
        }
    }

    fn window(&mut self, suffix: &str) -> Option<BreakerWindowSpec> {
        let (value, key) = self.raw(suffix)?;
        match value.parse::<BreakerWindowSpec>() {
            Ok(BreakerWindowSpec::Calls(0)) => {
                self.invalid(key, "must hold at least one call");
                None
            }
            Ok(BreakerWindowSpec::Duration(duration)) if duration.is_zero() => {
                self.invalid(key, "must be longer than zero");
                None
            }
            Ok(window) => Some(window),
            Err(_) => {
                self.malformed(key, "a call count like 20 or a duration like 30s");
                None
            }
        }
    }

    fn parsed<T: FromStr>(&mut self, suffix: &str, expected: &str) -> Option<T> {
        let (value, key) = self.raw(suffix)?;
        let parsed = value.parse::<T>().ok();
        if parsed.is_none() {
            self.malformed(key, expected);
        }
        parsed
    }
}

#[cfg(test)]
mod tests {
    use sekvent_config::MapSource;

    use super::*;

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn later_layers_override_only_what_they_set() {
        let defaults = PolicySpec {
            timeout: Some(secs(5)),
            retry_max_attempts: Some(3),
            retry_jitter: Some(Jitter::Full),
            ..PolicySpec::default()
        };
        let named = PolicySpec {
            timeout: Some(secs(2)),
            ..PolicySpec::default()
        };
        let component = PolicySpec {
            retry_max_attempts: Some(5),
            breaker_failure_rate: Some(0.25),
            ..PolicySpec::default()
        };
        let method = PolicySpec {
            timeout: Some(secs(1)),
            breaker_enabled: Some(false),
            ..PolicySpec::default()
        };
        let resolved = PolicySpec::resolve([&defaults, &named, &component, &method]);
        assert_eq!(resolved.timeout, Some(secs(1)));
        assert_eq!(resolved.retry_max_attempts, Some(5));
        assert_eq!(resolved.retry_jitter, Some(Jitter::Full));
        assert_eq!(resolved.breaker_failure_rate, Some(0.25));
        assert_eq!(resolved.breaker_enabled, Some(false));

        let without_method = PolicySpec::resolve([&defaults, &named, &component]);
        assert_eq!(without_method.timeout, Some(secs(2)));
        assert_eq!(PolicySpec::resolve([]), PolicySpec::default());
    }

    #[test]
    fn reads_every_key() {
        let source = MapSource::new()
            .with("ORDERS_TIMEOUT", "750ms")
            .with("ORDERS_RETRY_MAX_ATTEMPTS", "4")
            .with("ORDERS_RETRY_INITIAL_BACKOFF", "50ms")
            .with("ORDERS_RETRY_MAX_BACKOFF", "2")
            .with("ORDERS_RETRY_MULTIPLIER", "1.5")
            .with("ORDERS_RETRY_JITTER", "equal")
            .with("ORDERS_RETRY_BUDGET_RATIO", "0.1")
            .with("ORDERS_RETRY_BUDGET_MIN_PER_SEC", "0")
            .with("ORDERS_BULKHEAD_MAX_CONCURRENT", "8")
            .with("ORDERS_BULKHEAD_MAX_QUEUE", "16")
            .with("ORDERS_BULKHEAD_QUEUE_TIMEOUT", "250ms")
            .with("ORDERS_BREAKER_ENABLED", "yes")
            .with("ORDERS_BREAKER_FAILURE_RATE", "40%")
            .with("ORDERS_BREAKER_WINDOW", "30s")
            .with("ORDERS_BREAKER_MIN_CALLS", "5")
            .with("ORDERS_BREAKER_WAIT_IN_OPEN", "10s")
            .with("ORDERS_BREAKER_PERMITTED_IN_HALF_OPEN", "2")
            .with("ORDERS_RATE_LIMIT_PERMITS", "50")
            .with("ORDERS_RATE_LIMIT_WINDOW", "1m")
            .with("BILLING_TIMEOUT", "not read");
        let spec = PolicySpec::from_config(&source, "ORDERS_").unwrap();
        assert_eq!(spec.timeout, Some(Duration::from_millis(750)));
        assert_eq!(spec.retry_max_attempts, Some(4));
        assert_eq!(spec.retry_initial_backoff, Some(Duration::from_millis(50)));
        assert_eq!(spec.retry_max_backoff, Some(secs(2)));
        assert_eq!(spec.retry_multiplier, Some(1.5));
        assert_eq!(spec.retry_jitter, Some(Jitter::Equal));
        assert_eq!(spec.retry_budget_ratio, Some(0.1));
        assert_eq!(spec.retry_budget_min_per_sec, Some(0));
        assert_eq!(spec.bulkhead_max_concurrent, Some(8));
        assert_eq!(spec.bulkhead_max_queue, Some(16));
        assert_eq!(
            spec.bulkhead_queue_timeout,
            Some(Duration::from_millis(250))
        );
        assert_eq!(spec.breaker_enabled, Some(true));
        assert_eq!(spec.breaker_failure_rate, Some(0.4));
        assert_eq!(
            spec.breaker_window,
            Some(BreakerWindowSpec::Duration(secs(30)))
        );
        assert_eq!(spec.breaker_min_calls, Some(5));
        assert_eq!(spec.breaker_wait_in_open, Some(secs(10)));
        assert_eq!(spec.breaker_permitted_in_half_open, Some(2));
        assert_eq!(spec.rate_limit_permits, Some(50));
        assert_eq!(spec.rate_limit_window, Some(secs(60)));
        assert_eq!(PolicySpec::CONFIG_KEYS.len(), 19);

        let policy = spec.build("orders").unwrap();
        assert_eq!(policy.name(), "orders");
        assert_eq!(policy.timeout().limit(), Some(Duration::from_millis(750)));
        assert_eq!(policy.retry().max_attempts(), 4);
        assert_eq!(policy.bulkhead().unwrap().max_queue(), 16);
        assert_eq!(policy.breaker().unwrap().name(), "orders");
        assert_eq!(policy.rate_gate().unwrap().permits(), 50);
    }

    #[test]
    fn empty_source_gives_an_empty_spec() {
        let spec = PolicySpec::from_config(&MapSource::new(), "ORDERS_").unwrap();
        assert_eq!(spec, PolicySpec::default());
        let policy = spec.build("orders").unwrap();
        assert_eq!(policy.timeout().limit(), None);
        assert_eq!(policy.retry().max_attempts(), 1);
        assert!(policy.bulkhead().is_none());
        assert!(policy.breaker().is_none());
        assert!(policy.rate_gate().is_none());
    }

    #[test]
    fn a_single_bad_key_is_named_without_its_value() {
        let source = MapSource::new().with("ORDERS_TIMEOUT", "soon-ish-secret");
        let error = PolicySpec::from_config(&source, "ORDERS_").unwrap_err();
        assert_eq!(
            error,
            ConfigError::Malformed {
                key: "ORDERS_TIMEOUT".into(),
                expected: "a duration like 500ms, 5s or 2m".into(),
            }
        );
        assert!(!error.to_string().contains("soon-ish-secret"));
    }

    #[test]
    fn every_bad_key_is_reported() {
        let source = MapSource::new()
            .with("P_RETRY_MAX_ATTEMPTS", "0")
            .with("P_RETRY_MULTIPLIER", "0.5")
            .with("P_RETRY_JITTER", "sometimes")
            .with("P_RETRY_BUDGET_RATIO", "abc")
            .with("P_RETRY_BUDGET_MIN_PER_SEC", "-1")
            .with("P_BREAKER_FAILURE_RATE", "150%")
            .with("P_BREAKER_ENABLED", "maybe")
            .with("P_BREAKER_WINDOW", "0")
            .with("P_BULKHEAD_MAX_QUEUE", "many")
            .with("P_RATE_LIMIT_WINDOW", "later");
        let ConfigError::Multiple(errors) = PolicySpec::from_config(&source, "P_").unwrap_err()
        else {
            panic!("expected several errors");
        };
        let keys: Vec<_> = errors
            .iter()
            .map(|error| match error {
                ConfigError::Malformed { key, .. } | ConfigError::Invalid { key, .. } => {
                    key.as_str()
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                "P_RETRY_MAX_ATTEMPTS",
                "P_RETRY_MULTIPLIER",
                "P_RETRY_JITTER",
                "P_RETRY_BUDGET_RATIO",
                "P_RETRY_BUDGET_MIN_PER_SEC",
                "P_BULKHEAD_MAX_QUEUE",
                "P_BREAKER_ENABLED",
                "P_BREAKER_FAILURE_RATE",
                "P_BREAKER_WINDOW",
                "P_RATE_LIMIT_WINDOW",
            ]
        );
    }

    #[test]
    fn rate_and_window_edge_cases() {
        let read = |key: &str, value: &str| {
            PolicySpec::from_config(&MapSource::new().with(format!("X_{key}"), value), "X_")
        };
        assert_eq!(
            read("BREAKER_FAILURE_RATE", "0.5")
                .unwrap()
                .breaker_failure_rate,
            Some(0.5)
        );
        assert!(matches!(
            read("BREAKER_FAILURE_RATE", "0").unwrap_err(),
            ConfigError::Invalid { .. }
        ));
        assert!(matches!(
            read("BREAKER_FAILURE_RATE", "half").unwrap_err(),
            ConfigError::Malformed { .. }
        ));
        assert_eq!(
            read("BREAKER_WINDOW", "20").unwrap().breaker_window,
            Some(BreakerWindowSpec::Calls(20))
        );
        assert!(matches!(
            read("BREAKER_WINDOW", "0s").unwrap_err(),
            ConfigError::Invalid { .. }
        ));
        assert!(matches!(
            read("BREAKER_WINDOW", "a while").unwrap_err(),
            ConfigError::Malformed { .. }
        ));
        assert!(matches!(
            read("RETRY_BUDGET_RATIO", "2000").unwrap_err(),
            ConfigError::Invalid { .. }
        ));
        assert_eq!(
            read("BREAKER_ENABLED", "off").unwrap().breaker_enabled,
            Some(false)
        );
        assert!(matches!(
            read("RATE_LIMIT_PERMITS", "0").unwrap_err(),
            ConfigError::Invalid { reason, .. } if reason == "must be at least 1"
        ));
        assert!("99999999999".parse::<BreakerWindowSpec>().is_err());
    }

    #[test]
    fn serde_uses_humantime() {
        let spec = PolicySpec {
            timeout: Some(Duration::from_millis(1500)),
            retry_max_attempts: Some(3),
            retry_jitter: Some(Jitter::Equal),
            breaker_window: Some(BreakerWindowSpec::Calls(20)),
            rate_limit_window: Some(secs(60)),
            ..PolicySpec::default()
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "timeout": "1s 500ms",
                "retry_max_attempts": 3,
                "retry_jitter": "equal",
                "breaker_window": 20,
                "rate_limit_window": "1m",
            })
        );
        let back: PolicySpec = serde_json::from_value(json).unwrap();
        assert_eq!(back, spec);

        let parsed: PolicySpec = serde_json::from_value(serde_json::json!({
            "timeout": 3,
            "breaker_window": "30s",
            "bulkhead_queue_timeout": null,
        }))
        .unwrap();
        assert_eq!(parsed.timeout, Some(secs(3)));
        assert_eq!(
            parsed.breaker_window,
            Some(BreakerWindowSpec::Duration(secs(30)))
        );
        assert_eq!(parsed.bulkhead_queue_timeout, None);
        let window = serde_json::to_value(BreakerWindowSpec::Duration(secs(30))).unwrap();
        assert_eq!(window, serde_json::json!("30s"));

        assert!(
            serde_json::from_value::<PolicySpec>(serde_json::json!({"timeout": "soon"})).is_err()
        );
        assert!(serde_json::from_value::<PolicySpec>(serde_json::json!({"unknown": 1})).is_err());
        assert!(
            serde_json::from_value::<PolicySpec>(
                serde_json::json!({"breaker_window": 5_000_000_000_u64})
            )
            .is_err()
        );
    }

    #[test]
    fn build_validates_combinations() {
        let bad = |spec: PolicySpec| spec.build("x").unwrap_err().parameter();
        assert_eq!(
            bad(PolicySpec {
                retry_max_attempts: Some(0),
                ..PolicySpec::default()
            }),
            "retry.max_attempts"
        );
        assert_eq!(
            bad(PolicySpec {
                retry_max_attempts: Some(3),
                retry_initial_backoff: Some(secs(10)),
                retry_max_backoff: Some(secs(1)),
                ..PolicySpec::default()
            }),
            "backoff.max"
        );
        assert_eq!(
            bad(PolicySpec {
                bulkhead_max_concurrent: Some(0),
                ..PolicySpec::default()
            }),
            "bulkhead.max_concurrent"
        );
        assert_eq!(
            bad(PolicySpec {
                rate_limit_permits: Some(0),
                ..PolicySpec::default()
            }),
            "rate_gate.permits"
        );
        assert_eq!(
            bad(PolicySpec {
                breaker_enabled: Some(true),
                breaker_min_calls: Some(0),
                ..PolicySpec::default()
            }),
            "breaker.min_calls"
        );

        let queued = PolicySpec {
            bulkhead_max_concurrent: Some(1),
            bulkhead_max_queue: Some(2),
            breaker_enabled: Some(true),
            breaker_window: Some(BreakerWindowSpec::Duration(secs(5))),
            ..PolicySpec::default()
        }
        .build("x")
        .unwrap();
        assert_eq!(queued.bulkhead().unwrap().max_queue(), 2);
        assert!(
            queued.breaker().is_some(),
            "enabled explicitly without a rate"
        );

        let disabled = PolicySpec {
            breaker_failure_rate: Some(0.5),
            breaker_enabled: Some(false),
            ..PolicySpec::default()
        };
        assert!(disabled.build("x").unwrap().breaker().is_none());
        assert_eq!(BreakerWindowSpec::Calls(7).to_string(), "7");
    }
}
