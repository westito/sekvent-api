//! A tenant scope factory and the tenant resolver of an example `inventory`
//! product, declared through `sekvent::tenancy` alone.

use sekvent::component::ScopeResolver;
use sekvent::error::AppError;
use sekvent::tenancy::{
    Connector, ObjectNames, Registry, ScopeFactory, ScopeFuture, SelectorRules, TenantInfo,
    TenantResolver, Tenants,
};

/// The RPC paths whose calls get the tenant id instead of a lease.
const STREAMING: &[&str] = &["check.echo.v1.Echo/Pings"];

/// The scope of an `inventory` tenant: its migrated pool.
pub struct InventoryScope {
    /// The tenant's pool, connected as the tenant's role.
    pub pool: sqlx::PgPool,
}

/// Builds an [`InventoryScope`] on a tenant's pool.
#[derive(Debug, Default)]
pub struct InventoryFactory;

impl ScopeFactory for InventoryFactory {
    type Scope = InventoryScope;

    fn build<'a>(
        &'a self,
        _info: &'a TenantInfo,
        pool: sqlx::PgPool,
    ) -> ScopeFuture<'a, InventoryScope> {
        Box::pin(std::future::ready(Ok(InventoryScope { pool })))
    }
}

/// The keyed tenant cache of the `inventory` product.
pub fn inventory_tenants(
    registry: Registry,
    connector: Connector,
    names: ObjectNames,
    rules: SelectorRules,
    migrator: &'static sqlx::migrate::Migrator,
) -> Result<Tenants<InventoryScope>, AppError> {
    Tenants::builder(
        registry,
        connector,
        names,
        rules,
        migrator,
        InventoryFactory,
    )
    .build()
}

/// The resolver of an App over `tenants`: the echo service's `Pings` stream
/// gets the tenant id only.
pub fn resolver<S: Send + Sync + 'static>(tenants: Tenants<S>) -> impl ScopeResolver {
    TenantResolver::new(tenants, STREAMING)
}

#[cfg(test)]
mod tests {
    use sekvent::component::{CallContext, RequestScope, ScopeResolver};
    use sekvent::tenancy::{StreamTenant, TenantId, TenantLease, Tenants};

    /// A scope type that is not `Clone`: the lease is `Clone` by itself.
    #[derive(Debug)]
    struct Marker(u32);

    fn head(path: &str) -> http::request::Parts {
        http::Request::builder()
            .uri(format!("http://inventory.example.com{path}"))
            .body(())
            .expect("the request head is valid")
            .into_parts()
            .0
    }

    async fn resolve(resolver: &impl ScopeResolver, path: &str) -> RequestScope {
        resolver
            .resolve(&head(path), &CallContext::new())
            .await
            .expect("the fixed tenant resolves")
    }

    #[tokio::test]
    async fn a_unary_call_gets_a_lease_of_the_fixed_scope() {
        let resolver = super::resolver(Tenants::fixed(Marker(7)));
        let scope = resolve(&resolver, "/check.echo.v1.Echo/Ping").await;

        let local = TenantId::LOCAL.to_string();
        assert_eq!(scope.tenant(), Some(local.as_str()));
        let lease = scope
            .extension::<TenantLease<Marker>>()
            .expect("a unary call holds a lease");
        assert_eq!(lease.id(), TenantId::LOCAL);
        let marker: &Marker = lease;
        assert_eq!(marker.0, 7);
        let copy = lease.clone();
        let copied: &Marker = &copy;
        assert_eq!(copied.0, 7);
        assert_eq!(scope.extension::<StreamTenant>(), None);
    }

    #[tokio::test]
    async fn a_listed_stream_gets_the_tenant_id_only() {
        let resolver = super::resolver(Tenants::fixed(Marker(7)));
        let scope = resolve(&resolver, "/check.echo.v1.Echo/Pings").await;

        let local = TenantId::LOCAL.to_string();
        assert_eq!(scope.tenant(), Some(local.as_str()));
        assert_eq!(
            scope.extension::<StreamTenant>(),
            Some(&StreamTenant(TenantId::LOCAL))
        );
        assert!(scope.extension::<TenantLease<Marker>>().is_none());
    }

    #[tokio::test]
    async fn a_bypass_path_gets_an_empty_scope_on_the_fixed_form() {
        let tenants = Tenants::fixed_with_bypass(Marker(1), &["/check.health.v1.Health/"]);
        let resolver = super::resolver(tenants);
        let scope = resolve(&resolver, "/check.health.v1.Health/Check").await;

        assert_eq!(scope.tenant(), None);
        assert!(scope.extension::<TenantLease<Marker>>().is_none());
        assert_eq!(scope.extension::<StreamTenant>(), None);
    }
}
