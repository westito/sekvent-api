//! Lifecycle hooks and a guarded admin method of an example `inventory`
//! product, declared through `sekvent::tenancy` alone.

use std::collections::BTreeMap;

use sekvent::prelude::*;
use sekvent::tenancy::admin::{AdminAudit, AdminClaims, guard};
use sekvent::tenancy::lifecycle::{Description, HookFuture, SeedOutcome, TenantHooks};
use sekvent::tenancy::{TenantId, TenantInfo};

/// The admin operation [`suspend`] runs as: the RPC path without its
/// leading slash.
pub const SUSPEND: &str = "check.admin.v1.TenantAdmin/Suspend";

/// What a new `inventory` tenant is seeded with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventorySeed {
    /// The tenant's first warehouse.
    pub warehouse: String,
}

/// The `inventory` product's hooks: the seed hands a welcome code over,
/// `describe` adds the product's name. Adoption keeps the default refusal.
#[derive(Debug, Default)]
pub struct InventoryHooks;

impl TenantHooks for InventoryHooks {
    type Seed = InventorySeed;

    fn seed<'a>(
        &'a self,
        tenant: &'a TenantInfo,
        _pool: &'a sqlx::PgPool,
        seed: &'a InventorySeed,
    ) -> HookFuture<'a, SeedOutcome> {
        let code = Secret::new(format!("{}:{}", tenant.id, seed.warehouse));
        Box::pin(std::future::ready(Ok(SeedOutcome::with_handover(code))))
    }

    fn describe<'a>(
        &'a self,
        tenant: &'a TenantInfo,
        _pool: &'a sqlx::PgPool,
    ) -> HookFuture<'a, Description> {
        Box::pin(std::future::ready(Ok(Description {
            display_name: tenant.display_name.clone(),
            attributes: BTreeMap::from([("product".to_owned(), "inventory".to_owned())]),
        })))
    }
}

/// The body of an admin `Suspend` RPC for `tenant`, run in the admin guard:
/// the subject the token was minted for, once the token is checked and
/// recorded.
pub async fn suspend(
    audit: &AdminAudit,
    cx: &CallContext,
    tenant: TenantId,
) -> Result<String, AppError> {
    guard::run(
        audit,
        cx,
        SUSPEND,
        Some(tenant),
        |claims: AdminClaims| async move { Ok(claims.subject) },
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sekvent::context::SystemClock;
    use sekvent::tenancy::lifecycle::AdoptCandidate;
    use sekvent::tenancy::{DatabaseName, reasons};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::*;

    /// A pool that never connects: nobody listens on port 1.
    fn lazy_pool() -> sqlx::PgPool {
        PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new().host("127.0.0.1").port(1))
    }

    #[tokio::test]
    async fn the_hooks_answer_without_the_database() {
        let pool = lazy_pool();
        let tenant = TenantInfo::local();
        let seed = InventorySeed {
            warehouse: "north".to_owned(),
        };
        let seeded = InventoryHooks
            .seed(&tenant, &pool, &seed)
            .await
            .expect("seeded");
        assert!(!format!("{seeded:?}").contains("north"));
        let code = seeded.handover.expect("a welcome code");
        assert!(code.expose().ends_with(":north"));

        let description = InventoryHooks
            .describe(&tenant, &pool)
            .await
            .expect("described");
        assert_eq!(description.attributes["product"], "inventory");

        let candidate = AdoptCandidate::new(
            TenantId::LOCAL,
            DatabaseName::parse("legacy_stock").expect("a database name"),
            "stock",
            "Stock",
        );
        let refused = InventoryHooks
            .check_adoptable(&candidate, &pool)
            .await
            .expect_err("the default refuses");
        assert_eq!(refused.code(), ErrorCode::FailedPrecondition);
        assert_eq!(refused.reason(), Some(reasons::ADOPTION_REFUSED));
        assert_eq!(pool.size(), 0, "no hook touched the pool");
    }

    #[tokio::test]
    async fn the_guard_refuses_a_call_without_admin_claims() {
        let audit = AdminAudit::new(lazy_pool(), Arc::new(SystemClock));
        let error = suspend(&audit, &CallContext::new(), TenantId::LOCAL)
            .await
            .expect_err("no admin token");
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.reason(), Some(reasons::ADMIN_TOKEN_INVALID));
    }
}
