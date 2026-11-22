//! The scope resolver of an App that serves tenants.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use http::request::Parts;
use sekvent_component::ScopeResolver;
use sekvent_context::{CallContext, RequestScope};
use sekvent_error::AppError;

use crate::{TenantId, Tenants};

/// What the scope holds for a streaming method instead of a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTenant(pub TenantId);

/// The `ScopeResolver` of an App that serves tenants.
///
/// The scope names the tenant by `TenantId`'s text and holds its `TenantLease`, or a
/// [`StreamTenant`] for the listed streaming methods; a bypass path (the bypass prefixes of the
/// `Tenants` it was built over) gets an empty scope. Selection goes through
/// [`Tenants::select`], so a refusal reaches the caller as `Tenants` answers it: serving reasons
/// in the product's serving domain, a product refusal unchanged. It logs nothing and opens no
/// span.
pub struct TenantResolver<S> {
    /// The tenants this process serves.
    tenants: Tenants<S>,
    /// RPC paths that get the id only, each with or without one leading `/`.
    streaming: &'static [&'static str],
}

impl<S: Send + Sync + 'static> TenantResolver<S> {
    /// `streaming`: RPC paths that get the id only, written `<package>.<Service>/<Rpc>`
    /// (`billing.v1.Invoices/Watch`); an entry with one leading `/` matches the same method. An
    /// entry is compared whole with the routed path of the call (`request.uri.path()`,
    /// `/<package>.<Service>/<Rpc>`), never as a prefix and never percent-decoded.
    pub fn new(tenants: Tenants<S>, streaming: &'static [&'static str]) -> Self {
        Self { tenants, streaming }
    }

    /// Whether `path` (`/<package>.<Service>/<Rpc>`) is one of the streaming paths; one leading
    /// `/` is taken off both the path and each entry before they are compared.
    fn is_streaming(&self, path: &str) -> bool {
        path.strip_prefix('/').is_some_and(|rpc| {
            self.streaming
                .iter()
                .any(|&entry| entry.strip_prefix('/').unwrap_or(entry) == rpc)
        })
    }
}

impl<S: Send + Sync + 'static> ScopeResolver for TenantResolver<S> {
    fn resolve<'a>(
        &'a self,
        request: &'a Parts,
        _cx: &'a CallContext,
    ) -> Pin<Box<dyn Future<Output = Result<RequestScope, AppError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(id) = self.tenants.select(request).await? else {
                return Ok(RequestScope::new());
            };
            let scope = RequestScope::new().with_tenant(id.to_string());
            if self.is_streaming(request.uri.path()) {
                Ok(scope.with_extension(StreamTenant(id)))
            } else {
                Ok(scope.with_extension(self.tenants.get(id).await?))
            }
        })
    }
}

// `TenantResolver(..)`
impl<S> fmt::Debug for TenantResolver<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TenantResolver(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TenantLease;

    const UNARY: &str = "/billing.orders.v1.Orders/Get";
    const STREAM: &str = "billing.orders.v1.Orders/Watch";
    const HEALTH: &str = "/billing.health.v1.Health/";

    /// A scope type that is not `Clone`.
    #[derive(Debug)]
    struct Marker(u32);

    fn head(path: &str) -> Parts {
        http::Request::builder()
            .uri(path)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    async fn resolve(resolver: &TenantResolver<Marker>, path: &str) -> RequestScope {
        resolver
            .resolve(&head(path), &CallContext::new())
            .await
            .expect("the fixed form resolves")
    }

    fn assert_empty(scope: &RequestScope) {
        assert_eq!(scope.tenant(), None);
        assert!(scope.extension::<TenantLease<Marker>>().is_none());
        assert_eq!(scope.extension::<StreamTenant>(), None);
    }

    #[tokio::test]
    async fn a_unary_path_gets_the_local_tenant_and_its_lease() {
        let resolver = TenantResolver::new(Tenants::fixed(Marker(7)), &[STREAM]);
        let scope = resolve(&resolver, UNARY).await;
        let local = TenantId::LOCAL.to_string();
        assert_eq!(scope.tenant(), Some(local.as_str()));
        let lease = scope
            .extension::<TenantLease<Marker>>()
            .expect("a lease in the scope");
        assert_eq!(lease.0, 7);
        assert_eq!(lease.id(), TenantId::LOCAL);
        assert_eq!(scope.extension::<StreamTenant>(), None);
    }

    #[tokio::test]
    async fn a_streaming_path_gets_the_id_only() {
        let resolver = TenantResolver::new(Tenants::fixed(Marker(7)), &[STREAM]);
        let scope = resolve(&resolver, &format!("/{STREAM}")).await;
        let local = TenantId::LOCAL.to_string();
        assert_eq!(scope.tenant(), Some(local.as_str()));
        assert_eq!(
            scope.extension::<StreamTenant>(),
            Some(&StreamTenant(TenantId::LOCAL))
        );
        assert!(scope.extension::<TenantLease<Marker>>().is_none());
        let unlisted = resolve(&resolver, "/billing.orders.v1.Orders/Follow").await;
        assert!(unlisted.extension::<TenantLease<Marker>>().is_some());
        let without_slash = resolve(&resolver, &format!("/x/{STREAM}")).await;
        assert!(without_slash.extension::<TenantLease<Marker>>().is_some());
    }

    #[tokio::test]
    async fn a_streaming_entry_matches_with_or_without_a_leading_slash() {
        let plain: &'static [&'static str] = &[STREAM];
        let slashed: &'static [&'static str] = &["/billing.orders.v1.Orders/Watch"];
        for listed in [plain, slashed] {
            let resolver = TenantResolver::new(Tenants::fixed(Marker(7)), listed);
            let scope = resolve(&resolver, "/billing.orders.v1.Orders/Watch").await;
            assert_eq!(
                scope.extension::<StreamTenant>(),
                Some(&StreamTenant(TenantId::LOCAL)),
                "{listed:?}"
            );
            assert!(scope.extension::<TenantLease<Marker>>().is_none());
            let other = resolve(&resolver, "/billing.orders.v1.Orders/Follow").await;
            assert_eq!(other.extension::<StreamTenant>(), None, "{listed:?}");
            assert!(other.extension::<TenantLease<Marker>>().is_some());
            let longer = resolve(&resolver, "/billing.orders.v1.Orders/WatchAll").await;
            assert!(longer.extension::<TenantLease<Marker>>().is_some());
        }
        let twice: &'static [&'static str] = &["//billing.orders.v1.Orders/Watch"];
        let resolver = TenantResolver::new(Tenants::fixed(Marker(7)), twice);
        let scope = resolve(&resolver, "/billing.orders.v1.Orders/Watch").await;
        assert!(scope.extension::<TenantLease<Marker>>().is_some());
    }

    #[tokio::test]
    async fn a_slashed_streaming_entry_leaves_the_bypass_check_alone() {
        let listed: &'static [&'static str] = &["/billing.health.v1.Health/Watch"];
        let resolver =
            TenantResolver::new(Tenants::fixed_with_bypass(Marker(7), &[HEALTH]), listed);
        assert_empty(&resolve(&resolver, "/billing.health.v1.Health/Watch").await);
        assert_empty(&resolve(&resolver, "/billing.health.v1.Health/Check").await);
        let scope = resolve(&resolver, UNARY).await;
        assert!(scope.extension::<TenantLease<Marker>>().is_some());
    }

    #[tokio::test]
    async fn a_bypass_path_gets_an_empty_scope_on_the_fixed_form() {
        let resolver =
            TenantResolver::new(Tenants::fixed_with_bypass(Marker(7), &[HEALTH]), &[STREAM]);
        assert_empty(&resolve(&resolver, "/billing.health.v1.Health/Check").await);
        let listed: &'static [&'static str] = &["billing.health.v1.Health/Watch"];
        let streaming =
            TenantResolver::new(Tenants::fixed_with_bypass(Marker(7), &[HEALTH]), listed);
        assert_empty(&resolve(&streaming, "/billing.health.v1.Health/Watch").await);
        let outside = resolve(&resolver, UNARY).await;
        let local = TenantId::LOCAL.to_string();
        assert_eq!(outside.tenant(), Some(local.as_str()));
        let lease = outside
            .extension::<TenantLease<Marker>>()
            .expect("a lease in the scope");
        assert_eq!(lease.0, 7);
    }

    #[test]
    fn debug_prints_the_type_name_only() {
        let resolver = TenantResolver::new(Tenants::fixed(Marker(7)), &[STREAM]);
        assert_eq!(format!("{resolver:?}"), "TenantResolver(..)");
    }
}
