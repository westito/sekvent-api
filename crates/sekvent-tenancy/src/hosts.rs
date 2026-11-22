//! The host snapshot of the active tenants and the CORS predicate built on it.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use http::header::ORIGIN;
use http::request::Parts;
use sekvent_error::AppError;

use crate::host::host_of_origin;
use crate::select::request_host;
use crate::{Registry, SelectorRules, TenantId, TenantInfo};

/// Host to tenant, in memory, replaced as a whole: every host of every active tenant as of the
/// last [`refresh_hosts`] (or what the last [`replace`](Self::replace) was given). Starts empty;
/// the invalidation listener empties it after repeated failed reloads. Clones share the map.
/// `Debug` prints the number of hosts only.
#[derive(Clone, Default)]
pub struct HostSnapshot {
    /// The map and the reload lock, shared by every clone.
    inner: Arc<Inner>,
}

/// What the clones of a snapshot share.
#[derive(Default)]
struct Inner {
    /// Canonical host to tenant. Never held across an await.
    map: RwLock<HashMap<String, TenantId>>,
    /// Serialises reloads and `clear`.
    reload: tokio::sync::Mutex<()>,
}

impl HostSnapshot {
    /// An empty snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// The tenant that holds exactly `host`; the text is compared as given, not canonicalised.
    pub fn tenant_of(&self, host: &str) -> Option<TenantId> {
        self.read().get(host).copied()
    }

    /// The number of hosts.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether the snapshot holds no host.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// Replaces the whole map at once; for a repeated host the last pair wins.
    pub fn replace(&self, hosts: impl IntoIterator<Item = (String, TenantId)>) {
        let map: HashMap<String, TenantId> = hosts.into_iter().collect();
        *self.write() = map;
    }

    /// Empties the map, serialised with reloads.
    pub(crate) async fn clear(&self) {
        let _reload = self.inner.reload.lock().await;
        self.write().clear();
    }

    /// The map for reading; a poisoned lock still holds a whole map.
    fn read(&self) -> RwLockReadGuard<'_, HashMap<String, TenantId>> {
        self.inner
            .map
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The map for writing; a poisoned lock still holds a whole map.
    fn write(&self) -> RwLockWriteGuard<'_, HashMap<String, TenantId>> {
        self.inner
            .map
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

// `HostSnapshot { hosts: 3, .. }`
impl fmt::Debug for HostSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostSnapshot")
            .field("hosts", &self.len())
            .finish_non_exhaustive()
    }
}

/// Reads the active tenants with one statement (`Registry::active`), returns the connection, then
/// swaps the map. Returns what it read. On failure the old map stays. Reloads are serialised across
/// clones.
pub async fn refresh_hosts(
    snapshot: &HostSnapshot,
    registry: &Registry,
) -> Result<Vec<TenantInfo>, AppError> {
    let _reload = snapshot.inner.reload.lock().await;
    let active = registry.active().await?;
    snapshot.replace(
        active
            .iter()
            .flat_map(|info| info.hosts.iter().map(move |host| (host.clone(), info.id))),
    );
    Ok(active)
}

/// The function for `Cors::origin_predicate`: synchronous, no I/O. It answers `true` only when
/// the request's host (the URI authority and the one `Host` field, canonical) is one of the
/// id-header hosts of `rules`, the request carries exactly one `Origin` field, and the host of
/// `origin` is in `snapshot`. It never looks at the path. Both arguments are taken by value and
/// moved into the returned function, which borrows nothing; a later `replace` or refresh of the
/// snapshot is seen by it.
///
/// It allows the origin of any tenant in the snapshot, not only of the tenant the request names.
/// For an id-header selection the resolver then checks the `Origin` against the selected tenant's
/// own hosts (`ORIGIN_NOT_ALLOWED`). A path-label selection carries no origin check: on a
/// loopback host that is in both host lists, the predicate answers for a path-label request too.
pub fn predicate(
    snapshot: HostSnapshot,
    rules: SelectorRules,
) -> impl Fn(&str, &Parts) -> bool + Send + Sync + 'static {
    move |origin: &str, request: &Parts| -> bool {
        request_host(request).is_some_and(|host| rules.is_id_header_host(&host))
            && request.headers.get_all(ORIGIN).iter().count() == 1
            && host_of_origin(origin).is_some_and(|host| snapshot.tenant_of(&host).is_some())
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderName;
    use uuid::Uuid;

    use super::*;

    const API: &str = "api.example.com";
    const GO: &str = "go.example.com";

    fn tenant(n: u128) -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(n))
    }

    fn rules() -> SelectorRules {
        SelectorRules::new()
            .id_header(HeaderName::from_static("x-tenant-id"), [API.to_owned()])
            .unwrap()
            .path_label("/t/", [GO.to_owned()])
            .unwrap()
            .tenant_hosts(true)
    }

    /// The active tenants of the predicate tests: two hosts of tenant 1, one of tenant 2, one
    /// loopback host of tenant 3.
    fn snapshot() -> HostSnapshot {
        let snapshot = HostSnapshot::new();
        snapshot.replace([
            ("billing.example.com".to_owned(), tenant(1)),
            ("pay.example.com".to_owned(), tenant(1)),
            ("orders.example.com".to_owned(), tenant(2)),
            ("orders.localhost:3000".to_owned(), tenant(3)),
        ]);
        snapshot
    }

    /// A head to `host` at `path` with these `Origin` fields.
    fn head(host: Option<&str>, path: &str, origins: &[&str]) -> Parts {
        let mut builder = http::Request::builder().uri(path);
        if let Some(host) = host {
            builder = builder.header(http::header::HOST, host);
        }
        for origin in origins {
            builder = builder.header(ORIGIN, *origin);
        }
        builder.body(()).unwrap().into_parts().0
    }

    /// Asks `allowed` with a head carrying `origin` once.
    fn ask(allowed: &impl Fn(&str, &Parts) -> bool, host: &str, path: &str, origin: &str) -> bool {
        allowed(origin, &head(Some(host), path, &[origin]))
    }

    #[test]
    fn a_snapshot_starts_empty() {
        let snapshot = HostSnapshot::new();
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.len(), 0);
        assert_eq!(snapshot.tenant_of("billing.example.com"), None);
        assert_eq!(format!("{snapshot:?}"), "HostSnapshot { hosts: 0, .. }");
    }

    #[test]
    fn replace_swaps_the_whole_map_and_the_last_pair_wins() {
        let snapshot = snapshot();
        assert_eq!(snapshot.len(), 4);
        assert!(!snapshot.is_empty());
        snapshot.replace([
            ("inventory.example.com".to_owned(), tenant(4)),
            ("inventory.example.com".to_owned(), tenant(5)),
        ]);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.tenant_of("inventory.example.com"), Some(tenant(5)));
        for old in [
            "billing.example.com",
            "pay.example.com",
            "orders.example.com",
        ] {
            assert_eq!(snapshot.tenant_of(old), None, "{old:?}");
        }
    }

    #[test]
    fn clones_share_the_map() {
        let snapshot = HostSnapshot::new();
        let clone = snapshot.clone();
        clone.replace([("billing.example.com".to_owned(), tenant(1))]);
        assert_eq!(snapshot.tenant_of("billing.example.com"), Some(tenant(1)));
        assert_eq!(snapshot.len(), 1);
    }

    #[test]
    fn hosts_are_compared_as_given() {
        let snapshot = snapshot();
        assert_eq!(snapshot.tenant_of("billing.example.com"), Some(tenant(1)));
        assert_eq!(snapshot.tenant_of("Billing.example.com"), None);
        assert_eq!(snapshot.tenant_of("billing.example.com."), None);
        assert_eq!(snapshot.tenant_of("example.com"), None);
    }

    #[test]
    fn debug_prints_the_size_only() {
        let text = format!("{:?}", snapshot());
        assert_eq!(text, "HostSnapshot { hosts: 4, .. }");
        assert!(!text.contains("example"));
    }

    #[tokio::test]
    async fn clear_empties_every_clone() {
        let snapshot = snapshot();
        let clone = snapshot.clone();
        clone.clear().await;
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.tenant_of("billing.example.com"), None);
    }

    #[test]
    fn the_predicate_allows_the_hosts_of_active_tenants_on_an_id_header_host() {
        let allowed = predicate(snapshot(), rules());
        let path = "/billing.orders.v1.Orders/Get";
        assert!(ask(&allowed, API, path, "https://billing.example.com"));
        assert!(ask(&allowed, API, path, "https://pay.example.com"));
        assert!(ask(&allowed, API, path, "https://pay.example.com:443"));
        assert!(ask(&allowed, API, path, "https://orders.example.com"));
        assert!(ask(&allowed, API, path, "http://orders.localhost:3000"));
    }

    #[test]
    fn the_predicate_refuses_everything_else() {
        let allowed = predicate(snapshot(), rules());
        let path = "/billing.orders.v1.Orders/Get";
        for origin in [
            "https://inventory.example.com",
            "http://billing.example.com",
            "https://billing.example.com/",
            "https://billing.example.com/path",
            "https://billing.example.com:8443",
            "http://orders.localhost:3001",
            "https://example.com",
        ] {
            assert!(!ask(&allowed, API, path, origin), "{origin:?}");
        }
        let origin = "https://billing.example.com";
        assert!(!ask(&allowed, GO, path, origin));
        assert!(!ask(&allowed, GO, "/t/billing/x", origin));
        assert!(!ask(&allowed, "billing.example.com", path, origin));
        assert!(!ask(&allowed, "other.example.com", path, origin));
        let twice = head(Some(API), path, &[origin, origin]);
        assert!(!allowed(origin, &twice));
        let no_host = head(None, path, &[origin]);
        assert!(!allowed(origin, &no_host));
    }

    #[test]
    fn the_predicate_never_looks_at_the_path() {
        let allowed = predicate(snapshot(), rules());
        let origin = "https://billing.example.com";
        for path in [
            "/billing.orders.v1.Orders/Get",
            "/t/billing/x",
            "/api/t/orders/billing.orders.v1.Orders/Get",
            "/",
        ] {
            assert!(ask(&allowed, API, path, origin), "{path:?}");
        }
    }

    #[test]
    fn the_predicate_allows_a_registered_loopback_origin_with_its_port() {
        let rules = SelectorRules::new()
            .id_header(
                HeaderName::from_static("x-tenant-id"),
                ["localhost:8080".to_owned()],
            )
            .unwrap();
        let allowed = predicate(snapshot(), rules);
        assert!(ask(
            &allowed,
            "localhost:8080",
            "/x",
            "http://orders.localhost:3000"
        ));
        assert!(!ask(
            &allowed,
            "localhost:8080",
            "/x",
            "http://orders.localhost:3002"
        ));
        assert!(!ask(
            &allowed,
            "localhost:8081",
            "/x",
            "http://orders.localhost:3000"
        ));
    }

    #[test]
    fn the_predicate_sees_a_later_replace() {
        let snapshot = snapshot();
        let allowed = predicate(snapshot.clone(), rules());
        let origin = "https://inventory.example.com";
        assert!(!ask(&allowed, API, "/x", origin));
        snapshot.replace([("inventory.example.com".to_owned(), tenant(4))]);
        assert!(ask(&allowed, API, "/x", origin));
        assert!(!ask(&allowed, API, "/x", "https://billing.example.com"));
    }

    #[tokio::test]
    async fn the_predicate_owns_what_it_needs() {
        type Predicate = Box<dyn Fn(&str, &Parts) -> bool + Send + Sync + 'static>;
        let allowed: Predicate = Box::new(predicate(snapshot(), rules()));
        let origin = "https://pay.example.com";
        let answer =
            tokio::spawn(async move { allowed(origin, &head(Some(API), "/x", &[origin])) })
                .await
                .unwrap();
        assert!(answer);
    }
}
