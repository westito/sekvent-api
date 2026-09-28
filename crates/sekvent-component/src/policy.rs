//! Resilience policies of components: the layering of 4.3 (framework
//! default, attribute, named policy, component keys, method keys) over
//! [`PolicySpec`], per method and per component.

use std::collections::BTreeMap;

use sekvent_config::{ConfigError, ConfigSource};
use sekvent_resilience::{PolicyError, PolicySpec, RetryBudget, RetryPolicy};

use crate::config::{self, key};
use crate::server::MethodPolicy;
use crate::{BuildError, ComponentDescriptor, MethodDescriptor, POLICY_PREFIX};

/// Suffix of the keys that reference a named policy.
pub(crate) const POLICY: &str = "POLICY";

/// Fields accepted at method level (and at every other level).
pub(crate) const METHOD_FIELDS: &[&str] = &[
    "TIMEOUT",
    "BULKHEAD_MAX_CONCURRENT",
    "BULKHEAD_MAX_QUEUE",
    "BULKHEAD_QUEUE_TIMEOUT",
    "RETRY_MAX_ATTEMPTS",
    "RETRY_INITIAL_BACKOFF",
    "RETRY_MAX_BACKOFF",
    "RETRY_MULTIPLIER",
    "RETRY_JITTER",
    "RETRY_MAX_RETRY_AFTER",
];

/// Fields whose state belongs to the component's endpoint: accepted at
/// component level and in named policies only.
pub(crate) const COMPONENT_ONLY_FIELDS: &[&str] = &[
    "RETRY_BUDGET_RATIO",
    "RETRY_BUDGET_MIN_PER_SEC",
    "BREAKER_ENABLED",
    "BREAKER_FAILURE_RATE",
    "BREAKER_WINDOW",
    "BREAKER_MIN_CALLS",
    "BREAKER_WAIT_IN_OPEN",
    "BREAKER_PERMITTED_IN_HALF_OPEN",
];

/// Default retry budget: tokens earned per success, and retries always
/// allowed per second.
const BUDGET_RATIO: f64 = 0.2;
const BUDGET_MIN_PER_SEC: u32 = 10;

/// Every component-level field: method fields, then component-only ones.
pub(crate) fn component_fields() -> impl Iterator<Item = &'static str> {
    METHOD_FIELDS.iter().chain(COMPONENT_ONLY_FIELDS).copied()
}

/// The resolved policies of one component.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ComponentPolicies {
    /// One per method, in declaration order.
    pub(crate) methods: Vec<MethodPolicy>,
    /// Breaker and budget settings of the component.
    pub(crate) component: PolicySpec,
}

/// Layer 0: remote bindings retry idempotent methods three times and use a
/// breaker. Only the `grpc` caller reads these fields, so the layer applies
/// under every binding and one environment validates the same everywhere.
fn framework_default() -> PolicySpec {
    let mut spec = PolicySpec::default();
    spec.retry_max_attempts = Some(3);
    spec.breaker_enabled = Some(true);
    spec
}

/// Layer 1: the `#[call(timeout, bulkhead)]` attribute.
fn attribute(method: &MethodDescriptor) -> PolicySpec {
    let mut spec = PolicySpec::default();
    spec.timeout = method.timeout();
    spec.bulkhead_max_concurrent = method.bulkhead();
    spec
}

fn strip_rate_limit(spec: &mut PolicySpec) {
    spec.rate_limit_permits = None;
    spec.rate_limit_window = None;
}

fn strip_component_only(spec: &mut PolicySpec) {
    spec.retry_budget_ratio = None;
    spec.retry_budget_min_per_sec = None;
    spec.breaker_enabled = None;
    spec.breaker_failure_rate = None;
    spec.breaker_window = None;
    spec.breaker_min_calls = None;
    spec.breaker_wait_in_open = None;
    spec.breaker_permitted_in_half_open = None;
}

fn has_component_only(spec: &PolicySpec) -> bool {
    let mut stripped = spec.clone();
    strip_component_only(&mut stripped);
    stripped != *spec
}

/// The retries of one method, without a budget of its own: the component's
/// shared budget replaces it.
pub(crate) fn method_retry(spec: &PolicySpec) -> Result<Option<RetryPolicy>, PolicyError> {
    let mut spec = spec.clone();
    spec.retry_budget_ratio = None;
    spec.retry_budget_min_per_sec = None;
    spec.build_retry()
}

/// The component's retry budget, shared by its methods.
pub(crate) fn budget(spec: &PolicySpec) -> Result<RetryBudget, PolicyError> {
    RetryBudget::new(
        spec.retry_budget_ratio.unwrap_or(BUDGET_RATIO),
        spec.retry_budget_min_per_sec.unwrap_or(BUDGET_MIN_PER_SEC),
    )
}

/// A policy name: `[A-Za-z0-9_]+`, upper-cased.
fn read_name(source: &dyn ConfigSource, key: &str) -> Result<Option<String>, ConfigError> {
    source
        .get(key)
        .map(|raw| {
            if !raw.is_empty()
                && raw
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                Ok(raw.to_ascii_uppercase())
            } else {
                Err(ConfigError::Malformed {
                    key: source.describe(key),
                    expected: "a policy name of letters, digits and underscores".to_owned(),
                })
            }
        })
        .transpose()
}

/// `SEKVENT_POLICY_<NAME>_`.
fn policy_prefix(name: &str) -> String {
    format!("{POLICY_PREFIX}{name}_")
}

/// The named policies referenced so far, each read once.
#[derive(Debug, Default)]
pub(crate) struct NamedPolicies {
    /// `None` for a policy that has no key or failed to read.
    specs: BTreeMap<String, Option<PolicySpec>>,
}

impl NamedPolicies {
    /// The policy `name` referenced by `reference` (a `POLICY` key). A
    /// policy without any key, or with a malformed one, records an error and
    /// yields `None`.
    fn get(
        &mut self,
        source: &dyn ConfigSource,
        name: &str,
        reference: &str,
        errors: &mut Vec<BuildError>,
    ) -> Option<&PolicySpec> {
        let prefix = policy_prefix(name);
        if !component_fields().any(|field| source.get(&format!("{prefix}{field}")).is_some()) {
            errors.push(
                ConfigError::Invalid {
                    key: source.describe(reference),
                    reason: format!("no {prefix}* key is set"),
                }
                .into(),
            );
            self.specs.entry(name.to_owned()).or_insert(None);
            return None;
        }
        self.specs
            .entry(name.to_owned())
            .or_insert_with(|| match PolicySpec::from_config(source, &prefix) {
                Ok(mut spec) => {
                    strip_rate_limit(&mut spec);
                    Some(spec)
                }
                Err(error) => {
                    errors.push(error.into());
                    None
                }
            })
            .as_ref()
    }

    /// Reject every `SEKVENT_POLICY_*` key that is not a field of a
    /// referenced policy, sorted, with suggestions.
    pub(crate) fn check_unknown(&self, source: &dyn ConfigSource) -> Result<(), ConfigError> {
        let known: Vec<String> = self
            .specs
            .keys()
            .flat_map(|name| {
                let prefix = policy_prefix(name);
                component_fields().map(move |field| format!("{prefix}{field}"))
            })
            .collect();
        config::check_unknown(source, POLICY_PREFIX, &known)
    }
}

/// Resolve the policies of one component, recording every problem in
/// `errors`; `None` when there was any.
pub(crate) fn resolve(
    source: &dyn ConfigSource,
    descriptor: &ComponentDescriptor,
    named: &mut NamedPolicies,
    errors: &mut Vec<BuildError>,
) -> Option<ComponentPolicies> {
    let before = errors.len();
    let component = descriptor.name();
    let framework = framework_default();

    let component_prefix = format!("{}_", key(&[component]));
    let own = from_config(source, &component_prefix, errors).map(|mut spec| {
        strip_rate_limit(&mut spec);
        spec
    });
    let component_reference = key(&[component, POLICY]);
    let component_named = match read_name(source, &component_reference) {
        Ok(Some(name)) => named
            .get(source, &name, &component_reference, errors)
            .cloned(),
        Ok(None) => Some(PolicySpec::default()),
        Err(error) => {
            errors.push(error.into());
            None
        }
    };

    let mut methods = Vec::with_capacity(descriptor.methods().len());
    for method in descriptor.methods() {
        let prefix = format!("{}_", key(&[component, method.name()]));
        let own_method = from_config(source, &prefix, errors).map(|mut spec| {
            strip_rate_limit(&mut spec);
            strip_component_only(&mut spec);
            spec
        });
        let reference = key(&[component, method.name(), POLICY]);
        let method_named = match read_name(source, &reference) {
            Ok(Some(name)) => match named.get(source, &name, &reference, errors) {
                Some(spec) if has_component_only(spec) => {
                    errors.push(
                        ConfigError::Invalid {
                            key: source.describe(&reference),
                            reason: format!(
                                "policy {name} sets component-only fields (retry budget or \
                                 breaker); reference it from {component_reference} instead"
                            ),
                        }
                        .into(),
                    );
                    None
                }
                spec => spec.cloned(),
            },
            Ok(None) => component_named.clone(),
            Err(error) => {
                errors.push(error.into());
                None
            }
        };
        if let (Some(own), Some(named_layer), Some(own_method)) = (&own, &method_named, &own_method)
        {
            let spec =
                PolicySpec::resolve([&framework, &attribute(method), named_layer, own, own_method]);
            check(source, &prefix, method_checks(&spec), errors);
            methods.push(MethodPolicy {
                timeout: spec.timeout,
                spec,
            });
        }
    }

    let (Some(own), Some(component_named)) = (own, component_named) else {
        return None;
    };
    let spec = PolicySpec::resolve([&framework, &component_named, &own]);
    check(
        source,
        &component_prefix,
        component_checks(component, &spec),
        errors,
    );
    (errors.len() == before).then_some(ComponentPolicies {
        methods,
        component: spec,
    })
}

fn from_config(
    source: &dyn ConfigSource,
    prefix: &str,
    errors: &mut Vec<BuildError>,
) -> Option<PolicySpec> {
    PolicySpec::from_config(source, prefix)
        .map_err(|error| errors.push(error.into()))
        .ok()
}

/// What `build_*` rejects in a method's resolved spec.
fn method_checks(spec: &PolicySpec) -> Result<(), PolicyError> {
    spec.build_bulkhead()?;
    method_retry(spec)?;
    Ok(())
}

/// What `build_*` rejects in a component's resolved spec.
fn component_checks(component: &str, spec: &PolicySpec) -> Result<(), PolicyError> {
    spec.build_breaker(format!("component:{component}"))?;
    budget(spec)?;
    Ok(())
}

/// Record a [`PolicyError`] as `Invalid { key: "<prefix>*" }`.
fn check(
    source: &dyn ConfigSource,
    prefix: &str,
    outcome: Result<(), PolicyError>,
    errors: &mut Vec<BuildError>,
) {
    if let Err(error) = outcome {
        errors.push(
            ConfigError::Invalid {
                key: source.describe(&format!("{prefix}*")),
                reason: format!("{}: {}", error.parameter(), error.reason()),
            }
            .into(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sekvent_config::MapSource;
    use sekvent_resilience::Jitter;

    use super::*;

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve")
            .with_idempotent()
            .with_timeout(Duration::from_secs(2))
            .with_bulkhead(16),
        MethodDescriptor::call("release", "Release"),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS);

    fn resolve_pairs(pairs: &[(&str, &str)]) -> Result<ComponentPolicies, Vec<BuildError>> {
        let source: MapSource = pairs.iter().copied().collect();
        let mut named = NamedPolicies::default();
        let mut errors = Vec::new();
        let resolved = resolve(&source, INVENTORY, &mut named, &mut errors);
        if let Err(error) = named.check_unknown(&source) {
            errors.push(error.into());
        }
        match resolved {
            Some(resolved) if errors.is_empty() => Ok(resolved),
            _ => Err(errors),
        }
    }

    fn invalid_keys(errors: &[BuildError]) -> Vec<String> {
        errors
            .iter()
            .filter_map(|error| match error {
                BuildError::Config(ConfigError::Invalid { key, .. }) => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn field_lists() {
        assert_eq!(component_fields().count(), 18);
        for field in component_fields() {
            assert!(PolicySpec::CONFIG_KEYS.contains(&field), "{field}");
        }
        assert!(!component_fields().any(|field| field.starts_with("RATE_LIMIT")));
    }

    #[test]
    fn defaults_are_the_framework_layer_and_the_attributes() {
        let resolved = resolve_pairs(&[]).unwrap();
        let reserve = &resolved.methods[0];
        assert_eq!(reserve.timeout, Some(Duration::from_secs(2)));
        assert_eq!(reserve.spec.bulkhead_max_concurrent, Some(16));
        assert_eq!(reserve.spec.retry_max_attempts, Some(3));
        assert_eq!(resolved.methods[1].timeout, None);
        assert_eq!(resolved.component.breaker_enabled, Some(true));
        assert_eq!(resolved.component.retry_max_attempts, Some(3));
    }

    #[test]
    fn each_layer_beats_the_one_below() {
        let base = [
            ("SEKVENT_POLICY_REMOTE_TIMEOUT", "5s"),
            ("SEKVENT_POLICY_REMOTE_RETRY_MAX_ATTEMPTS", "4"),
            ("SEKVENT_POLICY_REMOTE_BREAKER_ENABLED", "false"),
            ("SEKVENT_COMPONENT_INVENTORY_POLICY", "remote"),
        ];
        let named = resolve_pairs(&base).unwrap();
        // The named policy beats the attribute and the framework default.
        assert_eq!(named.methods[0].timeout, Some(Duration::from_secs(5)));
        assert_eq!(named.methods[1].spec.retry_max_attempts, Some(4));
        assert_eq!(named.component.breaker_enabled, Some(false));
        // The bulkhead attribute survives a policy that does not set it.
        assert_eq!(named.methods[0].spec.bulkhead_max_concurrent, Some(16));

        let mut pairs = base.to_vec();
        pairs.extend([
            ("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "3s"),
            ("SEKVENT_COMPONENT_INVENTORY_BREAKER_ENABLED", "true"),
            ("SEKVENT_COMPONENT_INVENTORY_RETRY_JITTER", "none"),
        ]);
        let component = resolve_pairs(&pairs).unwrap();
        assert_eq!(component.methods[0].timeout, Some(Duration::from_secs(3)));
        assert_eq!(component.component.breaker_enabled, Some(true));
        assert_eq!(component.methods[1].spec.retry_jitter, Some(Jitter::None));

        pairs.extend([
            ("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT", "50ms"),
            (
                "SEKVENT_COMPONENT_INVENTORY_RELEASE_RETRY_MAX_ATTEMPTS",
                "1",
            ),
            (
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_BULKHEAD_MAX_CONCURRENT",
                "2",
            ),
        ]);
        let method = resolve_pairs(&pairs).unwrap();
        assert_eq!(method.methods[0].timeout, Some(Duration::from_millis(50)));
        assert_eq!(method.methods[0].spec.bulkhead_max_concurrent, Some(2));
        assert_eq!(method.methods[1].timeout, Some(Duration::from_secs(3)));
        assert_eq!(method.methods[1].spec.retry_max_attempts, Some(1));
        assert_eq!(method.methods[0].spec.retry_max_attempts, Some(4));
    }

    #[test]
    fn a_method_policy_replaces_the_component_policy() {
        let resolved = resolve_pairs(&[
            ("SEKVENT_POLICY_SLOW_TIMEOUT", "9s"),
            ("SEKVENT_POLICY_FAST_TIMEOUT", "1s"),
            ("SEKVENT_POLICY_FAST_RETRY_MAX_ATTEMPTS", "2"),
            ("SEKVENT_COMPONENT_INVENTORY_POLICY", "slow"),
            ("SEKVENT_COMPONENT_INVENTORY_RELEASE_POLICY", "Fast"),
        ])
        .unwrap();
        assert_eq!(resolved.methods[0].timeout, Some(Duration::from_secs(9)));
        assert_eq!(resolved.methods[1].timeout, Some(Duration::from_secs(1)));
        assert_eq!(resolved.methods[1].spec.retry_max_attempts, Some(2));
        assert_eq!(resolved.methods[0].spec.retry_max_attempts, Some(3));
    }

    #[test]
    fn component_only_fields_are_ignored_at_method_level() {
        // Not known keys either: the component's unknown-key check reports them.
        let resolved = resolve_pairs(&[
            (
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_BREAKER_ENABLED",
                "false",
            ),
            (
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_RETRY_BUDGET_RATIO",
                "0.5",
            ),
        ])
        .unwrap();
        assert_eq!(resolved.component.breaker_enabled, Some(true));
        assert_eq!(resolved.methods[0].spec.retry_budget_ratio, None);
    }

    #[test]
    fn a_method_reference_to_a_component_only_policy_is_invalid() {
        let errors = resolve_pairs(&[
            ("SEKVENT_POLICY_REMOTE_BREAKER_MIN_CALLS", "4"),
            ("SEKVENT_COMPONENT_INVENTORY_RESERVE_POLICY", "remote"),
        ])
        .unwrap_err();
        assert_eq!(
            invalid_keys(&errors),
            ["SEKVENT_COMPONENT_INVENTORY_RESERVE_POLICY"]
        );
        let text = errors[0].to_string();
        assert!(
            text.contains("SEKVENT_COMPONENT_INVENTORY_POLICY"),
            "{text}"
        );
    }

    #[test]
    fn an_empty_referenced_policy_is_invalid() {
        let errors = resolve_pairs(&[
            ("SEKVENT_COMPONENT_INVENTORY_POLICY", "ghost"),
            ("SEKVENT_COMPONENT_INVENTORY_RELEASE_POLICY", "ghost"),
        ])
        .unwrap_err();
        assert_eq!(
            invalid_keys(&errors),
            [
                "SEKVENT_COMPONENT_INVENTORY_POLICY",
                "SEKVENT_COMPONENT_INVENTORY_RELEASE_POLICY"
            ]
        );
        assert!(
            errors[0]
                .to_string()
                .contains("no SEKVENT_POLICY_GHOST_* key is set"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn policy_names_are_letters_digits_and_underscores() {
        let errors = resolve_pairs(&[
            ("SEKVENT_POLICY_REMOTE_TIMEOUT", "1s"),
            ("SEKVENT_COMPONENT_INVENTORY_POLICY", "re-mote"),
            ("SEKVENT_COMPONENT_INVENTORY_RESERVE_POLICY", ""),
        ])
        .unwrap_err();
        let malformed: Vec<&str> = errors
            .iter()
            .filter_map(|error| match error {
                BuildError::Config(ConfigError::Malformed { key, .. }) => Some(key.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            malformed,
            [
                "SEKVENT_COMPONENT_INVENTORY_POLICY",
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_POLICY"
            ]
        );
        // The policy is then unreferenced: its key is unknown.
        assert!(errors.iter().any(|error| matches!(
            error,
            BuildError::Config(ConfigError::UnknownKeys { keys, .. })
                if keys == &["SEKVENT_POLICY_REMOTE_TIMEOUT"]
        )));
    }

    #[test]
    fn unreferenced_and_misspelt_policy_keys_are_unknown() {
        let errors = resolve_pairs(&[
            ("SEKVENT_POLICY_REMOTE_TIMEOUT", "1s"),
            ("SEKVENT_POLICY_REMOTE_TIMEUOT", "1s"),
            ("SEKVENT_POLICY_REMOTE_RATE_LIMIT_PERMITS", "5"),
            ("SEKVENT_POLICY_OTHER_TIMEOUT", "1s"),
            ("SEKVENT_COMPONENT_INVENTORY_POLICY", "remote"),
        ])
        .unwrap_err();
        let [BuildError::Config(ConfigError::UnknownKeys { keys, suggestions })] = &errors[..]
        else {
            panic!("{errors:?}");
        };
        assert_eq!(
            keys,
            &[
                "SEKVENT_POLICY_OTHER_TIMEOUT",
                "SEKVENT_POLICY_REMOTE_RATE_LIMIT_PERMITS",
                "SEKVENT_POLICY_REMOTE_TIMEUOT",
            ]
        );
        assert!(suggestions.contains(&(
            "SEKVENT_POLICY_REMOTE_TIMEUOT".to_owned(),
            "SEKVENT_POLICY_REMOTE_TIMEOUT".to_owned()
        )));
    }

    #[test]
    fn policy_keys_of_a_prefixed_source_are_checked_by_their_full_names() {
        let base: MapSource = [
            ("APP_SEKVENT_POLICY_REMOTE_TIMEOUT", "1s"),
            ("APP_SEKVENT_POLICY_REMOTE_TIMEUOT", "1s"),
            ("APP_SEKVENT_COMPONENT_INVENTORY_POLICY", "remote"),
            ("SEKVENT_POLICY_OUTSIDE_TIMEOUT", "1s"),
        ]
        .into_iter()
        .collect();
        let source = sekvent_config::Prefixed::new(&base, "APP_");
        let mut named = NamedPolicies::default();
        let mut errors = Vec::new();
        let resolved = resolve(&source, INVENTORY, &mut named, &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(resolved.methods[1].timeout, Some(Duration::from_secs(1)));
        assert_eq!(
            named.check_unknown(&source),
            Err(ConfigError::UnknownKeys {
                keys: vec!["APP_SEKVENT_POLICY_REMOTE_TIMEUOT".to_owned()],
                suggestions: vec![(
                    "APP_SEKVENT_POLICY_REMOTE_TIMEUOT".to_owned(),
                    "APP_SEKVENT_POLICY_REMOTE_TIMEOUT".to_owned()
                )],
            })
        );
    }

    #[test]
    fn malformed_values_name_the_key_at_every_level() {
        let errors = resolve_pairs(&[
            ("SEKVENT_POLICY_REMOTE_RETRY_MULTIPLIER", "lots"),
            ("SEKVENT_COMPONENT_INVENTORY_POLICY", "remote"),
            ("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "0s"),
            (
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_BULKHEAD_MAX_QUEUE",
                "x",
            ),
        ])
        .unwrap_err();
        let text: Vec<String> = errors.iter().map(ToString::to_string).collect();
        let text = text.join("; ");
        for key in [
            "SEKVENT_POLICY_REMOTE_RETRY_MULTIPLIER",
            "SEKVENT_COMPONENT_INVENTORY_TIMEOUT",
            "SEKVENT_COMPONENT_INVENTORY_RESERVE_BULKHEAD_MAX_QUEUE",
        ] {
            assert!(text.contains(key), "{key} in {text}");
        }
        for value in ["lots", "0s"] {
            assert!(!text.contains(value), "{value} in {text}");
        }
    }

    #[test]
    fn policy_errors_name_the_level_prefix() {
        let errors = resolve_pairs(&[
            (
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_RETRY_INITIAL_BACKOFF",
                "10s",
            ),
            (
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_RETRY_MAX_BACKOFF",
                "1s",
            ),
            ("SEKVENT_COMPONENT_INVENTORY_RETRY_BUDGET_RATIO", "0"),
            ("SEKVENT_COMPONENT_INVENTORY_RETRY_BUDGET_MIN_PER_SEC", "0"),
        ])
        .unwrap_err();
        assert_eq!(
            invalid_keys(&errors),
            [
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_*",
                "SEKVENT_COMPONENT_INVENTORY_*"
            ]
        );
        let text = errors[1].to_string();
        assert!(text.contains("retry_budget"), "{text}");
    }

    #[test]
    fn a_zero_queue_timeout_with_a_queue_is_rejected() {
        let errors = resolve_pairs(&[
            ("SEKVENT_COMPONENT_INVENTORY_BULKHEAD_MAX_QUEUE", "2"),
            ("SEKVENT_COMPONENT_INVENTORY_BULKHEAD_QUEUE_TIMEOUT", "0s"),
        ])
        .unwrap_err();
        // Only the method with a bulkhead builds one.
        assert_eq!(
            invalid_keys(&errors),
            ["SEKVENT_COMPONENT_INVENTORY_RESERVE_*"]
        );
    }

    #[test]
    fn helpers() {
        let mut spec = framework_default();
        assert!(has_component_only(&spec));
        strip_component_only(&mut spec);
        assert!(!has_component_only(&spec));
        spec.rate_limit_permits = Some(3);
        strip_rate_limit(&mut spec);
        assert_eq!(spec.rate_limit_permits, None);
        assert!(budget(&PolicySpec::default()).is_ok());
        let mut retry = PolicySpec::default();
        retry.retry_max_attempts = Some(3);
        retry.retry_budget_ratio = Some(0.0);
        retry.retry_budget_min_per_sec = Some(0);
        assert!(method_retry(&retry).unwrap().is_some());
        assert!(budget(&retry).is_err());
        retry.retry_max_attempts = Some(1);
        assert!(method_retry(&retry).unwrap().is_none());
    }
}
