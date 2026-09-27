//! Configuration sources.
use std::collections::BTreeMap;

/// Where configuration values come from.
pub trait ConfigSource: Send + Sync {
    /// The raw value of `key`, if set.
    fn get(&self, key: &str) -> Option<String>;
    /// Every key currently set (for unknown-key detection).
    fn keys(&self) -> Vec<String>;
    /// The name to show in error messages for `key` (the full name when
    /// this source is a scoped view).
    fn describe(&self, key: &str) -> String {
        key.to_owned()
    }
}

/// The process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvSource;

impl ConfigSource for EnvSource {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    fn keys(&self) -> Vec<String> {
        std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .collect()
    }
}

/// An in-memory source, for tests and for layering defaults.
#[derive(Debug, Clone, Default)]
pub struct MapSource(BTreeMap<String, String>);

impl MapSource {
    /// An empty source.
    pub fn new() -> Self {
        Self::default()
    }
    /// Add a key (builder style).
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }
    /// Add or replace a key.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.insert(key.into(), value.into());
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for MapSource {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl ConfigSource for MapSource {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn keys(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }
}

/// A view of another source under a key prefix: `Prefixed::new(&env, "BILLING_")`
/// maps key `DB_URL` to `BILLING_DB_URL`.
pub struct Prefixed<'a> {
    inner: &'a dyn ConfigSource,
    prefix: String,
}

impl<'a> Prefixed<'a> {
    /// Scope `inner` under `prefix` (used verbatim, include the separator).
    pub fn new(inner: &'a dyn ConfigSource, prefix: impl Into<String>) -> Self {
        Self {
            inner,
            prefix: prefix.into(),
        }
    }
    /// The full key name for a relative key (for error messages).
    pub fn full_key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }
}

impl ConfigSource for Prefixed<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.inner.get(&self.full_key(key))
    }
    fn keys(&self) -> Vec<String> {
        self.inner
            .keys()
            .into_iter()
            .filter_map(|key| key.strip_prefix(&self.prefix).map(str::to_owned))
            .collect()
    }
    fn describe(&self, key: &str) -> String {
        self.inner.describe(&self.full_key(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_source_builders() {
        let mut map = MapSource::new().with("A", "1");
        map.set("B", "2");
        map.set("A", "3");
        assert_eq!(map.get("A").as_deref(), Some("3"));
        assert_eq!(map.get("B").as_deref(), Some("2"));
        assert_eq!(map.get("C"), None);
        assert_eq!(map.keys(), ["A", "B"]);
        assert_eq!(map.describe("A"), "A");

        let collected: MapSource = [("X", "1")].into_iter().collect();
        assert_eq!(collected.keys(), ["X"]);
    }

    #[test]
    fn prefixed_view() {
        let base = MapSource::new()
            .with("BILLING_URL", "u")
            .with("BILLING_DB_PORT", "1")
            .with("ORDERS_URL", "o");
        let billing = Prefixed::new(&base, "BILLING_");
        assert_eq!(billing.get("URL").as_deref(), Some("u"));
        assert_eq!(billing.get("ORDERS_URL"), None);
        assert_eq!(billing.keys(), ["DB_PORT", "URL"]);
        assert_eq!(billing.full_key("URL"), "BILLING_URL");
        assert_eq!(billing.describe("URL"), "BILLING_URL");

        let db = Prefixed::new(&billing, "DB_");
        assert_eq!(db.get("PORT").as_deref(), Some("1"));
        assert_eq!(db.keys(), ["PORT"]);
        assert_eq!(db.describe("PORT"), "BILLING_DB_PORT");
    }

    #[test]
    fn env_source_reads_the_process_environment() {
        let env = EnvSource;
        let keys = env.keys();
        let expected: Vec<String> = std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .collect();
        assert_eq!(keys, expected);
        let first_unicode = std::env::vars_os()
            .find_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)));
        if let Some((key, value)) = first_unicode {
            assert_eq!(env.get(&key), Some(value));
        }
        assert_eq!(env.get("SEKVENT_CONFIG_TEST_SURELY_UNSET_KEY"), None);
    }
}
