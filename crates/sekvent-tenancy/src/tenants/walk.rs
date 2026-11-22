//! Walks over the active tenants, one after another.

use std::future::Future;

use sekvent_error::AppError;

use super::keyed::{Refused, Tenants};
use super::lease::TenantLease;
use crate::TenantId;

/// What a walk over the active tenants did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WalkReport {
    /// Tenants the walk tried, the busy and the failed ones included.
    pub visited: usize,
    /// Tenants skipped because the tenant cache or the lookup answered `TENANT_BUSY` on its own
    /// (the cache was full, or no lookup permit was free), or, in the sweep, whose row stayed
    /// locked. A refusal of the product's factory is never counted here, whatever its reason.
    pub busy: usize,
    /// Tenants whose open, visit or version write failed otherwise, in id order.
    pub failed: Vec<TenantId>,
}

impl<S: Send + Sync + 'static> Tenants<S> {
    /// Visits the active tenants in id order, one after another, with a lease each. Only a failed
    /// list read fails the walk.
    ///
    /// A tenant whose open the cache or the lookup answers `TENANT_BUSY` on its own (the cache
    /// is full, or no lookup permit is free) is counted in `busy` and skipped. Any other failed
    /// open, a refusal of the product's factory included whatever its reason, and an error
    /// `visit` returns, is recorded in `failed` and logged at `warn` with the tenant id, the code
    /// and the reason only. Each lease is dropped when `visit`'s future ends, so a walk over more
    /// tenants than the cache holds evicts as it goes.
    pub async fn for_each_active<F, Fut>(&self, mut visit: F) -> Result<WalkReport, AppError>
    where
        F: FnMut(TenantLease<S>) -> Fut,
        Fut: Future<Output = Result<(), AppError>>,
    {
        let mut report = WalkReport::default();
        for id in self.active_ids().await? {
            report.visited += 1;
            match self.open(id).await {
                Ok(lease) => {
                    if let Err(error) = visit(lease).await {
                        log_failure(id, &error);
                        report.failed.push(id);
                    }
                }
                Err(Refused::Busy(_)) => report.busy += 1,
                Err(Refused::Failed(error)) => {
                    log_failure(id, &error);
                    report.failed.push(id);
                }
            }
        }
        Ok(report)
    }
}

/// The one log line of a tenant a walk or the sweep could not handle: never the error's
/// message, metadata or source.
pub(super) fn log_failure(tenant: TenantId, error: &AppError) {
    let code = error.code().as_str();
    let reason = error.reason();
    tracing::warn!(
        tenant = %tenant,
        code,
        reason,
        "a tenant walk skipped a tenant that failed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scope type that derives nothing.
    struct Count(u32);
    #[tokio::test]
    async fn the_fixed_form_walks_its_one_scope() {
        let tenants = Tenants::fixed(Count(4));
        let mut seen = Vec::new();
        let report = tenants
            .for_each_active(|lease| {
                seen.push((lease.id(), lease.0));
                async { Ok(()) }
            })
            .await
            .expect("the walk ran");
        assert_eq!(
            report,
            WalkReport {
                visited: 1,
                busy: 0,
                failed: Vec::new(),
            }
        );
        assert_eq!(seen, [(TenantId::LOCAL, 4)]);
    }

    #[tokio::test]
    async fn a_visit_that_fails_is_recorded() {
        let tenants = Tenants::fixed(Count(4));
        let report = tenants
            .for_each_active(|_lease| async { Err(AppError::internal("the visit failed")) })
            .await
            .expect("the walk ran");
        assert_eq!(report.visited, 1);
        assert_eq!(report.busy, 0);
        assert_eq!(report.failed, [TenantId::LOCAL]);
    }

    #[tokio::test]
    async fn the_fixed_form_refuses_another_tenant_as_a_failure() {
        let tenants = Tenants::fixed(Count(4));
        let other = TenantId::from(uuid::Uuid::from_u128(7));
        match tenants.open(other).await {
            Err(Refused::Failed(error)) => {
                assert_eq!(error.reason(), Some(crate::reasons::TENANT_NOT_HERE));
            }
            Err(Refused::Busy(_)) => panic!("not the cache's busy answer"),
            Ok(_) => panic!("only the local tenant is served"),
        }
    }
}
