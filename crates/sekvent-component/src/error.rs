use sekvent_config::ConfigError;
use sekvent_error::AppError;

use crate::Binding;

/// An error type a component method may return.
///
/// It travels between caller and implementation as an [`AppError`], so every
/// binding delivers the same value. Derive it with
/// `#[derive(ComponentError)]`, or use [`AppError`] itself.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a component error",
    note = "derive it with `#[derive(ComponentError)]`, or use `AppError`"
)]
pub trait ComponentError: Sized + Send + 'static {
    /// Code, reason, optional domain and fields as metadata.
    fn into_app_error(self) -> AppError;

    /// Known reason (and domain, when declared) with parseable fields: that
    /// variant; anything else: the catch-all variant. Never fails.
    fn from_app_error(error: AppError) -> Self;
}

impl ComponentError for AppError {
    fn into_app_error(self) -> AppError {
        self
    }

    fn from_app_error(error: AppError) -> Self {
        error
    }
}

/// Why [`AppBuilder::build`](crate::AppBuilder::build) (or an install) failed.
///
/// Messages name configuration keys and components, never configuration
/// values.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// The same component was installed twice, by handle type or by name.
    #[error("component {component} is installed twice")]
    DuplicateInstall {
        /// The component name.
        component: String,
    },
    /// Two resources of the same type were provided.
    #[error("a resource of type {type_name} is provided twice")]
    DuplicateResource {
        /// The resource type.
        type_name: &'static str,
    },
    /// Two components or methods produce the same configuration key.
    #[error(
        "configuration key {key} would configure both {first} and {second}; rename one of them"
    )]
    KeyCollision {
        /// The full key.
        key: String,
        /// The first owner, e.g. `component inventory` or `method inventory.reserve`.
        first: String,
        /// The second owner.
        second: String,
    },
    /// A configuration key is unknown, malformed or invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The selected binding is not compiled into this build.
    #[error(
        "component {component}: binding {binding} (set by {key}) is not available in this build"
    )]
    BindingUnavailable {
        /// The component name.
        component: String,
        /// The selected binding.
        binding: Binding,
        /// The key that selected it.
        key: String,
    },
    /// A `local_only` component was bound to something other than `local`.
    #[error(
        "component {component} is local_only and can only be bound local; {key} selects {binding}"
    )]
    LocalOnly {
        /// The component name.
        component: String,
        /// The selected binding.
        binding: Binding,
        /// The key that selected it.
        key: String,
    },
    /// A `remote_only` component was bound to an in-process binding.
    #[error("component {component} is remote_only and cannot be bound {binding} ({key})")]
    RemoteOnly {
        /// The component name.
        component: String,
        /// The selected binding.
        binding: Binding,
        /// The key that selected it.
        key: String,
    },
    /// A `remote_only` component has no binding.
    #[error("component {component} is remote_only; set {key} to a remote binding")]
    RemoteOnlyUnbound {
        /// The component name.
        component: String,
        /// The component's binding key.
        key: String,
    },
    /// A component cannot be exposed over gRPC as configured.
    #[error("component {component} cannot be served over gRPC ({key}): {reason}")]
    NotServable {
        /// The component name.
        component: String,
        /// The key that asks for it.
        key: String,
        /// Why not.
        reason: &'static str,
    },
    /// A factory returned an error.
    #[error("component {component} failed to build: {source}")]
    Factory {
        /// The component name.
        component: String,
        /// The factory's error.
        source: AppError,
    },
    /// Several errors at once. Never nested; a single error is returned bare.
    #[error("{} component build errors: {}", .0.len(), joined(.0))]
    Multiple(Vec<BuildError>),
}

impl BuildError {
    /// Merge `errors` into one: `None` for none, the error itself for one,
    /// [`Multiple`](Self::Multiple) otherwise. Nested `Multiple` values, and
    /// several configuration errors carried as one, are flattened.
    pub(crate) fn combine(errors: Vec<BuildError>) -> Option<BuildError> {
        let mut flat = Vec::with_capacity(errors.len());
        for error in errors {
            flatten_into(&mut flat, error);
        }
        match flat.len() {
            0 | 1 => flat.pop(),
            _ => Some(Self::Multiple(flat)),
        }
    }
}

fn flatten_into(out: &mut Vec<BuildError>, error: BuildError) {
    match error {
        BuildError::Multiple(inner) => {
            for error in inner {
                flatten_into(out, error);
            }
        }
        BuildError::Config(ConfigError::Multiple(inner)) => {
            for error in inner {
                flatten_into(out, BuildError::Config(error));
            }
        }
        other => out.push(other),
    }
}

fn joined(errors: &[BuildError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    fn duplicate(name: &str) -> BuildError {
        BuildError::DuplicateInstall {
            component: name.into(),
        }
    }

    #[test]
    fn app_error_is_its_own_component_error() {
        let error = AppError::not_found("no such order").with_reason("ORDER_NOT_FOUND");
        let back = AppError::from_app_error(error.into_app_error());
        assert_eq!(back.reason(), Some("ORDER_NOT_FOUND"));
        assert_eq!(back.message(), "no such order");
    }

    #[test]
    fn combine_flattens_and_unwraps() {
        assert!(BuildError::combine(Vec::new()).is_none());
        let single = BuildError::combine(vec![duplicate("a")]).unwrap();
        assert!(matches!(single, BuildError::DuplicateInstall { .. }));

        let nested = BuildError::combine(vec![
            duplicate("a"),
            BuildError::Multiple(vec![duplicate("b"), duplicate("c")]),
            BuildError::Config(ConfigError::Multiple(vec![
                ConfigError::Missing { key: "K1".into() },
                ConfigError::Missing { key: "K2".into() },
            ])),
        ])
        .unwrap();
        let BuildError::Multiple(items) = nested else {
            panic!("expected Multiple");
        };
        assert_eq!(items.len(), 5);
        assert!(
            items
                .iter()
                .all(|item| !matches!(item, BuildError::Multiple(_)))
        );
        assert!(matches!(
            &items[3],
            BuildError::Config(ConfigError::Missing { key }) if key == "K1"
        ));
    }

    #[test]
    fn messages() {
        let cases: Vec<(BuildError, &str)> = vec![
            (
                duplicate("inventory"),
                "component inventory is installed twice",
            ),
            (
                BuildError::DuplicateResource { type_name: "u8" },
                "a resource of type u8 is provided twice",
            ),
            (
                BuildError::KeyCollision {
                    key: "SEKVENT_COMPONENT_A_B_C_TIMEOUT".into(),
                    first: "method a.b_c".into(),
                    second: "method a_b.c".into(),
                },
                "configuration key SEKVENT_COMPONENT_A_B_C_TIMEOUT would configure both \
                 method a.b_c and method a_b.c; rename one of them",
            ),
            (
                BuildError::BindingUnavailable {
                    component: "inventory".into(),
                    binding: Binding::Grpc,
                    key: "SEKVENT_COMPONENT_BINDING".into(),
                },
                "component inventory: binding grpc (set by SEKVENT_COMPONENT_BINDING) \
                 is not available in this build",
            ),
            (
                BuildError::LocalOnly {
                    component: "notes".into(),
                    binding: Binding::LocalSerialized,
                    key: "SEKVENT_COMPONENT_NOTES_BINDING".into(),
                },
                "component notes is local_only and can only be bound local; \
                 SEKVENT_COMPONENT_NOTES_BINDING selects local-serialized",
            ),
            (
                BuildError::RemoteOnly {
                    component: "ledger".into(),
                    binding: Binding::Local,
                    key: "SEKVENT_COMPONENT_LEDGER_BINDING".into(),
                },
                "component ledger is remote_only and cannot be bound local \
                 (SEKVENT_COMPONENT_LEDGER_BINDING)",
            ),
            (
                BuildError::RemoteOnlyUnbound {
                    component: "ledger".into(),
                    key: "SEKVENT_COMPONENT_LEDGER_BINDING".into(),
                },
                "component ledger is remote_only; set SEKVENT_COMPONENT_LEDGER_BINDING \
                 to a remote binding",
            ),
            (
                BuildError::NotServable {
                    component: "notes".into(),
                    key: "SEKVENT_COMPONENT_NOTES_SERVE".into(),
                    reason: "it is local_only",
                },
                "component notes cannot be served over gRPC (SEKVENT_COMPONENT_NOTES_SERVE): \
                 it is local_only",
            ),
            (
                BuildError::Multiple(vec![duplicate("a"), duplicate("b")]),
                "2 component build errors: component a is installed twice; \
                 component b is installed twice",
            ),
            (
                BuildError::Config(ConfigError::Missing { key: "K".into() }),
                "missing required configuration key K",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }

    #[test]
    fn a_factory_error_keeps_its_source() {
        let error = BuildError::Factory {
            component: "orders".into(),
            source: AppError::failed_precondition("install inventory before orders"),
        };
        assert_eq!(
            error.to_string(),
            "component orders failed to build: FAILED_PRECONDITION: install inventory before orders"
        );
        assert!(error.source().is_some());
        let config: BuildError = ConfigError::Missing { key: "K".into() }.into();
        assert!(matches!(config, BuildError::Config(_)));
    }
}
