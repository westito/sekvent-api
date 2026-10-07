use std::fmt;
use std::time::SystemTime;

use futures::future::BoxFuture;
use sekvent_error::AppError;
use tokio_util::sync::CancellationToken;

/// Decides whether this process may run a job, outside the process (a
/// database lease). Without a guard a job runs on every instance.
///
/// Calls are awaited until the tick's misfire grace runs out (5 s for a
/// trigger) and a panic in one counts as an error with reason
/// `GUARD_PANICKED`.
pub trait JobGuard: Send + Sync + 'static {
    /// Take `job` for one run. `tick` is the scheduled time of a scheduled or
    /// catch-up run, `None` for a manual run. `Ok(None)`: another instance
    /// holds the job, or `tick` already ran.
    fn acquire<'a>(
        &'a self,
        job: &'a str,
        tick: Option<SystemTime>,
    ) -> BoxFuture<'a, Result<Option<JobPermit>, AppError>>;

    /// The scheduled time of the last tick any instance started.
    fn last_tick<'a>(&'a self, job: &'a str)
    -> BoxFuture<'a, Result<Option<SystemTime>, AppError>>;
}

/// Runs once after a run to give its right back.
pub(crate) type Release = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// The right to run once.
///
/// Dropping a permit without releasing it drops the release closure; the
/// guard's own cleanup (for example a lease's `Drop`) takes over.
pub struct JobPermit {
    fence: Option<u64>,
    lost: CancellationToken,
    release: Release,
}

impl JobPermit {
    /// `lost` fires when the right is withdrawn; `release` runs once after the run.
    pub fn new(
        fence: Option<u64>,
        lost: CancellationToken,
        release: impl FnOnce() -> BoxFuture<'static, ()> + Send + 'static,
    ) -> Self {
        Self {
            fence,
            lost,
            release: Box::new(release),
        }
    }

    /// The fencing token the guard issued with this permit, if any.
    pub fn fence(&self) -> Option<u64> {
        self.fence
    }

    pub(crate) fn into_parts(self) -> (CancellationToken, Release) {
        (self.lost, self.release)
    }
}

impl fmt::Debug for JobPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobPermit")
            .field("fence", &self.fence)
            .field("lost", &self.lost.is_cancelled())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn a_permit_hands_out_its_fence_lost_token_and_release_once() {
        let released = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&released);
        let lost = CancellationToken::new();
        let permit = JobPermit::new(Some(7), lost.clone(), move || {
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
            })
        });
        assert_eq!(permit.fence(), Some(7));

        let (seen, release) = permit.into_parts();
        assert!(!seen.is_cancelled());
        lost.cancel();
        assert!(seen.is_cancelled());

        release().await;
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn debug_shows_the_fence_and_whether_the_right_was_lost() {
        let lost = CancellationToken::new();
        let permit = JobPermit::new(None, lost.clone(), || Box::pin(async {}));
        assert_eq!(
            format!("{permit:?}"),
            "JobPermit { fence: None, lost: false, .. }"
        );
        lost.cancel();
        assert!(format!("{permit:?}").contains("lost: true"));
    }
}
