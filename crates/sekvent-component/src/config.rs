//! Component configuration: the key grammar, the known-key set and its
//! collisions, and the resolution of bindings and method policies.
//!
//! Keys (all optional, `<C>` / `<M>` the upper-cased component / method
//! name):
//!
//! - `SEKVENT_COMPONENT_BINDING`: default binding of standard components;
//! - `SEKVENT_COMPONENT_<C>_BINDING`: binding of component `C`;
//! - `SEKVENT_COMPONENT_<C>_TIMEOUT`, `SEKVENT_COMPONENT_<C>_BULKHEAD_MAX_CONCURRENT`:
//!   defaults for every method of `C`;
//! - `SEKVENT_COMPONENT_<C>_<M>_TIMEOUT`, `SEKVENT_COMPONENT_<C>_<M>_BULKHEAD_MAX_CONCURRENT`:
//!   method `M`.

use std::collections::HashMap;
use std::time::Duration;

use sekvent_config::__private::{opt_duration_value, opt_value};
use sekvent_config::{ConfigError, ConfigSource, Prefixed};

use crate::server::MethodPolicy;
use crate::{
    Binding, BuildError, CONFIG_PREFIX, ComponentDescriptor, ComponentMode, DEFAULT_BINDING_KEY,
};

const BINDING_EXPECTED: &str = "one of local, local-serialized, grpc";
const BINDING: &str = "BINDING";
const TIMEOUT: &str = "TIMEOUT";
const BULKHEAD: &str = "BULKHEAD_MAX_CONCURRENT";

/// One installed component, as the configuration sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    pub(crate) descriptor: &'static ComponentDescriptor,
    /// Installed with `install_remote`: treated as `remote_only`.
    pub(crate) remote: bool,
}

/// The configuration of one component, once it is known to be valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) binding: Binding,
    /// The key that decided the binding (the default key when the
    /// component's own key is unset).
    pub(crate) key: String,
    /// One policy per method, in declaration order.
    pub(crate) policies: Vec<MethodPolicy>,
}

/// `SEKVENT_COMPONENT_<parts joined by _, upper-cased>`.
fn key(parts: &[&str]) -> String {
    let mut key = String::from(CONFIG_PREFIX);
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            key.push('_');
        }
        key.push_str(&part.to_ascii_uppercase());
    }
    key
}

/// Resolve the binding and method policies of every entry, in order.
///
/// Every problem is collected: key collisions, unknown keys under
/// [`CONFIG_PREFIX`], malformed or invalid values, and bindings that are
/// unavailable or do not fit the component's mode.
pub(crate) fn resolve(
    source: &dyn ConfigSource,
    entries: &[Entry],
) -> Result<Vec<Resolved>, BuildError> {
    let mut errors = Vec::new();
    let known = known_keys(entries, &mut errors);
    if let Err(error) =
        sekvent_config::check_reserved(&Prefixed::new(source, CONFIG_PREFIX), &known)
    {
        errors.push(error.into());
    }
    let default = match read_binding(source, DEFAULT_BINDING_KEY) {
        Ok(binding) => DefaultBinding::Read(binding),
        Err(error) => {
            errors.push(error.into());
            DefaultBinding::Malformed
        }
    };
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let binding = resolve_binding(source, *entry, default, &mut errors);
        let policies = read_policies(source, entry.descriptor, &mut errors);
        if let (Some((binding, key)), Some(policies)) = (binding, policies) {
            resolved.push(Resolved {
                binding,
                key,
                policies,
            });
        }
    }
    match BuildError::combine(errors) {
        Some(error) => Err(error),
        None => Ok(resolved),
    }
}

/// Every key the entries may be configured with, in install order; a key
/// claimed by two owners is a [`BuildError::KeyCollision`].
fn known_keys(entries: &[Entry], errors: &mut Vec<BuildError>) -> Vec<String> {
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut known = Vec::new();
    let mut claim = |key: String, owner: &str| match owners.get(&key) {
        Some(first) if first != owner => errors.push(BuildError::KeyCollision {
            key,
            first: first.clone(),
            second: owner.to_owned(),
        }),
        Some(_) => {}
        None => {
            owners.insert(key.clone(), owner.to_owned());
            known.push(key);
        }
    };
    claim(DEFAULT_BINDING_KEY.to_owned(), "the default binding");
    for entry in entries {
        let component = entry.descriptor.name();
        let owner = format!("component {component}");
        for suffix in [BINDING, TIMEOUT, BULKHEAD] {
            claim(key(&[component, suffix]), &owner);
        }
        for method in entry.descriptor.methods() {
            let owner = format!("method {component}.{}", method.name());
            for suffix in [TIMEOUT, BULKHEAD] {
                claim(key(&[component, method.name(), suffix]), &owner);
            }
        }
    }
    known
}

/// A binding value; only the exact spellings of [`Binding::as_str`].
fn read_binding(source: &dyn ConfigSource, key: &str) -> Result<Option<Binding>, ConfigError> {
    source
        .get(key)
        .map(|raw| {
            Binding::parse(&raw).ok_or_else(|| ConfigError::Malformed {
                key: source.describe(key),
                expected: BINDING_EXPECTED.to_owned(),
            })
        })
        .transpose()
}

/// A positive duration.
fn read_timeout(source: &dyn ConfigSource, key: &str) -> Result<Option<Duration>, ConfigError> {
    match opt_duration_value(source, key)? {
        Some(timeout) if timeout.is_zero() => Err(ConfigError::Invalid {
            key: source.describe(key),
            reason: "must be a positive duration".to_owned(),
        }),
        timeout => Ok(timeout),
    }
}

/// A bulkhead size of at least one.
fn read_bulkhead(source: &dyn ConfigSource, key: &str) -> Result<Option<u32>, ConfigError> {
    match opt_value::<u32>(source, key)? {
        Some(0) => Err(ConfigError::Invalid {
            key: source.describe(key),
            reason: "must be at least 1".to_owned(),
        }),
        size => Ok(size),
    }
}

/// The value of [`DEFAULT_BINDING_KEY`].
#[derive(Debug, Clone, Copy)]
enum DefaultBinding {
    /// Read: set to a binding, or unset.
    Read(Option<Binding>),
    /// Malformed, and already reported.
    Malformed,
}

/// The binding of one entry and the key that decided it, or `None` after
/// recording why there is none.
fn resolve_binding(
    source: &dyn ConfigSource,
    entry: Entry,
    default: DefaultBinding,
    errors: &mut Vec<BuildError>,
) -> Option<(Binding, String)> {
    let component = entry.descriptor.name().to_owned();
    let own_key = key(&[&component, BINDING]);
    let own = match read_binding(source, &own_key) {
        Ok(own) => own,
        Err(error) => {
            errors.push(error.into());
            return None;
        }
    };
    let mode = if entry.remote {
        ComponentMode::RemoteOnly
    } else {
        entry.descriptor.mode()
    };
    let error = match (mode, own) {
        (ComponentMode::LocalOnly, None | Some(Binding::Local)) => {
            return Some((Binding::Local, own_key));
        }
        (ComponentMode::LocalOnly, Some(binding)) => BuildError::LocalOnly {
            component,
            binding,
            key: own_key,
        },
        (ComponentMode::RemoteOnly, None) => BuildError::RemoteOnlyUnbound {
            component,
            key: own_key,
        },
        (ComponentMode::RemoteOnly, Some(Binding::Grpc)) => BuildError::BindingUnavailable {
            component,
            binding: Binding::Grpc,
            key: own_key,
        },
        (ComponentMode::RemoteOnly, Some(binding)) => BuildError::RemoteOnly {
            component,
            binding,
            key: own_key,
        },
        (ComponentMode::Standard, own) => {
            let (binding, key) = match own {
                Some(binding) => (binding, own_key),
                None => match default {
                    DefaultBinding::Read(binding) => (
                        binding.unwrap_or(Binding::Local),
                        DEFAULT_BINDING_KEY.to_owned(),
                    ),
                    // Reported once, not per component.
                    DefaultBinding::Malformed => return None,
                },
            };
            if binding.is_local() {
                return Some((binding, key));
            }
            BuildError::BindingUnavailable {
                component,
                binding,
                key,
            }
        }
    };
    errors.push(error);
    None
}

/// Per-method policies: method key, then component key, then the declared
/// attribute. `None` after recording every malformed or invalid value.
fn read_policies(
    source: &dyn ConfigSource,
    descriptor: &ComponentDescriptor,
    errors: &mut Vec<BuildError>,
) -> Option<Vec<MethodPolicy>> {
    fn take<T>(errors: &mut Vec<BuildError>, value: Result<Option<T>, ConfigError>) -> Option<T> {
        value.unwrap_or_else(|error| {
            errors.push(error.into());
            None
        })
    }

    let before = errors.len();
    let component = descriptor.name();
    let component_timeout = take(errors, read_timeout(source, &key(&[component, TIMEOUT])));
    let component_bulkhead = take(errors, read_bulkhead(source, &key(&[component, BULKHEAD])));
    let mut policies = Vec::with_capacity(descriptor.methods().len());
    for method in descriptor.methods() {
        let name = method.name();
        let timeout = take(
            errors,
            read_timeout(source, &key(&[component, name, TIMEOUT])),
        );
        let bulkhead = take(
            errors,
            read_bulkhead(source, &key(&[component, name, BULKHEAD])),
        );
        policies.push(MethodPolicy {
            timeout: timeout.or(component_timeout).or(method.timeout()),
            bulkhead: bulkhead.or(component_bulkhead).or(method.bulkhead()),
        });
    }
    (errors.len() == before).then_some(policies)
}

#[cfg(test)]
mod tests {
    use sekvent_config::MapSource;

    use super::*;
    use crate::MethodDescriptor;

    const INVENTORY_METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve")
            .with_timeout(Duration::from_secs(2))
            .with_bulkhead(16),
        MethodDescriptor::call("release", "Release").with_timeout(Duration::from_millis(500)),
        MethodDescriptor::call("stock", "Stock"),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", INVENTORY_METHODS);
    const NOTES: &ComponentDescriptor =
        &ComponentDescriptor::new("notes", "Notes", &[MethodDescriptor::call("add", "Add")])
            .with_mode(ComponentMode::LocalOnly);
    const LEDGER: &ComponentDescriptor = &ComponentDescriptor::new(
        "ledger",
        "Ledger",
        &[MethodDescriptor::call("post", "Post")],
    )
    .with_mode(ComponentMode::RemoteOnly);

    fn entry(descriptor: &'static ComponentDescriptor) -> Entry {
        Entry {
            descriptor,
            remote: descriptor.mode() == ComponentMode::RemoteOnly,
        }
    }

    fn source(pairs: &[(&str, &str)]) -> MapSource {
        pairs.iter().copied().collect()
    }

    fn one(
        pairs: &[(&str, &str)],
        descriptor: &'static ComponentDescriptor,
    ) -> Result<Resolved, BuildError> {
        resolve(&source(pairs), &[entry(descriptor)]).map(|mut all| all.remove(0))
    }

    fn errors(error: BuildError) -> Vec<BuildError> {
        match error {
            BuildError::Multiple(errors) => errors,
            other => vec![other],
        }
    }

    #[test]
    fn keys_are_upper_cased_and_prefixed() {
        assert_eq!(
            key(&["order_history", "BINDING"]),
            "SEKVENT_COMPONENT_ORDER_HISTORY_BINDING"
        );
        assert_eq!(
            key(&["inventory", "reserve", TIMEOUT]),
            "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT"
        );
    }

    #[test]
    fn the_known_set_lists_every_key_in_install_order() {
        let mut errors = Vec::new();
        let known = known_keys(&[entry(NOTES)], &mut errors);
        assert!(errors.is_empty());
        assert_eq!(
            known,
            [
                "SEKVENT_COMPONENT_BINDING",
                "SEKVENT_COMPONENT_NOTES_BINDING",
                "SEKVENT_COMPONENT_NOTES_TIMEOUT",
                "SEKVENT_COMPONENT_NOTES_BULKHEAD_MAX_CONCURRENT",
                "SEKVENT_COMPONENT_NOTES_ADD_TIMEOUT",
                "SEKVENT_COMPONENT_NOTES_ADD_BULKHEAD_MAX_CONCURRENT",
            ]
        );
    }

    #[test]
    fn defaults_come_from_the_attributes() {
        let resolved = one(&[], INVENTORY).unwrap();
        assert_eq!(resolved.binding, Binding::Local);
        assert_eq!(resolved.key, DEFAULT_BINDING_KEY);
        assert_eq!(
            resolved.policies,
            [
                MethodPolicy {
                    timeout: Some(Duration::from_secs(2)),
                    bulkhead: Some(16)
                },
                MethodPolicy {
                    timeout: Some(Duration::from_millis(500)),
                    bulkhead: None
                },
                MethodPolicy::default(),
            ]
        );
    }

    #[test]
    fn method_keys_beat_component_keys_beat_attributes() {
        let resolved = one(
            &[
                ("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "3s"),
                ("SEKVENT_COMPONENT_INVENTORY_BULKHEAD_MAX_CONCURRENT", "4"),
                ("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT", "50ms"),
                (
                    "SEKVENT_COMPONENT_INVENTORY_RELEASE_BULKHEAD_MAX_CONCURRENT",
                    "2",
                ),
            ],
            INVENTORY,
        )
        .unwrap();
        assert_eq!(
            resolved.policies,
            [
                MethodPolicy {
                    timeout: Some(Duration::from_millis(50)),
                    bulkhead: Some(4)
                },
                MethodPolicy {
                    timeout: Some(Duration::from_secs(3)),
                    bulkhead: Some(2)
                },
                MethodPolicy {
                    timeout: Some(Duration::from_secs(3)),
                    bulkhead: Some(4)
                },
            ]
        );
        let whole_seconds = one(
            &[("SEKVENT_COMPONENT_INVENTORY_STOCK_TIMEOUT", "7")],
            INVENTORY,
        )
        .unwrap();
        assert_eq!(
            whole_seconds.policies[2].timeout,
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn malformed_and_zero_values_name_the_key_never_the_value() {
        let error = one(
            &[
                ("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "soonish"),
                ("SEKVENT_COMPONENT_INVENTORY_BULKHEAD_MAX_CONCURRENT", "0"),
                ("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT", "0s"),
                (
                    "SEKVENT_COMPONENT_INVENTORY_RELEASE_BULKHEAD_MAX_CONCURRENT",
                    "many",
                ),
                ("SEKVENT_COMPONENT_INVENTORY_BINDING", "Local"),
            ],
            INVENTORY,
        )
        .unwrap_err();
        let text = error.to_string();
        for value in ["soonish", "many", "Local"] {
            assert!(!text.contains(value), "{text}");
        }
        let errors = errors(error);
        assert_eq!(errors.len(), 5, "{text}");
        let expect = |key: &str, malformed: bool| {
            assert!(
                errors.iter().any(|error| match error {
                    BuildError::Config(ConfigError::Malformed { key: k, .. }) =>
                        malformed && k == key,
                    BuildError::Config(ConfigError::Invalid { key: k, .. }) =>
                        !malformed && k == key,
                    _ => false,
                }),
                "{key} in {text}"
            );
        };
        expect("SEKVENT_COMPONENT_INVENTORY_BINDING", true);
        expect("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", true);
        expect("SEKVENT_COMPONENT_INVENTORY_BULKHEAD_MAX_CONCURRENT", false);
        expect("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT", false);
        expect(
            "SEKVENT_COMPONENT_INVENTORY_RELEASE_BULKHEAD_MAX_CONCURRENT",
            true,
        );
        assert!(
            text.contains("one of local, local-serialized, grpc"),
            "{text}"
        );
    }

    #[test]
    fn standard_bindings() {
        let own = "SEKVENT_COMPONENT_INVENTORY_BINDING";
        for (pairs, binding, key) in [
            (vec![], Binding::Local, DEFAULT_BINDING_KEY),
            (
                vec![(DEFAULT_BINDING_KEY, "local-serialized")],
                Binding::LocalSerialized,
                DEFAULT_BINDING_KEY,
            ),
            (
                vec![(DEFAULT_BINDING_KEY, "local")],
                Binding::Local,
                DEFAULT_BINDING_KEY,
            ),
            (vec![(own, "local")], Binding::Local, own),
            (
                vec![(own, "local-serialized")],
                Binding::LocalSerialized,
                own,
            ),
            (
                vec![(DEFAULT_BINDING_KEY, "local-serialized"), (own, "local")],
                Binding::Local,
                own,
            ),
            (
                vec![(DEFAULT_BINDING_KEY, "grpc"), (own, "local")],
                Binding::Local,
                own,
            ),
        ] {
            let resolved = one(&pairs, INVENTORY).unwrap();
            assert_eq!(
                (resolved.binding, resolved.key.as_str()),
                (binding, key),
                "{pairs:?}"
            );
        }
        for (pairs, key) in [
            (vec![(own, "grpc")], own),
            (vec![(DEFAULT_BINDING_KEY, "grpc")], DEFAULT_BINDING_KEY),
        ] {
            let error = one(&pairs, INVENTORY).unwrap_err();
            assert!(
                matches!(
                    &error,
                    BuildError::BindingUnavailable { component, binding: Binding::Grpc, key: k }
                        if component == "inventory" && k == key
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn local_only_bindings() {
        let own = "SEKVENT_COMPONENT_NOTES_BINDING";
        for pairs in [
            vec![],
            vec![(own, "local")],
            vec![(DEFAULT_BINDING_KEY, "local-serialized")],
            vec![(DEFAULT_BINDING_KEY, "grpc")],
        ] {
            let resolved = one(&pairs, NOTES).unwrap();
            assert_eq!(resolved.binding, Binding::Local, "{pairs:?}");
            assert_eq!(resolved.key, own);
        }
        for (value, binding) in [
            ("local-serialized", Binding::LocalSerialized),
            ("grpc", Binding::Grpc),
        ] {
            let error = one(&[(own, value)], NOTES).unwrap_err();
            assert!(
                matches!(
                    &error,
                    BuildError::LocalOnly { component, binding: b, key } if component == "notes" && *b == binding && key == own
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn remote_only_bindings() {
        let own = "SEKVENT_COMPONENT_LEDGER_BINDING";
        let error = one(&[(DEFAULT_BINDING_KEY, "local")], LEDGER).unwrap_err();
        assert!(
            matches!(&error, BuildError::RemoteOnlyUnbound { component, key } if component == "ledger" && key == own),
            "{error}"
        );
        let error = one(&[(own, "grpc")], LEDGER).unwrap_err();
        assert!(
            matches!(&error, BuildError::BindingUnavailable { binding: Binding::Grpc, key, .. } if key == own),
            "{error}"
        );
        for (value, binding) in [
            ("local", Binding::Local),
            ("local-serialized", Binding::LocalSerialized),
        ] {
            let error = one(&[(own, value)], LEDGER).unwrap_err();
            assert!(
                matches!(&error, BuildError::RemoteOnly { binding: b, key, .. } if *b == binding && key == own),
                "{error}"
            );
        }
    }

    #[test]
    fn a_remote_install_of_a_standard_component_is_remote_only() {
        let entries = [Entry {
            descriptor: INVENTORY,
            remote: true,
        }];
        let error = resolve(&MapSource::new(), &entries).unwrap_err();
        assert!(
            matches!(error, BuildError::RemoteOnlyUnbound { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_malformed_default_is_reported_once() {
        let error = resolve(
            &source(&[(DEFAULT_BINDING_KEY, "serialized")]),
            &[entry(INVENTORY), entry(NOTES)],
        )
        .unwrap_err();
        assert!(
            matches!(&error, BuildError::Config(ConfigError::Malformed { key, .. }) if key == DEFAULT_BINDING_KEY),
            "{error}"
        );
    }

    #[test]
    fn unknown_keys_are_sorted_with_a_suggestion() {
        let error = resolve(
            &source(&[
                ("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEUOT", "1s"),
                ("SEKVENT_COMPONENT_ZZZ", "1"),
                ("SEKVENT_LOG", "debug"),
                ("OTHER", "x"),
            ]),
            &[entry(INVENTORY)],
        )
        .unwrap_err();
        let BuildError::Config(ConfigError::UnknownKeys { keys, suggestions }) = error else {
            panic!("expected unknown keys, got {error}");
        };
        assert_eq!(
            keys,
            [
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEUOT",
                "SEKVENT_COMPONENT_ZZZ"
            ]
        );
        assert_eq!(
            suggestions,
            [(
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEUOT".to_owned(),
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT".to_owned()
            )]
        );
    }

    #[test]
    fn colliding_keys_name_both_owners() {
        const A: &ComponentDescriptor =
            &ComponentDescriptor::new("a", "A", &[MethodDescriptor::call("b_c", "BC")]);
        const AB: &ComponentDescriptor =
            &ComponentDescriptor::new("a_b", "AB", &[MethodDescriptor::call("c", "C")]);
        let error = resolve(&MapSource::new(), &[entry(A), entry(AB)]).unwrap_err();
        let errors = errors(error);
        assert!(
            errors.iter().any(|error| matches!(
                error,
                BuildError::KeyCollision { key, first, second }
                    if key == "SEKVENT_COMPONENT_A_B_C_TIMEOUT"
                        && first == "method a.b_c"
                        && second == "method a_b.c"
            )),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .all(|error| matches!(error, BuildError::KeyCollision { .. }))
        );
    }

    #[test]
    fn errors_of_several_components_are_reported_together() {
        let error = resolve(
            &source(&[
                ("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "later"),
                ("SEKVENT_COMPONENT_NOTES_BINDING", "grpc"),
            ]),
            &[entry(INVENTORY), entry(NOTES), entry(LEDGER)],
        )
        .unwrap_err();
        assert_eq!(errors(error).len(), 3);
    }
}
