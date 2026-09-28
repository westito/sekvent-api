//! Compile guard for consumers of the `sekvent` facade.
//!
//! This crate depends on `sekvent` and no other sekvent crate, the way a
//! service's manifest does, so `#[derive(EnvConfig)]` here has to find its
//! runtime through `sekvent::config`. If the derive ever names a crate the
//! consumer does not depend on, this crate stops compiling. It is not
//! published.

#![forbid(unsafe_code)]

use sekvent::EnvConfig;
use sekvent::config::{ConfigError, ConfigSource, Secret};

/// Settings of an example `orders` service.
#[derive(Debug, EnvConfig)]
#[config(prefix = "ORDERS_")]
pub struct OrdersConfig {
    /// Port to listen on.
    #[config(default = "8080")]
    pub port: u16,
    /// Token presented to the billing service.
    pub billing_token: Secret,
    /// Database pool settings.
    #[config(nested)]
    pub db: PoolConfig,
}

/// Database pool settings.
#[derive(Debug, EnvConfig)]
pub struct PoolConfig {
    /// Maximum open connections.
    #[config(default = "4")]
    pub max_connections: u32,
}

/// Read and validate [`OrdersConfig`] from `source`, rejecting unknown keys
/// in the reserved `SEKVENT_` namespace.
pub fn load(source: &dyn ConfigSource) -> Result<OrdersConfig, ConfigError> {
    sekvent::config::load(source)
}

#[cfg(test)]
mod tests {
    use sekvent::config::{FromConfig, MapSource};

    use super::*;

    #[test]
    fn a_derived_config_loads_through_the_facade() {
        let source = MapSource::new()
            .with("ORDERS_BILLING_TOKEN", "t0ken")
            .with("ORDERS_DB_MAX_CONNECTIONS", "9")
            .with("SEKVENT_LOG", "info");
        let config = load(&source).unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.billing_token.expose(), "t0ken");
        assert_eq!(config.db.max_connections, 9);
        assert!(!format!("{config:?}").contains("t0ken"));

        let keys: Vec<String> = OrdersConfig::keys()
            .into_iter()
            .map(|info| info.key)
            .collect();
        assert_eq!(
            keys,
            [
                "ORDERS_PORT",
                "ORDERS_BILLING_TOKEN",
                "ORDERS_DB_MAX_CONNECTIONS"
            ]
        );
    }

    #[test]
    fn errors_name_the_keys() {
        let source = MapSource::new().with("SEKVENT_LGO", "info");
        let text = load(&source).unwrap_err().to_string();
        assert!(text.contains("ORDERS_BILLING_TOKEN"), "{text}");
        assert!(
            text.contains("SEKVENT_LGO (did you mean SEKVENT_LOG?)"),
            "{text}"
        );
    }

    #[derive(Debug, EnvConfig)]
    #[config(crate = "::sekvent::config")]
    struct Explicit {
        #[config(default = "on")]
        enabled: bool,
    }

    #[test]
    fn an_explicit_runtime_path_is_honoured() {
        let config = Explicit::from_config(&MapSource::new()).unwrap();
        assert!(config.enabled);
    }
}
