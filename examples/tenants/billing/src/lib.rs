//! The example billing product on `sekvent::tenancy`: every tenant has a
//! Postgres database and a role of its own.
//!
//! - [`BillingScope`] is what a call of a tenant works with: the tenant's
//!   migrated pool, connected as the tenant's role. [`BillingFactory`]
//!   builds it when the tenant cache opens the tenant.
//! - [`TENANT_MIGRATOR`] is the schema of every tenant database
//!   (`migrations/tenant`); the tenant cache and the provisioner run it as
//!   the tenant's role. [`CONTROL_MIGRATOR`] is the control database's
//!   schema (`migrations/control`), a copy of the crate's
//!   [`REFERENCE_SCHEMA`](sekvent::tenancy::control::REFERENCE_SCHEMA).
//! - [`BillingHooks`] is the product's side of the lifecycle: the seed
//!   creates the default invoice series and an owner invitation whose token
//!   is handed over once; `usage` counts invoices; `describe` names the
//!   tenant.
//! - [`InvoicesService`] implements the public `Invoices` component on the
//!   scope of the call; [`TenantAdminService`] implements the product's own
//!   `TenantAdmin` component, each body wrapped in the admin guard and
//!   calling the [`Provisioner`](sekvent::tenancy::lifecycle::Provisioner).

#![forbid(unsafe_code)]

mod admin;
mod hooks;
mod invoices;

pub use admin::TenantAdminService;
pub use hooks::{BillingHooks, BillingSeed};
pub use invoices::InvoicesService;
use sekvent::tenancy::{ScopeFactory, ScopeFuture, TenantId, TenantInfo};
use sqlx::PgPool;
use sqlx::migrate::Migrator;

/// The schema of every tenant database.
pub static TENANT_MIGRATOR: Migrator = sqlx::migrate!("migrations/tenant");

/// The schema of the control database: the crate's reference schema (the
/// tenants, their hosts and the admin audit), copied into a migration of the
/// product's own. Run it as the control database's owner before serving.
pub static CONTROL_MIGRATOR: Migrator = sqlx::migrate!("migrations/control");

/// What a call of one tenant works with.
#[derive(Debug)]
pub struct BillingScope {
    /// The tenant.
    pub tenant: TenantId,
    /// The tenant's migrated pool, connected as the tenant's role.
    pub pool: PgPool,
}

/// Builds a [`BillingScope`] on a tenant's migrated pool.
#[derive(Debug, Default, Clone, Copy)]
pub struct BillingFactory;

impl ScopeFactory for BillingFactory {
    type Scope = BillingScope;

    fn build<'a>(&'a self, info: &'a TenantInfo, pool: PgPool) -> ScopeFuture<'a, BillingScope> {
        Box::pin(std::future::ready(Ok(BillingScope {
            tenant: info.id,
            pool,
        })))
    }
}

#[cfg(test)]
mod tests {
    use sekvent::tenancy::control::REFERENCE_SCHEMA;

    #[test]
    fn the_control_migration_is_the_reference_schema() {
        assert_eq!(
            include_str!("../migrations/control/0001_control.sql"),
            REFERENCE_SCHEMA
        );
    }
}
