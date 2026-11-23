//! A tenant's usage and its description for an operator: read on the tenant's pool for the
//! operation, without the lifecycle admission.

use sekvent_error::AppError;

use super::hooks::{Description, TenantHooks};
use super::open::{LifecycleCapability, OpenMode, open};
use super::provisioner::{Provisioner, Usage};
use crate::cluster::databases;
use crate::error::tenant_state;
use crate::{TenantId, TenantRow, TenantState};

/// Whether a tenant in `state` may be measured or described.
fn readable(state: TenantState) -> bool {
    matches!(state, TenantState::Active | TenantState::Suspended)
}

impl<H: TenantHooks> Provisioner<H> {
    /// The database's size (`databases::size` on the maintenance connection) and the product's
    /// counters (`TenantHooks::usage`). The row is read as committed now; `TENANT_NOT_FOUND`
    /// without one, `TENANT_STATE` unless it is `active` or `suspended` (both are accepted; a
    /// product that refuses a suspended tenant does so in its own adapter).
    ///
    /// The hook runs on the warm entry's pool when this process's tenant cache holds the
    /// `active` tenant (the cache's sessions), and otherwise on a pool opened for the operation
    /// whose sessions start read-only and whose statements time out after 5 s; that pool holds a
    /// slot of the cache's capacity (`TENANT_BUSY` when the cache is full).
    pub async fn usage(&self, tenant: TenantId) -> Result<Usage, AppError> {
        let row = self.usage_row(tenant).await?;
        let pool = open(self, &LifecycleCapability::new(), &row, OpenMode::ReadOnly).await?;
        let counters = self.inner.hooks.usage(&row.info, pool.pool()).await;
        pool.close().await;
        let counters = counters?;
        let (_, database) = self.stored_names(&row)?;
        let mut conn = self.inner.maintenance.connect().await?;
        let size = databases::size(&mut conn, &database).await;
        let closed = conn.close().await;
        let database_bytes = size?;
        closed?;
        Ok(Usage::new(database_bytes, counters))
    }

    /// The product's description of the tenant (`TenantHooks::describe`) on the tenant's pool for
    /// the operation; the states, pools and refusals of [`usage`](Self::usage).
    pub async fn describe(&self, tenant: TenantId) -> Result<Description, AppError> {
        let row = self.usage_row(tenant).await?;
        let pool = open(self, &LifecycleCapability::new(), &row, OpenMode::ReadOnly).await?;
        let description = self.inner.hooks.describe(&row.info, pool.pool()).await;
        pool.close().await;
        description
    }

    /// The row as committed now, when it is `active` or `suspended`.
    async fn usage_row(&self, tenant: TenantId) -> Result<TenantRow, AppError> {
        let row = self.tenant(tenant).await?;
        if readable(row.info.state) {
            Ok(row)
        } else {
            Err(tenant_state())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_active_and_suspended_tenants_are_read() {
        let states = [
            TenantState::Provisioning,
            TenantState::Active,
            TenantState::Suspended,
            TenantState::Purging,
            TenantState::Purged,
        ];
        let read: Vec<bool> = states.iter().map(|state| readable(*state)).collect();
        assert_eq!(read, [false, true, true, false, false]);
    }
}
