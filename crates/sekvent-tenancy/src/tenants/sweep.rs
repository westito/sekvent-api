//! The migration sweep over the active tenants.

use futures::StreamExt as _;
use futures::stream;
use sekvent_error::AppError;

use super::keyed::{Refused, Tenants};
use super::walk::{WalkReport, log_failure};
use crate::TenantId;
use crate::control::VersionStored;

/// How many tenants the sweep opens at once at most; never more than the cache's capacity.
const OPEN_AT_ONCE: usize = 2;

/// What the sweep did with one tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Swept {
    /// Opened, and the version stored, already stored, or nothing to store.
    Done,
    /// The cache or the lookup answered `TENANT_BUSY` on its own.
    Busy,
    /// Opened, but the row was locked by another transaction.
    Locked,
    /// The open or the write failed otherwise.
    Failed,
}

impl<S: Send + Sync + 'static> Tenants<S> {
    /// Opens every active tenant (which migrates it), two at a time (one at a time on a cache
    /// whose capacity is 1), drops the lease, then stores the migrator's latest up version
    /// without waiting on a row lock; a busy row is retried once after the others. Only a failed
    /// list read fails the sweep.
    ///
    /// A row still locked after the retry, and an open the cache or the lookup answers
    /// `TENANT_BUSY` on its own (the cache is full, or no lookup permit is free), count in
    /// `busy`; any other failed open, a refusal of the product's factory included whatever its
    /// reason, and a failed write are recorded in `failed` and logged like a walk's. A migrator
    /// without an up migration, and the fixed form, store nothing. An ordinary open never writes
    /// a version: only the sweep does.
    pub async fn migration_sweep(&self) -> Result<WalkReport, AppError> {
        let ids = self.active_ids().await?;
        let latest = self.latest_version();
        let mut report = WalkReport {
            visited: ids.len(),
            ..WalkReport::default()
        };
        let first: Vec<(TenantId, Swept)> = stream::iter(ids)
            .map(|id| async move { (id, self.open_and_store(id, latest).await) })
            .buffer_unordered(at_once(self.stats().capacity))
            .collect()
            .await;
        let mut locked = Vec::new();
        for (id, swept) in first {
            if tally(&mut report, id, swept) {
                locked.push(id);
            }
        }
        locked.sort_unstable();
        for id in locked {
            if tally(&mut report, id, self.store(id, latest).await) {
                report.busy += 1;
            }
        }
        report.failed.sort_unstable();
        Ok(report)
    }

    /// Opens the tenant, drops the lease at once, then stores the version.
    async fn open_and_store(&self, id: TenantId, latest: Option<i64>) -> Swept {
        match self.open(id).await {
            Ok(lease) => drop(lease),
            Err(Refused::Busy(_)) => return Swept::Busy,
            Err(Refused::Failed(error)) => {
                log_failure(id, &error);
                return Swept::Failed;
            }
        }
        self.store(id, latest).await
    }

    /// Stores `latest` as the tenant's schema version without waiting on its row lock.
    async fn store(&self, id: TenantId, latest: Option<i64>) -> Swept {
        let (Some(version), Some(registry)) = (latest, self.registry()) else {
            return Swept::Done;
        };
        match registry.store_schema_version(id, version).await {
            Ok(VersionStored::Stored | VersionStored::Unchanged) => Swept::Done,
            Ok(VersionStored::Busy) => Swept::Locked,
            Err(error) => {
                log_failure(id, &error);
                Swept::Failed
            }
        }
    }
}

/// How many opens the sweep runs at once on a cache of `capacity`: two, never more than the
/// capacity, and at least one.
fn at_once(capacity: usize) -> usize {
    capacity.clamp(1, OPEN_AT_ONCE)
}

/// Counts one outcome in `report`; whether the row was locked, which the caller decides on.
fn tally(report: &mut WalkReport, id: TenantId, swept: Swept) -> bool {
    match swept {
        Swept::Done => {}
        Swept::Busy => report.busy += 1,
        Swept::Locked => return true,
        Swept::Failed => report.failed.push(id),
    }
    false
}

#[cfg(test)]
mod tests {
    use crate::{TenantId, Tenants};

    use super::{Swept, WalkReport, at_once, tally};

    #[test]
    fn the_sweep_never_opens_more_at_once_than_the_cache_holds() {
        assert_eq!(at_once(1), 1);
        assert_eq!(at_once(2), 2);
        assert_eq!(at_once(64), 2);
        assert_eq!(at_once(0), 1);
    }

    #[test]
    fn an_outcome_counts_once_and_a_locked_row_is_left_to_the_caller() {
        let (first, second) = (TenantId::LOCAL, TenantId::from(uuid::Uuid::from_u128(7)));
        let mut report = WalkReport::default();
        assert!(!tally(&mut report, first, Swept::Done));
        assert!(!tally(&mut report, first, Swept::Busy));
        assert!(!tally(&mut report, second, Swept::Failed));
        assert!(tally(&mut report, first, Swept::Locked));
        assert_eq!(
            report,
            WalkReport {
                visited: 0,
                busy: 1,
                failed: vec![second],
            }
        );
    }
    #[tokio::test]
    async fn the_fixed_form_is_opened_and_nothing_is_written() {
        let tenants = Tenants::fixed(9_u32);
        let report = tenants.migration_sweep().await.expect("the sweep ran");
        assert_eq!(
            report,
            WalkReport {
                visited: 1,
                busy: 0,
                failed: Vec::new(),
            }
        );
        assert_eq!(tenants.latest_version(), None);
        assert!(tenants.peek(TenantId::LOCAL).is_some());
    }
}
