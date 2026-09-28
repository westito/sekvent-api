use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use sekvent_config::{ConfigError, ConfigSource, Secret};

use crate::token::{same_token, validate_token};
use crate::{BearerInjector, InboundLink, LinkError, TokenMap};

/// Prefix of inbound token keys: `SEKVENT_LINK_INBOUND_<NAME>`.
pub const INBOUND_PREFIX: &str = "SEKVENT_LINK_INBOUND_";
/// Prefix of outbound token keys: `SEKVENT_LINK_OUTBOUND_<NAME>`.
pub const OUTBOUND_PREFIX: &str = "SEKVENT_LINK_OUTBOUND_";
/// Comma-separated names of the inbound links that are trusted.
pub const TRUSTED_KEY: &str = "SEKVENT_LINK_TRUSTED";

/// `SEKVENT_LINK_INBOUND_<LINK>` (upper-cased).
pub fn inbound_key(link: &str) -> String {
    format!("{INBOUND_PREFIX}{}", link.to_ascii_uppercase())
}

/// `SEKVENT_LINK_OUTBOUND_<LINK>` (upper-cased).
pub fn outbound_key(link: &str) -> String {
    format!("{OUTBOUND_PREFIX}{}", link.to_ascii_uppercase())
}

/// Service-link configuration read from a [`ConfigSource`].
///
/// `<NAME>` is `[A-Za-z0-9_]+` and becomes the lower-cased link name, so
/// `SEKVENT_LINK_INBOUND_BILLING` configures link `billing`. Names in
/// [`TRUSTED_KEY`] are matched case-insensitively and must each have an
/// inbound token. Every problem is reported at once; errors name keys,
/// never tokens.
#[derive(Debug, Clone)]
pub struct LinkConfig {
    inbound: Arc<TokenMap>,
    outbound: BTreeMap<String, BearerInjector>,
}

impl LinkConfig {
    /// Read and validate the link keys in `source`.
    pub fn from_source(source: &dyn ConfigSource) -> Result<Self, ConfigError> {
        let mut errors = Vec::new();
        let trusted = trusted_names(source);
        let mut inbound: Vec<(InboundLink, String)> = Vec::new();
        let mut outbound = BTreeMap::new();

        let mut keys = source.keys();
        keys.sort();
        for key in keys {
            if let Some(suffix) = key.strip_prefix(INBOUND_PREFIX) {
                let Some((name, token)) = read_link(source, &key, suffix, &mut errors) else {
                    continue;
                };
                if inbound.iter().any(|(link, _)| link.name == name) {
                    errors.push(invalid(
                        source,
                        &key,
                        &LinkError::DuplicateName { link: name },
                    ));
                    continue;
                }
                let link = InboundLink {
                    trusted: trusted.contains(&name),
                    name,
                    token,
                };
                inbound.push((link, key));
            } else if let Some(suffix) = key.strip_prefix(OUTBOUND_PREFIX) {
                let Some((name, token)) = read_link(source, &key, suffix, &mut errors) else {
                    continue;
                };
                if outbound.contains_key(&name) {
                    errors.push(invalid(
                        source,
                        &key,
                        &LinkError::DuplicateName { link: name },
                    ));
                    continue;
                }
                match BearerInjector::new(name.clone(), &token) {
                    Ok(injector) => {
                        outbound.insert(name, injector);
                    }
                    Err(error) => errors.push(invalid(source, &key, &error)),
                }
            }
        }

        for name in &trusted {
            if !inbound.iter().any(|(link, _)| link.name == *name) {
                errors.push(ConfigError::Invalid {
                    key: source.describe(TRUSTED_KEY),
                    reason: format!(
                        "it names link `{name}`, which has no {INBOUND_PREFIX}<NAME> token"
                    ),
                });
            }
        }

        let (links, link_keys): (Vec<_>, Vec<_>) = inbound.into_iter().unzip();
        let map = match TokenMap::new(links) {
            Ok(map) => Some(map),
            Err(error) => {
                let key = match &error {
                    LinkError::DuplicateToken { second, .. } => link_keys
                        .iter()
                        .find(|key| key[INBOUND_PREFIX.len()..].eq_ignore_ascii_case(second))
                        .cloned()
                        .unwrap_or_else(|| format!("{INBOUND_PREFIX}*")),
                    _ => format!("{INBOUND_PREFIX}*"),
                };
                errors.push(invalid(source, &key, &error));
                None
            }
        };

        match (map, errors.len()) {
            (Some(map), 0) => Ok(Self {
                inbound: Arc::new(map),
                outbound,
            }),
            (_, 1) => Err(errors.remove(0)),
            _ => Err(ConfigError::Multiple(errors)),
        }
    }

    /// The accepted inbound tokens, ready for the inbound middleware,
    /// interceptor or [`authenticator`](crate::authenticator).
    pub fn inbound(&self) -> &Arc<TokenMap> {
        &self.inbound
    }

    /// The injector for calls to `name`, if configured.
    pub fn outbound(&self, name: &str) -> Option<&BearerInjector> {
        self.outbound.get(&name.to_ascii_lowercase())
    }

    /// The injector for calls to `name`; a missing one is an error naming
    /// the expected key.
    pub fn require_outbound(&self, name: &str) -> Result<&BearerInjector, ConfigError> {
        self.outbound(name).ok_or_else(|| ConfigError::Missing {
            key: outbound_key(name),
        })
    }

    /// Fail when two link keys (inbound or outbound) hold the same token, so
    /// that no token serves two purposes in one process.
    ///
    /// [`LinkError::DuplicateToken`] names the two links as `inbound/<link>`
    /// or `outbound/<link>`, never the token; inbound links come first, then
    /// outbound ones, each in name order. Tokens are compared in constant
    /// time, like [`validate_unique`](crate::validate_unique).
    pub fn check_distinct_tokens(&self) -> Result<(), LinkError> {
        let links: Vec<(String, &Secret)> = self
            .inbound
            .tokens()
            .map(|(name, token)| (format!("inbound/{name}"), token))
            .chain(
                self.outbound
                    .iter()
                    .map(|(name, injector)| (format!("outbound/{name}"), injector.token())),
            )
            .collect();
        for (position, (first, left)) in links.iter().enumerate() {
            if let Some((second, _)) = links[position + 1..]
                .iter()
                .find(|(_, right)| same_token(left, right))
            {
                return Err(LinkError::DuplicateToken {
                    first: first.clone(),
                    second: second.clone(),
                });
            }
        }
        Ok(())
    }

    /// Names of the configured outbound links.
    pub fn outbound_names(&self) -> impl Iterator<Item = &str> {
        self.outbound.keys().map(String::as_str)
    }

    /// The link keys present in `source`, to declare as known when checking
    /// the reserved `SEKVENT_` namespace for unknown keys.
    pub fn keys(source: &dyn ConfigSource) -> Vec<String> {
        let mut keys: Vec<String> = source
            .keys()
            .into_iter()
            .filter(|key| {
                key == TRUSTED_KEY
                    || key.starts_with(INBOUND_PREFIX)
                    || key.starts_with(OUTBOUND_PREFIX)
            })
            .collect();
        keys.sort();
        keys
    }
}

fn trusted_names(source: &dyn ConfigSource) -> BTreeSet<String> {
    source
        .get(TRUSTED_KEY)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

/// Validate one link key's name and token, recording any problem.
fn read_link(
    source: &dyn ConfigSource,
    key: &str,
    suffix: &str,
    errors: &mut Vec<ConfigError>,
) -> Option<(String, Secret)> {
    if suffix.is_empty()
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        errors.push(ConfigError::Invalid {
            key: source.describe(key),
            reason: "the link name after the prefix must be one or more of A-Z, a-z, 0-9 and `_`"
                .to_owned(),
        });
        return None;
    }
    let name = suffix.to_ascii_lowercase();
    let token = Secret::new(source.get(key).unwrap_or_default());
    if token.is_blank() {
        errors.push(ConfigError::EmptySecret {
            key: source.describe(key),
        });
        return None;
    }
    if let Err(error) = validate_token(&name, token.expose()) {
        errors.push(invalid(source, key, &error));
        return None;
    }
    Some((name, token))
}

fn invalid(source: &dyn ConfigSource, key: &str, error: &LinkError) -> ConfigError {
    ConfigError::Invalid {
        key: source.describe(key),
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use sekvent_config::MapSource;
    use sekvent_context::ServiceIdentity;

    use super::*;

    const BILLING: &str = "billing-token-0123456789abcdefABCDEF";
    const ORDERS: &str = "orders_token_0123456789abcdefABCDEF";
    const SHIPPING: &str = "shipping-token-0123456789abcdefABCDEF";

    fn errors(result: Result<LinkConfig, ConfigError>) -> Vec<ConfigError> {
        match result.unwrap_err() {
            ConfigError::Multiple(errors) => errors,
            single => vec![single],
        }
    }

    fn assert_no_token(errors: &[ConfigError]) {
        for error in errors {
            let text = error.to_string();
            for token in [BILLING, ORDERS, SHIPPING] {
                assert!(!text.contains(token), "{text}");
            }
        }
    }

    #[test]
    fn reads_inbound_outbound_and_trusted() {
        let source = MapSource::new()
            .with("SEKVENT_LINK_INBOUND_BILLING", BILLING)
            .with("SEKVENT_LINK_INBOUND_ORDERS", ORDERS)
            .with("SEKVENT_LINK_OUTBOUND_SHIPPING", SHIPPING)
            .with("SEKVENT_LINK_TRUSTED", " Billing , ,")
            .with("UNRELATED", "x");
        let config = LinkConfig::from_source(&source).unwrap();

        let inbound = config.inbound();
        assert_eq!(inbound.len(), 2);
        assert_eq!(
            inbound.authenticate(BILLING),
            Some(ServiceIdentity::trusted("billing"))
        );
        assert_eq!(
            inbound.authenticate(ORDERS),
            Some(ServiceIdentity::untrusted("orders"))
        );

        assert_eq!(config.outbound("shipping").unwrap().link(), "shipping");
        assert!(config.outbound("SHIPPING").is_some());
        assert!(config.outbound("billing").is_none());
        assert!(config.require_outbound("shipping").is_ok());
        assert_eq!(
            config.require_outbound("billing").unwrap_err(),
            ConfigError::Missing {
                key: "SEKVENT_LINK_OUTBOUND_BILLING".into()
            }
        );
        assert_eq!(config.outbound_names().collect::<Vec<_>>(), ["shipping"]);
        assert!(!format!("{config:?}").contains(BILLING));

        assert_eq!(
            LinkConfig::keys(&source),
            [
                "SEKVENT_LINK_INBOUND_BILLING",
                "SEKVENT_LINK_INBOUND_ORDERS",
                "SEKVENT_LINK_OUTBOUND_SHIPPING",
                "SEKVENT_LINK_TRUSTED",
            ]
        );
    }

    #[test]
    fn empty_source_is_an_empty_config() {
        let config = LinkConfig::from_source(&MapSource::new()).unwrap();
        assert!(config.inbound().is_empty());
        assert_eq!(config.outbound_names().count(), 0);
        assert!(LinkConfig::keys(&MapSource::new()).is_empty());
    }

    #[test]
    fn single_error_is_not_wrapped() {
        let source = MapSource::new().with("SEKVENT_LINK_INBOUND_BILLING", "short");
        let error = LinkConfig::from_source(&source).unwrap_err();
        assert!(matches!(
            &error,
            ConfigError::Invalid { key, .. } if key == "SEKVENT_LINK_INBOUND_BILLING"
        ));
    }

    #[test]
    fn every_problem_is_reported_by_key() {
        let source = MapSource::new()
            .with("SEKVENT_LINK_INBOUND_", BILLING)
            .with("SEKVENT_LINK_INBOUND_BAD-NAME", BILLING)
            .with("SEKVENT_LINK_INBOUND_BLANK", "   ")
            .with("SEKVENT_LINK_INBOUND_SHORT", "short")
            .with("SEKVENT_LINK_INBOUND_SPACED", format!("{BILLING} x"))
            .with("SEKVENT_LINK_OUTBOUND_SHIPPING", format!(" {SHIPPING}"))
            .with("SEKVENT_LINK_OUTBOUND_EMPTY", "")
            .with("SEKVENT_LINK_TRUSTED", "ghost");
        let errors = errors(LinkConfig::from_source(&source));
        assert_no_token(&errors);
        let keys: Vec<(&str, bool)> = errors
            .iter()
            .map(|error| match error {
                ConfigError::Invalid { key, .. } => (key.as_str(), false),
                ConfigError::EmptySecret { key } => (key.as_str(), true),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            keys,
            [
                ("SEKVENT_LINK_INBOUND_", false),
                ("SEKVENT_LINK_INBOUND_BAD-NAME", false),
                ("SEKVENT_LINK_INBOUND_BLANK", true),
                ("SEKVENT_LINK_INBOUND_SHORT", false),
                ("SEKVENT_LINK_INBOUND_SPACED", false),
                ("SEKVENT_LINK_OUTBOUND_EMPTY", true),
                ("SEKVENT_LINK_OUTBOUND_SHIPPING", false),
                ("SEKVENT_LINK_TRUSTED", false),
            ]
        );
    }

    #[test]
    fn names_differing_only_in_case_collide() {
        let source = MapSource::new()
            .with("SEKVENT_LINK_INBOUND_BILLING", BILLING)
            .with("SEKVENT_LINK_INBOUND_billing", ORDERS)
            .with("SEKVENT_LINK_OUTBOUND_SHIPPING", SHIPPING)
            .with("SEKVENT_LINK_OUTBOUND_shipping", SHIPPING);
        let errors = errors(LinkConfig::from_source(&source));
        assert_eq!(errors.len(), 2);
        assert_no_token(&errors);
        for error in &errors {
            assert!(error.to_string().contains("configured twice"), "{error}");
        }
    }

    #[test]
    fn shared_inbound_token_names_the_second_key() {
        let source = MapSource::new()
            .with("SEKVENT_LINK_INBOUND_BILLING", BILLING)
            .with("SEKVENT_LINK_INBOUND_ORDERS", BILLING);
        let error = LinkConfig::from_source(&source).unwrap_err();
        match &error {
            ConfigError::Invalid { key, reason } => {
                assert_eq!(key, "SEKVENT_LINK_INBOUND_ORDERS");
                assert!(reason.contains("billing") && reason.contains("orders"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_no_token(&[error]);
    }

    #[test]
    fn link_keys_are_upper_cased() {
        assert_eq!(inbound_key("billing"), "SEKVENT_LINK_INBOUND_BILLING");
        assert_eq!(inbound_key("Order_2"), "SEKVENT_LINK_INBOUND_ORDER_2");
        assert_eq!(outbound_key("shipping"), "SEKVENT_LINK_OUTBOUND_SHIPPING");
        assert_eq!(outbound_key("SHIPPING"), "SEKVENT_LINK_OUTBOUND_SHIPPING");
    }

    #[test]
    fn distinct_tokens_pass_the_check() {
        let source = MapSource::new()
            .with("SEKVENT_LINK_INBOUND_BILLING", BILLING)
            .with("SEKVENT_LINK_INBOUND_ORDERS", ORDERS)
            .with("SEKVENT_LINK_OUTBOUND_SHIPPING", SHIPPING);
        let config = LinkConfig::from_source(&source).unwrap();
        assert_eq!(config.check_distinct_tokens(), Ok(()));
        let empty = LinkConfig::from_source(&MapSource::new()).unwrap();
        assert_eq!(empty.check_distinct_tokens(), Ok(()));
    }

    #[test]
    fn an_inbound_token_reused_outbound_is_refused() {
        let source = MapSource::new()
            .with("SEKVENT_LINK_INBOUND_BILLING", BILLING)
            .with("SEKVENT_LINK_INBOUND_ORDERS", ORDERS)
            .with("SEKVENT_LINK_OUTBOUND_SHIPPING", ORDERS);
        let config = LinkConfig::from_source(&source).unwrap();
        let error = config.check_distinct_tokens().unwrap_err();
        assert_eq!(
            error,
            LinkError::DuplicateToken {
                first: "inbound/orders".into(),
                second: "outbound/shipping".into(),
            }
        );
        assert!(!error.to_string().contains(ORDERS), "{error}");
    }

    #[test]
    fn two_outbound_links_sharing_a_token_are_refused() {
        let source = MapSource::new()
            .with("SEKVENT_LINK_INBOUND_BILLING", BILLING)
            .with("SEKVENT_LINK_OUTBOUND_SHIPPING", SHIPPING)
            .with("SEKVENT_LINK_OUTBOUND_ORDERS", SHIPPING);
        let config = LinkConfig::from_source(&source).unwrap();
        let error = config.check_distinct_tokens().unwrap_err();
        assert_eq!(
            error,
            LinkError::DuplicateToken {
                first: "outbound/orders".into(),
                second: "outbound/shipping".into(),
            }
        );
        assert!(!error.to_string().contains(SHIPPING), "{error}");
    }

    #[test]
    fn scoped_sources_report_full_keys() {
        let inner = MapSource::new().with("ORDERS_SEKVENT_LINK_INBOUND_BILLING", "short");
        let scoped = sekvent_config::Prefixed::new(&inner, "ORDERS_");
        let error = LinkConfig::from_source(&scoped).unwrap_err();
        assert!(matches!(
            &error,
            ConfigError::Invalid { key, .. } if key == "ORDERS_SEKVENT_LINK_INBOUND_BILLING"
        ));
    }
}
