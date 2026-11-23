//! Suspend, resume and set hosts: each admitted first, then decided from the locked row.

use std::time::SystemTime;

use sekvent_error::AppError;

use super::announce::announce;
use super::hooks::TenantHooks;
use super::open::{LifecycleCapability, OpenMode, open};
use super::provisioner::{ControlTx, HostsChange, Provisioner, commit, rollback, validate_hosts};
use crate::control::rows;
use crate::error::tenant_state;
use crate::invalidation::ChangeKind;
use crate::{TenantId, TenantRow, TenantState};

/// What a state change does with the state it found under the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The source state: run the hook, then change the state.
    Change,
    /// The target state already: nothing changes, the state is announced again.
    Done,
}

/// The step of a change from `source` to `target` for a row found in `found`; `None` when the
/// call is refused (`TENANT_STATE`).
fn transition(source: TenantState, target: TenantState, found: TenantState) -> Option<Step> {
    if found == source {
        Some(Step::Change)
    } else if found == target {
        Some(Step::Done)
    } else {
        None
    }
}

/// Whether a tenant in `state` may have its hosts replaced.
fn hosts_changeable(state: TenantState) -> bool {
    matches!(state, TenantState::Active | TenantState::Suspended)
}

/// The product's hook that runs before a state change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hook {
    BeforeSuspend,
    BeforeResume,
}

/// Rolls `tx` back and returns `error`: the error that made the call give up wins over a failed
/// rollback (the server rolls a lost session's transaction back by itself).
pub(super) async fn abandon(tx: ControlTx, error: AppError) -> AppError {
    let _ = rollback(tx).await;
    error
}

/// Sets the state of the row `tx` holds locked; `INTERNAL` when the row is gone.
pub(super) async fn store_state(
    tx: &mut ControlTx,
    tenant: TenantId,
    state: TenantState,
    now: SystemTime,
) -> Result<(), AppError> {
    if rows::set_state(tx, tenant, state, now).await? {
        Ok(())
    } else {
        Err(AppError::internal("the locked tenant row is gone"))
    }
}

/// Reads the row back inside `tx` and commits; `INTERNAL` when the row is gone. On an error the
/// transaction is rolled back.
pub(super) async fn finish(mut tx: ControlTx, tenant: TenantId) -> Result<TenantRow, AppError> {
    let row = match rows::get(&mut tx, tenant).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return Err(abandon(tx, AppError::internal("the locked tenant row is gone")).await);
        }
        Err(error) => return Err(abandon(tx, error).await),
    };
    commit(tx).await?;
    Ok(row)
}

impl<H: TenantHooks> Provisioner<H> {
    /// `active` → `suspended`. `TenantHooks::before_suspend` runs first on the tenant opened for
    /// the operation; strict: a tenant whose database cannot be opened, or whose hook fails,
    /// stays `active`. A suspended tenant is returned unchanged and announced again.
    /// `TENANT_NOT_FOUND` without a row; `TENANT_STATE` for `provisioning`, `purging`, `purged`.
    pub async fn suspend(&self, tenant: TenantId) -> Result<TenantRow, AppError> {
        self.state_change(
            tenant,
            TenantState::Active,
            TenantState::Suspended,
            Hook::BeforeSuspend,
        )
        .await
    }

    /// `suspended` → `active`, after `TenantHooks::before_resume`, with the strictness of
    /// [`suspend`](Self::suspend). An active tenant is returned unchanged and announced again.
    /// `TENANT_NOT_FOUND` without a row; `TENANT_STATE` for `provisioning`, `purging`, `purged`.
    pub async fn resume(&self, tenant: TenantId) -> Result<TenantRow, AppError> {
        self.state_change(
            tenant,
            TenantState::Suspended,
            TenantState::Active,
            Hook::BeforeResume,
        )
        .await
    }

    /// Replaces the tenant's label and its whole host set while it is `active` or `suspended`;
    /// the state is unchanged. Validated after the lock (`INVALID_ARGUMENT` with field
    /// violations); a host another tenant holds is `TENANT_STATE`. Announced with
    /// `ChangeKind::HOSTS`. `TENANT_NOT_FOUND` without a row; `TENANT_STATE` for `provisioning`,
    /// `purging`, `purged`.
    pub async fn set_hosts(
        &self,
        tenant: TenantId,
        change: HostsChange,
    ) -> Result<TenantRow, AppError> {
        let _permit = self.inner.registry.admit().await?;
        let (mut tx, row) = self.begin_locked(tenant).await?;
        if !hosts_changeable(row.info.state) {
            return Err(abandon(tx, tenant_state()).await);
        }
        let valid = match validate_hosts(&self.inner.labels, &change) {
            Ok(valid) => valid,
            Err(error) => return Err(abandon(tx, error).await),
        };
        let stored = rows::set_hosts(
            &mut tx,
            tenant,
            &valid.subdomain,
            &valid.hosts,
            &valid.primary_host,
            self.now(),
        )
        .await;
        if let Err(error) = stored {
            return Err(abandon(tx, error).await);
        }
        let row = finish(tx, tenant).await?;
        announce(self, tenant, ChangeKind::HOSTS).await;
        Ok(row)
    }

    /// `source` → `target` after `hook`, decided from the locked row.
    async fn state_change(
        &self,
        tenant: TenantId,
        source: TenantState,
        target: TenantState,
        hook: Hook,
    ) -> Result<TenantRow, AppError> {
        let _permit = self.inner.registry.admit().await?;
        let (mut tx, row) = self.begin_locked(tenant).await?;
        match transition(source, target, row.info.state) {
            None => Err(abandon(tx, tenant_state()).await),
            Some(Step::Done) => {
                rollback(tx).await?;
                announce(self, tenant, ChangeKind::STATE).await;
                Ok(row)
            }
            Some(Step::Change) => {
                if let Err(error) = self.state_hook(&row, hook).await {
                    return Err(abandon(tx, error).await);
                }
                if let Err(error) = store_state(&mut tx, tenant, target, self.now()).await {
                    return Err(abandon(tx, error).await);
                }
                let row = finish(tx, tenant).await?;
                announce(self, tenant, ChangeKind::STATE).await;
                Ok(row)
            }
        }
    }

    /// Opens the tenant read-write for the operation, runs `hook` on it and closes it whatever
    /// the hook answered. A failure to open is the open's error; the hook's error is unchanged.
    async fn state_hook(&self, row: &TenantRow, hook: Hook) -> Result<(), AppError> {
        let pool = open(self, &LifecycleCapability::new(), row, OpenMode::ReadWrite).await?;
        let hooks = &self.inner.hooks;
        let outcome = match hook {
            Hook::BeforeSuspend => hooks.before_suspend(&row.info, pool.pool()).await,
            Hook::BeforeResume => hooks.before_resume(&row.info, pool.pool()).await,
        };
        pool.close().await;
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [TenantState; 5] = [
        TenantState::Provisioning,
        TenantState::Active,
        TenantState::Suspended,
        TenantState::Purging,
        TenantState::Purged,
    ];

    #[test]
    fn suspend_changes_an_active_tenant_and_repeats_on_a_suspended_one() {
        let steps: Vec<Option<Step>> = STATES
            .iter()
            .map(|found| transition(TenantState::Active, TenantState::Suspended, *found))
            .collect();
        assert_eq!(
            steps,
            [None, Some(Step::Change), Some(Step::Done), None, None]
        );
    }

    #[test]
    fn resume_changes_a_suspended_tenant_and_repeats_on_an_active_one() {
        let steps: Vec<Option<Step>> = STATES
            .iter()
            .map(|found| transition(TenantState::Suspended, TenantState::Active, *found))
            .collect();
        assert_eq!(
            steps,
            [None, Some(Step::Done), Some(Step::Change), None, None]
        );
    }

    #[test]
    fn hosts_change_only_for_active_and_suspended_tenants() {
        let allowed: Vec<bool> = STATES
            .iter()
            .map(|state| hosts_changeable(*state))
            .collect();
        assert_eq!(allowed, [false, true, true, false, false]);
    }
}
