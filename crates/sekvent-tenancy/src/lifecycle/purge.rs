//! Purge: the tenant's database and role dropped, the row kept as a tombstone without hosts.
//! Nothing of the product runs and the tenant is never opened.

use sekvent_error::AppError;

use super::announce::announce;
use super::hooks::TenantHooks;
use super::provisioner::{Provisioner, commit, rollback};
use super::state::{abandon, finish, store_state};
use crate::cluster::{databases, roles};
use crate::control::rows;
use crate::error::tenant_state;
use crate::invalidation::ChangeKind;
use crate::{DatabaseName, TenantId, TenantRole, TenantRow, TenantState};

/// What the first lock of a purge does with the state it found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    /// `suspended`: mark `purging`, commit, announce, continue.
    Mark,
    /// `purging`: announce again and continue.
    Continue,
    /// `purged`: announce again and return the row.
    Done,
    /// Any other state: `TENANT_STATE`.
    Refused,
}

/// The first lock's decision for a row found in `state`.
fn start(state: TenantState) -> Start {
    match state {
        TenantState::Suspended => Start::Mark,
        TenantState::Purging => Start::Continue,
        TenantState::Purged => Start::Done,
        TenantState::Provisioning | TenantState::Active => Start::Refused,
    }
}

/// What the second lock of a purge does with the state it found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Destroy {
    /// `purging`: drop the database and the role, then commit `purged`.
    Drop,
    /// `purged` (another call finished first): announce again and return the row.
    Done,
    /// Any other state (changed between the two locks): `TENANT_STATE`.
    Refused,
}

/// The second lock's decision for a row found in `state`.
fn destroy(state: TenantState) -> Destroy {
    match state {
        TenantState::Purging => Destroy::Drop,
        TenantState::Purged => Destroy::Done,
        TenantState::Provisioning | TenantState::Active | TenantState::Suspended => {
            Destroy::Refused
        }
    }
}

impl<H: TenantHooks> Provisioner<H> {
    /// `suspended` or `purging` → `purging`, then `purged`.
    ///
    /// The lifecycle admission; the row locked; `suspended` is marked `purging`, committed and
    /// announced (`purging` found is announced again and continued, `purged` found is returned
    /// and announced again). Then the row locked again and held across the destructive phase:
    /// the stored names checked (`INTERNAL` for a role that is not the derived one, before any
    /// connection); on one maintenance connection `databases::drop(database, role)` then
    /// `roles::drop(role)`; the hosts cleared, the state `purged` (the database name is free
    /// again, the role name never), committed, announced. A failure after the first commit
    /// leaves `purging`, and a repeated call continues. `TENANT_NOT_FOUND` without a row;
    /// `TENANT_STATE` for `active` and `provisioning`.
    pub async fn purge(&self, tenant: TenantId) -> Result<TenantRow, AppError> {
        let _permit = self.inner.registry.admit().await?;

        let (mut tx, row) = self.begin_locked(tenant).await?;
        match start(row.info.state) {
            Start::Mark => {
                let marked = store_state(&mut tx, tenant, TenantState::Purging, self.now()).await;
                if let Err(error) = marked {
                    return Err(abandon(tx, error).await);
                }
                commit(tx).await?;
                announce(self, tenant, ChangeKind::STATE).await;
            }
            Start::Continue => {
                rollback(tx).await?;
                announce(self, tenant, ChangeKind::STATE).await;
            }
            Start::Done => {
                rollback(tx).await?;
                announce(self, tenant, ChangeKind::STATE).await;
                return Ok(row);
            }
            Start::Refused => return Err(abandon(tx, tenant_state()).await),
        }

        let (mut tx, row) = self.begin_locked(tenant).await?;
        match destroy(row.info.state) {
            Destroy::Drop => {}
            Destroy::Done => {
                rollback(tx).await?;
                announce(self, tenant, ChangeKind::STATE).await;
                return Ok(row);
            }
            Destroy::Refused => return Err(abandon(tx, tenant_state()).await),
        }
        let (role, database) = match self.stored_names(&row) {
            Ok(names) => names,
            Err(error) => return Err(abandon(tx, error).await),
        };
        if let Err(error) = self.purge_objects(&database, &role).await {
            return Err(abandon(tx, error).await);
        }
        let now = self.now();
        let cleared = match rows::clear_hosts(&mut tx, tenant, now).await {
            Ok(true) => store_state(&mut tx, tenant, TenantState::Purged, now).await,
            Ok(false) => Err(AppError::internal("the locked tenant row is gone")),
            Err(error) => Err(error),
        };
        if let Err(error) = cleared {
            return Err(abandon(tx, error).await);
        }
        let row = finish(tx, tenant).await?;
        announce(self, tenant, ChangeKind::STATE).await;
        Ok(row)
    }

    /// On one maintenance connection: the database dropped as its role (or left alone when it
    /// is another owner's and the role is gone), then the role. The connection is closed
    /// whatever happened; a drop's error wins over the close's.
    async fn purge_objects(
        &self,
        database: &DatabaseName,
        role: &TenantRole,
    ) -> Result<(), AppError> {
        let mut conn = self.inner.maintenance.connect().await?;
        let dropped = match databases::drop(&mut conn, database, role).await {
            Ok(()) => roles::drop(&mut conn, role).await,
            Err(error) => Err(error),
        };
        let closed = conn.close().await;
        dropped.and(closed)
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
    fn the_first_lock_marks_a_suspended_tenant_and_refuses_a_live_one() {
        let starts: Vec<Start> = STATES.iter().map(|state| start(*state)).collect();
        assert_eq!(
            starts,
            [
                Start::Refused,
                Start::Refused,
                Start::Mark,
                Start::Continue,
                Start::Done,
            ]
        );
    }

    #[test]
    fn the_second_lock_drops_only_a_purging_tenant() {
        let steps: Vec<Destroy> = STATES.iter().map(|state| destroy(*state)).collect();
        assert_eq!(
            steps,
            [
                Destroy::Refused,
                Destroy::Refused,
                Destroy::Refused,
                Destroy::Drop,
                Destroy::Done,
            ]
        );
    }
}
