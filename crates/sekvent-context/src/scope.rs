//! The scope of one request, as a serving boundary resolved it before it
//! authenticated the caller.

use std::fmt;
use std::sync::Arc;

/// What a serving boundary resolved for one request before it
/// authenticated the caller: the tenant the request belongs to, and typed
/// values its handlers need.
///
/// It reaches handlers through [`CallContext::scope`](crate::CallContext::scope)
/// and [`CallContext::scoped`](crate::CallContext::scoped). A tenant it names
/// becomes the context's tenant
/// ([`CallContext::with_scope`](crate::CallContext::with_scope)) and travels
/// to other services like any tenant; the scope itself and its extensions
/// never leave this process: they are never encoded in headers, a
/// [`child`](crate::CallContext::child) or
/// [`detached`](crate::CallContext::detached) context does not carry them,
/// and `Debug` shows the tenant and only how many extensions there are.
///
/// Extensions are typed, at most one value per type
/// ([`with_extension`](Self::with_extension), [`extension`](Self::extension)).
/// Use a type of your own (a newtype) per meaning, so two sources cannot
/// collide on a shared type such as `String`.
///
/// Cloning shares the tenant and the extensions: a value placed in a scope
/// lives as long as some clone of the scope does.
#[derive(Clone, Default)]
pub struct RequestScope {
    inner: Arc<Inner>,
}

#[derive(Clone, Default)]
struct Inner {
    tenant: Option<String>,
    extensions: http::Extensions,
}

impl RequestScope {
    /// A scope without a tenant or extensions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Name the tenant the request belongs to. It must pass
    /// [`headers::is_valid_tenant`](crate::headers::is_valid_tenant), the
    /// rule the header codec carries tenants by: a serving boundary fails
    /// a call whose resolved scope names any other tenant with `INTERNAL`,
    /// rather than serve it with a tenant that is lost on the next hop.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).tenant = Some(tenant.into());
        self
    }

    /// Attach `value` as the extension of type `T`, replacing an earlier
    /// value of the same type.
    #[must_use]
    pub fn with_extension<T: Clone + Send + Sync + 'static>(mut self, value: T) -> Self {
        Arc::make_mut(&mut self.inner).extensions.insert(value);
        self
    }

    /// The tenant, if the scope names one.
    pub fn tenant(&self) -> Option<&str> {
        self.inner.tenant.as_deref()
    }

    /// The extension of type `T`, if one was attached.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.inner.extensions.get::<T>()
    }
}

impl fmt::Debug for RequestScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestScope")
            .field("tenant", &self.inner.tenant)
            .field("extensions", &self.inner.extensions.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallContext, EndUser, ServiceIdentity, headers};

    /// A tenant's database, as a resolver might attach it.
    #[derive(Debug, Clone, PartialEq)]
    struct TenantDb(&'static str);

    /// A value that must never show up in logs or headers.
    #[derive(Debug, Clone, PartialEq)]
    struct SiteKey(&'static str);

    fn scope() -> RequestScope {
        RequestScope::new()
            .with_tenant("tenant-s")
            .with_extension(TenantDb("db-secret-71"))
            .with_extension(SiteKey("site-key-3e"))
    }

    #[test]
    fn builders_and_getters() {
        let empty = RequestScope::new();
        assert_eq!(empty.tenant(), None);
        assert_eq!(empty.extension::<TenantDb>(), None);

        let scope = scope();
        assert_eq!(scope.tenant(), Some("tenant-s"));
        assert_eq!(
            scope.extension::<TenantDb>(),
            Some(&TenantDb("db-secret-71"))
        );
        assert_eq!(scope.extension::<SiteKey>(), Some(&SiteKey("site-key-3e")));
        assert_eq!(scope.extension::<String>(), None, "absent type");
    }

    #[test]
    fn the_last_insert_of_a_type_wins() {
        let scope = scope()
            .with_extension(TenantDb("db-2"))
            .with_tenant("tenant-t");
        assert_eq!(scope.extension::<TenantDb>(), Some(&TenantDb("db-2")));
        assert_eq!(scope.tenant(), Some("tenant-t"));
    }

    /// A value whose clones are counted.
    #[derive(Clone)]
    struct Counted(Arc<()>);

    #[test]
    fn clones_share_their_values() {
        let marker = Arc::new(());
        let scope = RequestScope::new().with_extension(Counted(Arc::clone(&marker)));
        assert_eq!(Arc::strong_count(&marker), 2);
        let clone = scope.clone();
        assert!(Arc::ptr_eq(&scope.inner, &clone.inner));
        assert_eq!(Arc::strong_count(&marker), 2, "no value is cloned");
        drop(scope);
        assert_eq!(Arc::strong_count(&marker), 2, "the clone keeps it");
        assert!(Arc::ptr_eq(
            &clone.extension::<Counted>().unwrap().0,
            &marker
        ));

        // A builder on a shared scope leaves the other clone as it was.
        let changed = clone.clone().with_tenant("tenant-x");
        assert_eq!(changed.tenant(), Some("tenant-x"));
        assert_eq!(clone.tenant(), None);
        drop(changed);
        drop(clone);
        assert_eq!(
            Arc::strong_count(&marker),
            1,
            "released with the last clone"
        );
    }

    #[test]
    fn debug_shows_the_tenant_and_the_count_only() {
        let shown = format!("{:?}", scope());
        assert!(shown.contains("tenant-s"), "{shown}");
        assert!(shown.contains("extensions: 2"), "{shown}");
        for hidden in ["db-secret-71", "site-key-3e", "TenantDb", "SiteKey"] {
            assert!(!shown.contains(hidden), "{shown}");
        }

        let cx = CallContext::new().with_scope(scope());
        let shown = format!("{cx:?}");
        assert!(shown.contains("tenant-s"), "{shown}");
        assert!(shown.contains("extensions: 2"), "{shown}");
        for hidden in ["db-secret-71", "site-key-3e", "TenantDb", "SiteKey"] {
            assert!(!shown.contains(hidden), "{shown}");
        }
    }

    #[test]
    fn with_scope_sets_a_named_tenant_only() {
        let cx = CallContext::new()
            .with_tenant("tenant-h")
            .with_scope(scope());
        assert_eq!(cx.tenant(), Some("tenant-s"));
        assert_eq!(cx.scope().and_then(RequestScope::tenant), Some("tenant-s"));
        assert_eq!(cx.scoped::<TenantDb>(), Some(&TenantDb("db-secret-71")));
        assert_eq!(cx.scoped::<String>(), None);

        let unnamed = CallContext::new()
            .with_tenant("tenant-h")
            .with_scope(RequestScope::new().with_extension(TenantDb("db")));
        assert_eq!(unnamed.tenant(), Some("tenant-h"), "left as it was");
        assert_eq!(unnamed.scoped::<TenantDb>(), Some(&TenantDb("db")));

        assert!(CallContext::new().scope().is_none());
        assert_eq!(CallContext::new().scoped::<TenantDb>(), None);
    }

    #[test]
    fn child_and_detached_keep_the_tenant_and_drop_the_scope() {
        let cx = CallContext::new().with_scope(scope());
        for derived in [cx.child(), cx.detached()] {
            assert!(derived.scope().is_none());
            assert_eq!(derived.scoped::<TenantDb>(), None);
            assert_eq!(derived.tenant(), Some("tenant-s"));
        }
    }

    #[test]
    fn an_end_user_meets_the_scopes_tenant() {
        // No scope tenant: the user's tenant, or none.
        let cx = CallContext::new()
            .with_scope(RequestScope::new())
            .with_end_user(EndUser::new("u").with_tenant("tenant-u"));
        assert_eq!(cx.tenant(), Some("tenant-u"));
        let cx = CallContext::new()
            .with_scope(RequestScope::new())
            .with_tenant("tenant-h")
            .with_end_user(EndUser::new("u"));
        assert_eq!(cx.tenant(), None);

        // A scope tenant wins over a user without one, the same one and,
        // here, another one: refusing that is the serving boundary's job.
        for user in [
            EndUser::new("u"),
            EndUser::new("u").with_tenant("tenant-s"),
            EndUser::new("u").with_tenant("tenant-u"),
        ] {
            let cx = CallContext::new().with_scope(scope()).with_end_user(user);
            assert_eq!(cx.tenant(), Some("tenant-s"));
            assert_eq!(cx.subject(), Some("u"));
            assert!(cx.scope().is_some(), "the scope stays");
        }
    }

    #[test]
    fn with_scope_after_an_end_user_is_a_plain_setter() {
        // Refusing the mismatch is `App::resolve_scope`'s job.
        let cx = CallContext::new()
            .with_end_user(EndUser::new("u").with_tenant("tenant-u"))
            .with_scope(scope());
        assert_eq!(cx.tenant(), Some("tenant-s"));
        assert_eq!(cx.end_user().and_then(EndUser::tenant), Some("tenant-u"));
        assert_eq!(cx.subject(), Some("u"));
    }

    #[test]
    fn sanitizing_keeps_the_scopes_tenant_only() {
        let anonymous = CallContext::new()
            .with_subject("user-7")
            .with_client_addr(std::net::IpAddr::from([203, 0, 113, 9]))
            .with_scope(scope())
            .sanitize_for_caller();
        assert_eq!(anonymous.tenant(), Some("tenant-s"));
        assert_eq!(anonymous.subject(), None);
        assert_eq!(anonymous.client_addr(), None);

        let unnamed = CallContext::new()
            .with_tenant("tenant-h")
            .with_scope(RequestScope::new())
            .sanitize_for_caller();
        assert_eq!(unnamed.tenant(), None, "a header's tenant is dropped");

        let untrusted = CallContext::new()
            .with_scope(scope())
            .with_caller(ServiceIdentity::untrusted("orders"))
            .sanitize_for_caller();
        assert_eq!(untrusted.tenant(), Some("tenant-s"));
    }

    #[test]
    fn a_client_header_loses_to_the_scope() {
        let mut map = http::HeaderMap::new();
        map.insert(headers::TENANT, http::HeaderValue::from_static("tenant-h"));
        let cx = headers::from_headers(&map, None);
        assert_eq!(cx.tenant(), None, "dropped without a trusted caller");
        let cx = cx.with_scope(scope());
        assert_eq!(cx.tenant(), Some("tenant-s"));
    }

    #[test]
    fn no_extension_goes_on_the_wire() {
        let cx = CallContext::new().with_scope(scope());
        let mut injected = http::HeaderMap::new();
        headers::inject(&cx, &mut injected);
        assert_eq!(injected[headers::TENANT], "tenant-s");
        let mut propagated = http::HeaderMap::new();
        headers::propagate(&cx, &mut propagated);
        assert_eq!(propagated[headers::TENANT], "tenant-s");
        let mut external = http::HeaderMap::new();
        headers::propagate_external(&cx, &mut external);
        assert!(!external.contains_key(headers::TENANT));

        for map in [&injected, &propagated, &external] {
            for (name, value) in map {
                let value = value.to_str().unwrap();
                for secret in ["db-secret-71", "site-key-3e"] {
                    assert!(!value.contains(secret), "{name}: {value}");
                }
            }
        }
    }
}
