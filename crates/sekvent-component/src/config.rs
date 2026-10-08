//! Component configuration: the key grammar, the known-key set and its
//! collisions, and the resolution of bindings, exposure, links and method
//! policies.
//!
//! Keys (all optional unless a rule requires them; `<C>` / `<M>` the
//! upper-cased component / method name):
//!
//! - `SEKVENT_COMPONENT_BINDING`: default binding of standard components;
//! - `SEKVENT_COMPONENT_MAX_HOPS`: deepest chain of component calls;
//! - `SEKVENT_COMPONENT_<C>_BINDING`, `_ENDPOINT`, `_LINK`, `_AUTH`, `_SERVE`,
//!   `_SERVE_AUTH`, `_POLICY` and the component policy fields;
//! - `SEKVENT_COMPONENT_<C>_<M>_POLICY` and the method policy fields;
//! - `SEKVENT_POLICY_<N>_<FIELD>`: named policies, checked in [`crate::policy`].

use std::collections::HashMap;

use sekvent_config::__private::opt_value;
use sekvent_config::{ConfigError, ConfigSource};
use sekvent_resilience::PolicySpec;

use crate::policy::{self, NamedPolicies};
use crate::server::MethodPolicy;
use crate::{
    Binding, BuildError, CONFIG_PREFIX, ComponentDescriptor, ComponentMode, DEFAULT_BINDING_KEY,
    DEFAULT_MAX_HOPS, LOCAL_CALLER, MAX_HOPS_KEY,
};

const BINDING_EXPECTED: &str = "one of local, local-serialized, grpc";
const BINDING: &str = "BINDING";
const ENDPOINT: &str = "ENDPOINT";
const LINK: &str = "LINK";
const AUTH: &str = "AUTH";
const SERVE: &str = "SERVE";
const SERVE_AUTH: &str = "SERVE_AUTH";
/// Suffixes every component is configured with, besides its policy fields.
const COMPONENT_KEYS: &[&str] = &[
    BINDING,
    ENDPOINT,
    LINK,
    AUTH,
    SERVE,
    SERVE_AUTH,
    policy::POLICY,
];
/// Largest accepted `SEKVENT_COMPONENT_MAX_HOPS`.
const MAX_HOPS_LIMIT: u32 = 1000;
/// Why a link may not be called [`LOCAL_CALLER`].
const LOCAL_RESERVED: &str =
    "`local` is reserved for the caller of in-process calls; choose another link name";

/// One installed component, as the configuration sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    pub(crate) descriptor: &'static ComponentDescriptor,
    /// Installed with `install_remote`: treated as `remote_only`.
    pub(crate) remote: bool,
}

impl Entry {
    fn mode(self) -> ComponentMode {
        if self.remote {
            ComponentMode::RemoteOnly
        } else {
            self.descriptor.mode()
        }
    }
}

/// How a locally bound component is exposed over gRPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Serve {
    /// The `SERVE` key, for error messages.
    pub(crate) key: String,
    /// Who may call it.
    pub(crate) auth: ServeAuth,
}

/// `SERVE_AUTH`: the ways a served component's callers authenticate.
/// Neither is `none`: anyone may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServeAuth {
    /// A peer service with an inbound link token.
    pub(crate) link: bool,
    /// An end user the App's end-user authenticator accepts.
    pub(crate) bearer: bool,
}

impl ServeAuth {
    pub(crate) const NONE: Self = Self {
        link: false,
        bearer: false,
    };
    pub(crate) const LINK: Self = Self {
        link: true,
        bearer: false,
    };
    pub(crate) const BEARER: Self = Self {
        link: false,
        bearer: true,
    };
    pub(crate) const LINK_BEARER: Self = Self {
        link: true,
        bearer: true,
    };

    /// The exact spellings: `link`, `bearer`, `link,bearer` (either order)
    /// and `none`.
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "link" => Some(Self::LINK),
            "bearer" => Some(Self::BEARER),
            "link,bearer" | "bearer,link" => Some(Self::LINK_BEARER),
            "none" => Some(Self::NONE),
            _ => None,
        }
    }
}

const SERVE_AUTH_EXPECTED: &str = "one of link, bearer, link,bearer, none";

/// Where a `grpc`-bound component is called.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "grpc"), allow(dead_code))]
pub(crate) struct Remote {
    /// The validated `http://host:port` URL.
    pub(crate) endpoint: String,
    /// The link whose outbound token is presented (lower-cased).
    pub(crate) link: String,
    /// Whether the outbound token is presented.
    pub(crate) auth: bool,
}

/// The configuration of one component, once it is known to be valid.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Resolved {
    pub(crate) binding: Binding,
    /// The key that decided the binding (the default key when the
    /// component's own key is unset).
    pub(crate) key: String,
    /// One policy per method, in declaration order.
    pub(crate) policies: Vec<MethodPolicy>,
    /// Component-level policy: breaker and retry budget.
    pub(crate) component: PolicySpec,
    /// `Some` when exposed over gRPC.
    pub(crate) serve: Option<Serve>,
    /// `Some` when bound `grpc`.
    pub(crate) remote: Option<Remote>,
}

/// Everything the App needs from the configuration.
#[derive(Debug)]
pub(crate) struct Settings {
    /// One entry per installed component, in install order.
    pub(crate) components: Vec<Resolved>,
    pub(crate) max_hops: u32,
    /// Read only when a remote binding or an exposed component needs it.
    #[cfg(feature = "grpc")]
    pub(crate) links: Option<sekvent_link::LinkConfig>,
}

/// `SEKVENT_COMPONENT_<parts joined by _, upper-cased>`.
pub(crate) fn key(parts: &[&str]) -> String {
    let mut key = String::from(CONFIG_PREFIX);
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            key.push('_');
        }
        key.push_str(&part.to_ascii_uppercase());
    }
    key
}

/// [`resolve_with`] for an App without an end-user authenticator.
#[cfg(test)]
pub(crate) fn resolve(
    source: &dyn ConfigSource,
    entries: &[Entry],
) -> Result<Settings, BuildError> {
    resolve_with(source, entries, false)
}

/// Resolve the binding, exposure, link settings and policies of every
/// entry, in order; `end_user` tells whether the App has an end-user
/// authenticator.
///
/// Every problem is collected: key collisions, unknown keys under
/// [`CONFIG_PREFIX`] and `SEKVENT_POLICY_`, malformed or invalid values,
/// bindings that are unavailable or do not fit the component's mode, and
/// missing endpoints, link tokens or end-user authenticator.
pub(crate) fn resolve_with(
    source: &dyn ConfigSource,
    entries: &[Entry],
    end_user: bool,
) -> Result<Settings, BuildError> {
    let mut errors = Vec::new();
    let known = known_keys(entries, &mut errors);
    if let Err(error) = check_unknown(source, CONFIG_PREFIX, &known) {
        errors.push(error.into());
    }
    let default = match read_binding(source, DEFAULT_BINDING_KEY) {
        Ok(binding) => DefaultBinding::Read(binding),
        Err(error) => {
            errors.push(error.into());
            DefaultBinding::Malformed
        }
    };
    let max_hops = read_max_hops(source).unwrap_or_else(|error| {
        errors.push(error.into());
        DEFAULT_MAX_HOPS
    });
    let mut named = NamedPolicies::default();
    let mut components = Vec::with_capacity(entries.len());
    let mut served = Vec::new();
    for entry in entries {
        let binding = resolve_binding(source, *entry, default, &mut errors);
        let policies = policy::resolve(source, entry.descriptor, &mut named, &mut errors);
        let serve = read_serve(
            source,
            *entry,
            binding.as_ref().map(|(b, _)| *b),
            end_user,
            &mut errors,
        );
        if let Ok(Some(serve)) = &serve {
            served.push((entry.descriptor, serve.key.clone()));
        }
        let remote = read_remote(
            source,
            *entry,
            binding.as_ref().map(|(b, _)| *b),
            &mut errors,
        );
        if let (Some((binding, key)), Some(policies), Ok(serve), Ok(remote)) =
            (binding, policies, serve, remote)
        {
            components.push(Resolved {
                binding,
                key,
                policies: policies.methods,
                component: policies.component,
                serve,
                remote,
            });
        }
    }
    if let Err(error) = named.check_unknown(source) {
        errors.push(error.into());
    }
    check_services(source, &served, &mut errors);
    #[cfg(feature = "grpc")]
    let links = read_links(source, &components, &mut errors);
    if let Some(error) = BuildError::combine(errors) {
        return Err(error);
    }
    warn_unauthenticated(entries, &components);
    Ok(Settings {
        components,
        max_hops,
        #[cfg(feature = "grpc")]
        links,
    })
}

/// Two exposed components with one gRPC service name would claim the same
/// routes: the second is a configuration error naming both. `served` holds
/// each exposed component and its `SERVE` key, in install order.
fn check_services(
    source: &dyn ConfigSource,
    served: &[(&'static ComponentDescriptor, String)],
    errors: &mut Vec<BuildError>,
) {
    let mut seen: Vec<(String, &ComponentDescriptor, &str)> = Vec::new();
    for (descriptor, key) in served {
        let service = descriptor
            .full_service_name()
            .unwrap_or_else(|| descriptor.service().to_owned());
        let Some(&(_, first, first_key)) = seen.iter().find(|(name, ..)| *name == service) else {
            seen.push((service, *descriptor, key.as_str()));
            continue;
        };
        errors.push(
            ConfigError::Invalid {
                key: source.describe(key),
                reason: format!(
                    "component {} would serve gRPC service {service}, which component {} \
                     already serves ({}); expose only one of them",
                    descriptor.name(),
                    first.name(),
                    source.describe(first_key)
                ),
            }
            .into(),
        );
    }
}

/// The keys of a source that start with a prefix, under the names the
/// source itself uses (whatever scope it is a view of), so they compare
/// with the names this crate builds.
struct Scope<'a> {
    source: &'a dyn ConfigSource,
    prefix: &'a str,
}

impl ConfigSource for Scope<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.source.get(key)
    }

    fn keys(&self) -> Vec<String> {
        self.source
            .keys()
            .into_iter()
            .filter(|key| key.starts_with(self.prefix))
            .collect()
    }
}

/// Reject every key of `source` under `prefix` that `known` does not list,
/// sorted, with suggestions.
///
/// `known` and the source's keys are compared as `source` names them; the
/// error shows them as [`ConfigSource::describe`] does, so a
/// [`Prefixed`](sekvent_config::Prefixed) source is checked and reported by
/// the names an operator sets.
pub(crate) fn check_unknown(
    source: &dyn ConfigSource,
    prefix: &str,
    known: &[String],
) -> Result<(), ConfigError> {
    sekvent_config::check_reserved(&Scope { source, prefix }, known).map_err(|error| {
        let ConfigError::UnknownKeys { keys, suggestions } = error else {
            return error;
        };
        ConfigError::UnknownKeys {
            keys: keys.iter().map(|key| source.describe(key)).collect(),
            suggestions: suggestions
                .iter()
                .map(|(unknown, near)| (source.describe(unknown), source.describe(near)))
                .collect(),
        }
    })
}

/// Log once per key that turns link authentication off.
fn warn_unauthenticated(entries: &[Entry], components: &[Resolved]) {
    for (entry, resolved) in entries.iter().zip(components) {
        let name = entry.descriptor.name();
        if resolved.remote.as_ref().is_some_and(|remote| !remote.auth) {
            tracing::warn!(
                key = %key(&[name, AUTH]),
                "component calls do not present a link token"
            );
        }
        if resolved
            .serve
            .as_ref()
            .is_some_and(|serve| serve.auth == ServeAuth::NONE)
        {
            tracing::warn!(
                key = %key(&[name, SERVE_AUTH]),
                "component is served over gRPC without authentication"
            );
        }
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
    claim(MAX_HOPS_KEY.to_owned(), "the hop limit");
    for entry in entries {
        let component = entry.descriptor.name();
        let owner = format!("component {component}");
        for suffix in COMPONENT_KEYS
            .iter()
            .copied()
            .chain(policy::component_fields())
        {
            claim(key(&[component, suffix]), &owner);
        }
        for method in entry.descriptor.methods() {
            let owner = format!("method {component}.{}", method.name());
            for suffix in
                std::iter::once(policy::POLICY).chain(policy::METHOD_FIELDS.iter().copied())
            {
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

/// One of two exact spellings: `Some(true)` for `yes`, `Some(false)` for
/// `no`, `None` when unset.
fn read_choice(
    source: &dyn ConfigSource,
    key: &str,
    yes: &str,
    no: &str,
) -> Result<Option<bool>, ConfigError> {
    source
        .get(key)
        .map(|raw| match raw.as_str() {
            value if value == yes => Ok(true),
            value if value == no => Ok(false),
            _ => Err(ConfigError::Malformed {
                key: source.describe(key),
                expected: format!("{yes} or {no}"),
            }),
        })
        .transpose()
}

/// `SEKVENT_COMPONENT_MAX_HOPS`: 1 to 1000, default 16.
fn read_max_hops(source: &dyn ConfigSource) -> Result<u32, ConfigError> {
    match opt_value::<u32>(source, MAX_HOPS_KEY)? {
        None => Ok(DEFAULT_MAX_HOPS),
        Some(hops) if (1..=MAX_HOPS_LIMIT).contains(&hops) => Ok(hops),
        Some(_) => Err(ConfigError::Invalid {
            key: source.describe(MAX_HOPS_KEY),
            reason: format!("must be between 1 and {MAX_HOPS_LIMIT}"),
        }),
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
    let error = match (entry.mode(), own) {
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
        (ComponentMode::RemoteOnly, Some(Binding::Grpc)) => {
            match available(component, Binding::Grpc, own_key) {
                Ok(resolved) => return Some(resolved),
                Err(error) => error,
            }
        }
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
            match available(component, binding, key) {
                Ok(resolved) => return Some(resolved),
                Err(error) => error,
            }
        }
    };
    errors.push(error);
    None
}

/// `binding` when this build has it.
fn available(
    component: String,
    binding: Binding,
    key: String,
) -> Result<(Binding, String), BuildError> {
    if binding.is_local() || cfg!(feature = "grpc") {
        Ok((binding, key))
    } else {
        Err(BuildError::BindingUnavailable {
            component,
            binding,
            key,
        })
    }
}

/// A problem was pushed onto the error list; the value is unusable.
#[derive(Debug, Clone, Copy)]
struct Recorded;

/// `SERVE` and `SERVE_AUTH`: `Ok(None)` when not exposed, [`Recorded`]
/// after recording a problem. `binding` is `None` when it failed to resolve;
/// `end_user` tells whether the App has an end-user authenticator.
fn read_serve(
    source: &dyn ConfigSource,
    entry: Entry,
    binding: Option<Binding>,
    end_user: bool,
    errors: &mut Vec<BuildError>,
) -> Result<Option<Serve>, Recorded> {
    let component = entry.descriptor.name();
    let serve_key = key(&[component, SERVE]);
    let auth_key = key(&[component, SERVE_AUTH]);
    let serve = read_choice(source, &serve_key, "grpc", "none");
    let auth = source
        .get(&auth_key)
        .map(|raw| {
            ServeAuth::parse(&raw).ok_or_else(|| ConfigError::Malformed {
                key: source.describe(&auth_key),
                expected: SERVE_AUTH_EXPECTED.to_owned(),
            })
        })
        .transpose();
    let (serve, auth) = match (serve, auth) {
        (Ok(serve), Ok(auth)) => (serve.unwrap_or(false), auth.unwrap_or(ServeAuth::LINK)),
        (serve, auth) => {
            errors.extend(serve.err().into_iter().chain(auth.err()).map(Into::into));
            return Err(Recorded);
        }
    };
    if !serve {
        return Ok(None);
    }
    let reason = if !cfg!(feature = "grpc") {
        "this build has no gRPC support; enable the grpc feature"
    } else if entry.mode() == ComponentMode::LocalOnly {
        "it is local_only"
    } else {
        match binding {
            Some(binding) if binding.is_local() => {
                if auth.bearer && !end_user {
                    errors.push(BuildError::EndUserAuthenticatorMissing {
                        component: component.to_owned(),
                        key: source.describe(&auth_key),
                    });
                    return Err(Recorded);
                }
                return Ok(Some(Serve {
                    key: serve_key,
                    auth,
                }));
            }
            Some(_) => "it is not bound locally",
            // The binding error is already recorded.
            None => return Err(Recorded),
        }
    };
    errors.push(BuildError::NotServable {
        component: component.to_owned(),
        key: serve_key,
        reason,
    });
    Err(Recorded)
}

/// `ENDPOINT`, `LINK` and `AUTH`: `Ok(None)` unless bound `grpc`,
/// [`Recorded`] after recording a problem. `LINK` and `AUTH` are validated under every
/// binding.
fn read_remote(
    source: &dyn ConfigSource,
    entry: Entry,
    binding: Option<Binding>,
    errors: &mut Vec<BuildError>,
) -> Result<Option<Remote>, Recorded> {
    let component = entry.descriptor.name();
    let link_key = key(&[component, LINK]);
    let link = source
        .get(&link_key)
        .map(|raw| {
            if raw.is_empty()
                || !raw
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                Err(ConfigError::Malformed {
                    key: source.describe(&link_key),
                    expected: "a link name of letters, digits and underscores".to_owned(),
                })
            } else if raw.eq_ignore_ascii_case(LOCAL_CALLER) {
                Err(ConfigError::Invalid {
                    key: source.describe(&link_key),
                    reason: LOCAL_RESERVED.to_owned(),
                })
            } else {
                Ok(raw.to_ascii_lowercase())
            }
        })
        .transpose();
    let auth = read_choice(source, &key(&[component, AUTH]), "link", "none");
    let (link, auth) = match (link, auth) {
        (Ok(link), Ok(auth)) => (
            link.unwrap_or_else(|| component.to_ascii_lowercase()),
            auth.unwrap_or(true),
        ),
        (link, auth) => {
            errors.extend(link.err().into_iter().chain(auth.err()).map(Into::into));
            return Err(Recorded);
        }
    };
    if binding != Some(Binding::Grpc) {
        return Ok(None);
    }
    read_endpoint(source, component, errors)
        .map(|endpoint| {
            Some(Remote {
                endpoint,
                link,
                auth,
            })
        })
        .ok_or(Recorded)
}

#[cfg(feature = "grpc")]
fn read_endpoint(
    source: &dyn ConfigSource,
    component: &str,
    errors: &mut Vec<BuildError>,
) -> Option<String> {
    let endpoint_key = key(&[component, ENDPOINT]);
    match crate::grpc::endpoint::read(source, &endpoint_key) {
        Ok(Some(endpoint)) => Some(endpoint),
        Ok(None) => {
            errors.push(
                ConfigError::Missing {
                    key: source.describe(&endpoint_key),
                }
                .into(),
            );
            None
        }
        Err(error) => {
            errors.push(error.into());
            None
        }
    }
}

#[cfg(not(feature = "grpc"))]
fn read_endpoint(
    _source: &dyn ConfigSource,
    _component: &str,
    _errors: &mut Vec<BuildError>,
) -> Option<String> {
    // A grpc binding never resolves without the feature.
    None
}

/// The link configuration, read only when a remote binding presents a token
/// or an exposed component checks one.
#[cfg(feature = "grpc")]
fn read_links(
    source: &dyn ConfigSource,
    components: &[Resolved],
    errors: &mut Vec<BuildError>,
) -> Option<sekvent_link::LinkConfig> {
    let mut outbound: Vec<&str> = Vec::new();
    for remote in components
        .iter()
        .filter_map(|resolved| resolved.remote.as_ref())
    {
        if remote.auth && !outbound.contains(&remote.link.as_str()) {
            outbound.push(&remote.link);
        }
    }
    let inbound = components
        .iter()
        .any(|resolved| resolved.serve.as_ref().is_some_and(|serve| serve.auth.link));
    if outbound.is_empty() && !inbound {
        return None;
    }
    let links = match sekvent_link::LinkConfig::from_source(source) {
        Ok(links) => links,
        Err(error) => {
            errors.push(error.into());
            return None;
        }
    };
    if let Err(error) = links.check_distinct_tokens() {
        let key = match &error {
            sekvent_link::LinkError::DuplicateToken { second, .. } => {
                match second.split_once('/') {
                    Some(("inbound", link)) => sekvent_link::inbound_key(link),
                    Some(("outbound", link)) => sekvent_link::outbound_key(link),
                    _ => "SEKVENT_LINK_*".to_owned(),
                }
            }
            _ => "SEKVENT_LINK_*".to_owned(),
        };
        errors.push(
            ConfigError::Invalid {
                key: source.describe(&key),
                reason: error.to_string(),
            }
            .into(),
        );
    }
    for link in outbound {
        if let Err(error) = links.require_outbound(link) {
            errors.push(error.into());
        }
    }
    if inbound && links.inbound().is_empty() {
        errors.push(
            ConfigError::Missing {
                key: format!("{}<CALLER>", sekvent_link::INBOUND_PREFIX),
            }
            .into(),
        );
    }
    // A served component could not tell this caller from an in-process one.
    if inbound
        && links
            .inbound()
            .identities()
            .any(|identity| identity.name == LOCAL_CALLER)
    {
        errors.push(
            ConfigError::Invalid {
                key: source.describe(&sekvent_link::inbound_key(LOCAL_CALLER)),
                reason: LOCAL_RESERVED.to_owned(),
            }
            .into(),
        );
    }
    Some(links)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    const SHOP_TOKEN: &str = "shop-token-0123456789abcdefghijklmnopqrst";
    const LEDGER_TOKEN: &str = "ledger-token-0123456789abcdefghijklmnopq";
    const ENDPOINT_URL: &str = "http://127.0.0.1:50051";

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
        resolve(&source(pairs), &[entry(descriptor)]).map(|mut all| all.components.remove(0))
    }

    fn errors(error: BuildError) -> Vec<BuildError> {
        match error {
            BuildError::Multiple(errors) => errors,
            other => vec![other],
        }
    }

    fn timeouts(resolved: &Resolved) -> Vec<Option<Duration>> {
        resolved
            .policies
            .iter()
            .map(|policy| policy.timeout)
            .collect()
    }

    fn bulkheads(resolved: &Resolved) -> Vec<Option<u32>> {
        resolved
            .policies
            .iter()
            .map(|policy| policy.spec.bulkhead_max_concurrent)
            .collect()
    }

    #[test]
    fn keys_are_upper_cased_and_prefixed() {
        assert_eq!(
            key(&["order_history", "BINDING"]),
            "SEKVENT_COMPONENT_ORDER_HISTORY_BINDING"
        );
        assert_eq!(
            key(&["inventory", "reserve", "TIMEOUT"]),
            "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT"
        );
    }

    #[test]
    fn the_known_set_lists_every_key_in_install_order() {
        let mut errors = Vec::new();
        let known = known_keys(&[entry(NOTES)], &mut errors);
        assert!(errors.is_empty());
        let mut expected = vec![
            "SEKVENT_COMPONENT_BINDING".to_owned(),
            "SEKVENT_COMPONENT_MAX_HOPS".to_owned(),
        ];
        for suffix in COMPONENT_KEYS
            .iter()
            .copied()
            .chain(policy::component_fields())
        {
            expected.push(format!("SEKVENT_COMPONENT_NOTES_{suffix}"));
        }
        expected.push("SEKVENT_COMPONENT_NOTES_ADD_POLICY".to_owned());
        for field in policy::METHOD_FIELDS {
            expected.push(format!("SEKVENT_COMPONENT_NOTES_ADD_{field}"));
        }
        assert_eq!(known, expected);
        assert!(known.contains(&"SEKVENT_COMPONENT_NOTES_SERVE_AUTH".to_owned()));
        assert!(known.contains(&"SEKVENT_COMPONENT_NOTES_BREAKER_WINDOW".to_owned()));
        assert!(!known.contains(&"SEKVENT_COMPONENT_NOTES_ADD_BREAKER_WINDOW".to_owned()));
        assert!(!known.contains(&"SEKVENT_COMPONENT_NOTES_RATE_LIMIT_PERMITS".to_owned()));
    }

    #[test]
    fn defaults_come_from_the_attributes() {
        let resolved = one(&[], INVENTORY).unwrap();
        assert_eq!(resolved.binding, Binding::Local);
        assert_eq!(resolved.key, DEFAULT_BINDING_KEY);
        assert_eq!(
            timeouts(&resolved),
            [
                Some(Duration::from_secs(2)),
                Some(Duration::from_millis(500)),
                None
            ]
        );
        assert_eq!(bulkheads(&resolved), [Some(16), None, None]);
        assert_eq!(resolved.serve, None);
        assert_eq!(resolved.remote, None);
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
            timeouts(&resolved),
            [
                Some(Duration::from_millis(50)),
                Some(Duration::from_secs(3)),
                Some(Duration::from_secs(3))
            ]
        );
        assert_eq!(bulkheads(&resolved), [Some(4), Some(2), Some(4)]);
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
        assert!(text.contains("must be longer than zero"), "{text}");
        assert!(text.contains("must be at least 1"), "{text}");
    }

    #[test]
    fn standard_bindings() {
        let own = "SEKVENT_COMPONENT_INVENTORY_BINDING";
        let endpoint = ("SEKVENT_COMPONENT_INVENTORY_ENDPOINT", ENDPOINT_URL);
        let no_auth = ("SEKVENT_COMPONENT_INVENTORY_AUTH", "none");
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
            (vec![(own, "grpc"), endpoint, no_auth], Binding::Grpc, own),
            (
                vec![(DEFAULT_BINDING_KEY, "grpc"), endpoint, no_auth],
                Binding::Grpc,
                DEFAULT_BINDING_KEY,
            ),
        ] {
            let resolved = one(&pairs, INVENTORY).unwrap();
            assert_eq!(
                (resolved.binding, resolved.key.as_str()),
                (binding, key),
                "{pairs:?}"
            );
            assert_eq!(resolved.remote.is_some(), binding == Binding::Grpc);
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
        let resolved = one(
            &[
                (own, "grpc"),
                ("SEKVENT_COMPONENT_LEDGER_ENDPOINT", "http://ledger:7000/"),
                ("SEKVENT_LINK_OUTBOUND_LEDGER", LEDGER_TOKEN),
            ],
            LEDGER,
        )
        .unwrap();
        assert_eq!(resolved.binding, Binding::Grpc);
        assert_eq!(
            resolved.remote,
            Some(Remote {
                endpoint: "http://ledger:7000".to_owned(),
                link: "ledger".to_owned(),
                auth: true,
            })
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
                (
                    "SEKVENT_COMPONENT_INVENTORY_RESERVE_BREAKER_ENABLED",
                    "true",
                ),
                ("SEKVENT_COMPONENT_INVENTORY_RATE_LIMIT_PERMITS", "3"),
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
                "SEKVENT_COMPONENT_INVENTORY_RATE_LIMIT_PERMITS",
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_BREAKER_ENABLED",
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEUOT",
                "SEKVENT_COMPONENT_ZZZ"
            ]
        );
        assert!(suggestions.contains(&(
            "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEUOT".to_owned(),
            "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT".to_owned()
        )));
    }

    #[test]
    fn a_prefixed_source_is_checked_and_reported_by_full_names() {
        for prefix in ["APP_", "SEKVENT_APP_"] {
            let base: MapSource = [
                ("SEKVENT_COMPONENT_INVENTORY_TIMEUOT", "1s"),
                ("SEKVENT_COMPONENT_BINDING", "local-serialized"),
            ]
            .into_iter()
            .map(|(key, value)| (format!("{prefix}{key}"), value))
            .chain([("SEKVENT_COMPONENT_OUTSIDE".to_owned(), "1")])
            .collect();
            let scoped = sekvent_config::Prefixed::new(&base, prefix);
            let error = resolve(&scoped, &[entry(INVENTORY)]).unwrap_err();
            let BuildError::Config(ConfigError::UnknownKeys { keys, suggestions }) = error else {
                panic!("expected unknown keys, got {error}");
            };
            let typo = format!("{prefix}SEKVENT_COMPONENT_INVENTORY_TIMEUOT");
            assert_eq!(keys, std::slice::from_ref(&typo), "{prefix}");
            assert!(
                suggestions
                    .contains(&(typo, format!("{prefix}SEKVENT_COMPONENT_INVENTORY_TIMEOUT")))
            );

            let valid: MapSource = [
                (format!("{prefix}SEKVENT_COMPONENT_INVENTORY_TIMEOUT"), "3s"),
                (
                    format!("{prefix}SEKVENT_COMPONENT_BINDING"),
                    "local-serialized",
                ),
            ]
            .into_iter()
            .collect();
            let scoped = sekvent_config::Prefixed::new(&valid, prefix);
            let settings = resolve(&scoped, &[entry(INVENTORY)]).unwrap();
            assert_eq!(settings.components[0].binding, Binding::LocalSerialized);
            assert_eq!(
                settings.components[0].policies[2].timeout,
                Some(Duration::from_secs(3))
            );
        }
    }

    #[test]
    fn two_exposed_components_cannot_serve_one_grpc_service() {
        const STOCK: &ComponentDescriptor = &ComponentDescriptor::new(
            "stock",
            "Inventory",
            &[MethodDescriptor::call("count", "Count")],
        );
        const SHOP: &ComponentDescriptor = &ComponentDescriptor::new(
            "shop_inventory",
            "Inventory",
            &[MethodDescriptor::call("count", "Count")],
        )
        .with_package("shop.v1");
        let serve = |component: &str| {
            [
                (format!("SEKVENT_COMPONENT_{component}_SERVE"), "grpc"),
                (format!("SEKVENT_COMPONENT_{component}_SERVE_AUTH"), "none"),
            ]
        };
        let pairs: MapSource = ["INVENTORY", "STOCK", "SHOP_INVENTORY"]
            .into_iter()
            .flat_map(serve)
            .collect();
        let error = resolve(&pairs, &[entry(INVENTORY), entry(STOCK), entry(SHOP)]).unwrap_err();
        let BuildError::Config(ConfigError::Invalid { key, reason }) = &error else {
            panic!("{error}");
        };
        assert_eq!(key, "SEKVENT_COMPONENT_STOCK_SERVE");
        assert_eq!(
            reason,
            "component stock would serve gRPC service Inventory, which component inventory \
             already serves (SEKVENT_COMPONENT_INVENTORY_SERVE); expose only one of them"
        );

        // The same trait under another package is another service; so is an
        // unexposed component.
        let pairs: MapSource = ["INVENTORY", "SHOP_INVENTORY"]
            .into_iter()
            .flat_map(serve)
            .collect();
        let settings = resolve(&pairs, &[entry(INVENTORY), entry(STOCK), entry(SHOP)]).unwrap();
        assert!(settings.components[0].serve.is_some());
        assert!(settings.components[1].serve.is_none());
        assert!(settings.components[2].serve.is_some());
    }

    #[test]
    fn the_local_caller_name_is_not_a_link_name() {
        for value in ["local", "LOCAL"] {
            let error = one(&[("SEKVENT_COMPONENT_INVENTORY_LINK", value)], INVENTORY).unwrap_err();
            assert!(
                matches!(&error, BuildError::Config(ConfigError::Invalid { key, reason })
                    if key == "SEKVENT_COMPONENT_INVENTORY_LINK" && reason.contains("reserved")),
                "{error}"
            );
        }

        let error = one(
            &[
                ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
                ("SEKVENT_LINK_INBOUND_LOCAL", SHOP_TOKEN),
            ],
            INVENTORY,
        )
        .unwrap_err();
        assert!(
            matches!(&error, BuildError::Config(ConfigError::Invalid { key, reason })
                if key == "SEKVENT_LINK_INBOUND_LOCAL" && reason.contains("reserved")),
            "{error}"
        );
        assert!(!error.to_string().contains(SHOP_TOKEN), "{error}");

        // Without authenticated serving no caller is named after a link.
        one(
            &[
                ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
                ("SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH", "none"),
                ("SEKVENT_LINK_INBOUND_LOCAL", SHOP_TOKEN),
            ],
            INVENTORY,
        )
        .unwrap();
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
    fn a_method_named_bulkhead_queue_collides_with_a_component_key() {
        const QUEUE: &ComponentDescriptor = &ComponentDescriptor::new(
            "jobs",
            "Jobs",
            &[MethodDescriptor::call("bulkhead_queue", "BulkheadQueue")],
        );
        let error = resolve(&MapSource::new(), &[entry(QUEUE)]).unwrap_err();
        assert!(
            matches!(
                &error,
                BuildError::KeyCollision { key, first, second }
                    if key == "SEKVENT_COMPONENT_JOBS_BULKHEAD_QUEUE_TIMEOUT"
                        && first == "component jobs"
                        && second == "method jobs.bulkhead_queue"
            ),
            "{error}"
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

    #[test]
    fn max_hops() {
        let settings = resolve(&MapSource::new(), &[entry(INVENTORY)]).unwrap();
        assert_eq!(settings.max_hops, DEFAULT_MAX_HOPS);
        for (value, hops) in [("1", 1), ("1000", 1000), ("40", 40)] {
            let settings = resolve(&source(&[(MAX_HOPS_KEY, value)]), &[]).unwrap();
            assert_eq!(settings.max_hops, hops);
        }
        for value in ["0", "1001"] {
            let error = resolve(&source(&[(MAX_HOPS_KEY, value)]), &[]).unwrap_err();
            assert!(
                matches!(&error, BuildError::Config(ConfigError::Invalid { key, .. }) if key == MAX_HOPS_KEY),
                "{error}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "configuration key {MAX_HOPS_KEY} is invalid: must be between 1 and {MAX_HOPS_LIMIT}"
                ),
                "the value is never echoed"
            );
        }
        let error = resolve(&source(&[(MAX_HOPS_KEY, "deep")]), &[]).unwrap_err();
        assert!(
            matches!(&error, BuildError::Config(ConfigError::Malformed { key, .. }) if key == MAX_HOPS_KEY),
            "{error}"
        );
    }

    #[test]
    fn choice_values_are_exact() {
        let error = one(
            &[
                ("SEKVENT_COMPONENT_INVENTORY_SERVE", "GRPC"),
                ("SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH", "off"),
                ("SEKVENT_COMPONENT_INVENTORY_AUTH", "yes"),
                ("SEKVENT_COMPONENT_INVENTORY_LINK", "bad-link"),
            ],
            INVENTORY,
        )
        .unwrap_err();
        let errors = errors(error);
        let malformed: Vec<(&str, &str)> = errors
            .iter()
            .filter_map(|error| match error {
                BuildError::Config(ConfigError::Malformed { key, expected }) => {
                    Some((key.as_str(), expected.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            malformed,
            [
                ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc or none"),
                (
                    "SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH",
                    SERVE_AUTH_EXPECTED
                ),
                (
                    "SEKVENT_COMPONENT_INVENTORY_LINK",
                    "a link name of letters, digits and underscores"
                ),
                ("SEKVENT_COMPONENT_INVENTORY_AUTH", "link or none"),
            ]
        );
    }

    #[test]
    fn serving() {
        let pairs = [
            ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
            ("SEKVENT_LINK_INBOUND_SHOP", SHOP_TOKEN),
        ];
        let resolved = one(&pairs, INVENTORY).unwrap();
        assert_eq!(
            resolved.serve,
            Some(Serve {
                key: "SEKVENT_COMPONENT_INVENTORY_SERVE".to_owned(),
                auth: ServeAuth::LINK,
            })
        );
        let settings = resolve(&source(&pairs), &[entry(INVENTORY)]).unwrap();
        assert_eq!(settings.links.unwrap().inbound().len(), 1);

        let resolved = one(
            &[
                ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
                ("SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH", "none"),
                ("SEKVENT_COMPONENT_BINDING", "local-serialized"),
            ],
            INVENTORY,
        )
        .unwrap();
        assert_eq!(
            resolved.serve.map(|serve| serve.auth),
            Some(ServeAuth::NONE)
        );

        let resolved = one(&[("SEKVENT_COMPONENT_INVENTORY_SERVE", "none")], INVENTORY).unwrap();
        assert_eq!(resolved.serve, None);
    }

    #[test]
    fn end_user_serving_needs_an_authenticator_and_no_link() {
        let serve = |auth: &'static str| {
            [
                ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
                ("SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH", auth),
            ]
        };
        for (value, auth) in [
            ("bearer", ServeAuth::BEARER),
            ("link,bearer", ServeAuth::LINK_BEARER),
            ("bearer,link", ServeAuth::LINK_BEARER),
        ] {
            let mut pairs = serve(value).to_vec();
            pairs.push(("SEKVENT_LINK_INBOUND_SHOP", SHOP_TOKEN));
            let settings = resolve_with(&source(&pairs), &[entry(INVENTORY)], true).unwrap();
            assert_eq!(
                settings.components[0]
                    .serve
                    .as_ref()
                    .map(|serve| serve.auth),
                Some(auth)
            );

            let error = resolve(&source(&pairs), &[entry(INVENTORY)]).unwrap_err();
            assert!(
                matches!(&error, BuildError::EndUserAuthenticatorMissing { component, key }
                    if component == "inventory" && key == "SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH"),
                "{error}"
            );
        }

        // Bearer alone reads no link keys; with link it needs an inbound token.
        let settings = resolve_with(&source(&serve("bearer")), &[entry(INVENTORY)], true).unwrap();
        assert!(settings.links.is_none());
        let error =
            resolve_with(&source(&serve("link,bearer")), &[entry(INVENTORY)], true).unwrap_err();
        assert!(
            matches!(&error, BuildError::Config(ConfigError::Missing { key })
                if key == "SEKVENT_LINK_INBOUND_<CALLER>"),
            "{error}"
        );

        // Not served: the mode alone asks for nothing.
        let settings = resolve(
            &source(&[("SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH", "bearer")]),
            &[entry(INVENTORY)],
        )
        .unwrap();
        assert!(settings.components[0].serve.is_none());

        for value in ["Bearer", "link, bearer", "link,bearer,link"] {
            let error = one(&serve(value), INVENTORY).unwrap_err();
            assert!(
                matches!(&error, BuildError::Config(ConfigError::Malformed { key, expected })
                    if key == "SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH" && expected == SERVE_AUTH_EXPECTED),
                "{error}"
            );
        }
    }

    #[test]
    fn serving_requires_a_local_standard_component_and_an_inbound_token() {
        let key = "SEKVENT_COMPONENT_NOTES_SERVE";
        let error = one(&[(key, "grpc")], NOTES).unwrap_err();
        assert!(
            matches!(&error, BuildError::NotServable { component, key: k, reason } if component == "notes" && k == key && *reason == "it is local_only"),
            "{error}"
        );

        let error = one(
            &[
                ("SEKVENT_COMPONENT_LEDGER_SERVE", "grpc"),
                ("SEKVENT_COMPONENT_LEDGER_BINDING", "grpc"),
                ("SEKVENT_COMPONENT_LEDGER_ENDPOINT", ENDPOINT_URL),
                ("SEKVENT_COMPONENT_LEDGER_AUTH", "none"),
            ],
            LEDGER,
        )
        .unwrap_err();
        assert!(
            matches!(&error, BuildError::NotServable { reason, .. } if *reason == "it is not bound locally"),
            "{error}"
        );

        let error = one(&[("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc")], INVENTORY).unwrap_err();
        assert!(
            matches!(&error, BuildError::Config(ConfigError::Missing { key }) if key == "SEKVENT_LINK_INBOUND_<CALLER>"),
            "{error}"
        );

        // A failed binding is reported once, not again as not servable.
        let error = one(
            &[
                ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
                ("SEKVENT_COMPONENT_INVENTORY_BINDING", "nearby"),
            ],
            INVENTORY,
        )
        .unwrap_err();
        assert!(
            matches!(&error, BuildError::Config(ConfigError::Malformed { .. })),
            "{error}"
        );
    }

    #[test]
    fn a_grpc_binding_needs_an_endpoint_and_a_token() {
        let binding = ("SEKVENT_COMPONENT_INVENTORY_BINDING", "grpc");
        let errors = errors(one(&[binding], INVENTORY).unwrap_err());
        assert!(
            errors.iter().any(|error| matches!(error, BuildError::Config(ConfigError::Missing { key }) if key == "SEKVENT_COMPONENT_INVENTORY_ENDPOINT")),
            "{errors:?}"
        );

        let error = one(
            &[
                binding,
                (
                    "SEKVENT_COMPONENT_INVENTORY_ENDPOINT",
                    "https://inventory:443",
                ),
            ],
            INVENTORY,
        )
        .unwrap_err();
        assert!(
            matches!(&error, BuildError::Config(ConfigError::Invalid { key, .. }) if key == "SEKVENT_COMPONENT_INVENTORY_ENDPOINT"),
            "{error}"
        );

        let error = one(
            &[
                binding,
                ("SEKVENT_COMPONENT_INVENTORY_ENDPOINT", ENDPOINT_URL),
            ],
            INVENTORY,
        )
        .unwrap_err();
        assert!(
            matches!(&error, BuildError::Config(ConfigError::Missing { key }) if key == "SEKVENT_LINK_OUTBOUND_INVENTORY"),
            "{error}"
        );

        let resolved = one(
            &[
                binding,
                ("SEKVENT_COMPONENT_INVENTORY_ENDPOINT", ENDPOINT_URL),
                ("SEKVENT_COMPONENT_INVENTORY_LINK", "Stock_Keeper"),
                ("SEKVENT_LINK_OUTBOUND_STOCK_KEEPER", LEDGER_TOKEN),
            ],
            INVENTORY,
        )
        .unwrap();
        assert_eq!(resolved.remote.unwrap().link, "stock_keeper");
    }

    #[test]
    fn link_problems_are_reported_with_their_keys() {
        let pairs = [
            ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
            ("SEKVENT_LINK_INBOUND_SHOP", SHOP_TOKEN),
            ("SEKVENT_LINK_OUTBOUND_BILLING", SHOP_TOKEN),
        ];
        let error = one(&pairs, INVENTORY).unwrap_err();
        let BuildError::Config(ConfigError::Invalid { key, reason }) = &error else {
            panic!("{error}");
        };
        assert_eq!(key, "SEKVENT_LINK_OUTBOUND_BILLING");
        assert!(!reason.contains(SHOP_TOKEN), "{reason}");

        let error = one(
            &[
                ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
                ("SEKVENT_LINK_INBOUND_SHOP", "tiny"),
            ],
            INVENTORY,
        )
        .unwrap_err();
        assert!(!error.to_string().contains("tiny"), "{error}");
        assert!(
            error.to_string().contains("SEKVENT_LINK_INBOUND_SHOP"),
            "{error}"
        );
    }

    #[test]
    fn link_keys_are_ignored_without_remote_bindings_or_exposure() {
        let settings = resolve(
            &source(&[("SEKVENT_LINK_INBOUND_SHOP", "tiny")]),
            &[entry(INVENTORY)],
        )
        .unwrap();
        assert!(settings.links.is_none());
        let settings = resolve(
            &source(&[
                ("SEKVENT_COMPONENT_INVENTORY_BINDING", "grpc"),
                ("SEKVENT_COMPONENT_INVENTORY_ENDPOINT", ENDPOINT_URL),
                ("SEKVENT_COMPONENT_INVENTORY_AUTH", "none"),
                ("SEKVENT_LINK_INBOUND_SHOP", "tiny"),
            ]),
            &[entry(INVENTORY)],
        )
        .unwrap();
        assert!(settings.links.is_none());
        assert!(!settings.components[0].remote.as_ref().unwrap().auth);
    }

    #[test]
    fn one_environment_builds_under_every_binding() {
        let common = [
            ("SEKVENT_COMPONENT_INVENTORY_ENDPOINT", ENDPOINT_URL),
            ("SEKVENT_COMPONENT_INVENTORY_LINK", "inventory"),
            ("SEKVENT_COMPONENT_INVENTORY_BREAKER_MIN_CALLS", "4"),
            (
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_RETRY_MAX_ATTEMPTS",
                "2",
            ),
            ("SEKVENT_COMPONENT_INVENTORY_POLICY", "remote"),
            ("SEKVENT_POLICY_REMOTE_TIMEOUT", "1s"),
            ("SEKVENT_LINK_OUTBOUND_INVENTORY", LEDGER_TOKEN),
            (MAX_HOPS_KEY, "8"),
        ];
        for binding in Binding::ALL {
            let mut pairs = common.to_vec();
            pairs.push(("SEKVENT_COMPONENT_INVENTORY_BINDING", binding.as_str()));
            let settings = resolve(&source(&pairs), &[entry(INVENTORY)]).unwrap();
            assert_eq!(settings.components[0].binding, binding);
            assert_eq!(settings.max_hops, 8);
            assert_eq!(
                settings.components[0].policies[1].timeout,
                Some(Duration::from_secs(1))
            );
        }
    }
}
