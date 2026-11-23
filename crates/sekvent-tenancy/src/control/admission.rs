//! The two admissions of the control pool: the lookup admission, which a request's cache miss
//! passes without waiting, and the lifecycle admission, which a lifecycle call waits for.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use sekvent_error::AppError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::messages::LIFECYCLE_BUSY;
/// How long a lifecycle call waits for a permit by default.
const LIFECYCLE_WAIT: Duration = Duration::from_secs(10);

/// `n` as a permit count.
fn permits(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// The lookup permits of a control pool of `control_max` connections: `max(1, control_max / 2)`.
pub(super) fn lookup_permits(control_max: u32) -> usize {
    permits((control_max / 2).max(1))
}

/// The lifecycle permits of a control pool of `control_max` connections:
/// `max(1, (control_max - 1) / 2)`.
pub(super) fn lifecycle_permits(control_max: u32) -> usize {
    permits((control_max.saturating_sub(1) / 2).max(1))
}

/// A lookup read's hold on the control pool; dropping it gives the permit back.
pub(super) struct LookupPermit {
    /// The permit, given back on drop.
    _permit: OwnedSemaphorePermit,
}

/// The lookup admission: a fixed number of reads at once, taken without waiting. Clones share the
/// one semaphore.
#[derive(Clone)]
pub(super) struct LookupAdmission {
    /// The permits, shared by every clone.
    permits: Arc<Semaphore>,
}

impl LookupAdmission {
    /// [`lookup_permits`] of `control_max` permits.
    pub(super) fn new(control_max: u32) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(lookup_permits(control_max))),
        }
    }

    /// A permit when one is free; `None` at once otherwise.
    pub(super) fn try_admit(&self) -> Option<LookupPermit> {
        Arc::clone(&self.permits)
            .try_acquire_owned()
            .ok()
            .map(|permit| LookupPermit { _permit: permit })
    }

    /// The permits free now.
    #[cfg(test)]
    pub(super) fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

/// A lifecycle call's hold on the control pool; dropping it gives the permit back.
pub struct LifecyclePermit {
    /// The permit, given back on drop.
    _permit: OwnedSemaphorePermit,
}

// `LifecyclePermit(..)`
impl fmt::Debug for LifecyclePermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LifecyclePermit(..)")
    }
}

/// The lifecycle admission: `max(1, (control_max - 1) / 2)` permits. Clones share the one
/// semaphore; each value keeps its own wait.
#[derive(Clone)]
pub(super) struct LifecycleAdmission {
    /// The permits, shared by every clone.
    permits: Arc<Semaphore>,
    /// How long `admit` waits for a permit.
    wait: Duration,
}

impl LifecycleAdmission {
    /// `control_max` is the control pool's `max_connections`; the wait is 10 s.
    pub(super) fn new(control_max: u32) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(lifecycle_permits(control_max))),
            wait: LIFECYCLE_WAIT,
        }
    }

    /// The same permits, waited for up to `wait`.
    pub(super) fn with_wait(self, wait: Duration) -> Self {
        Self { wait, ..self }
    }

    /// Waits up to the wait on tokio's clock (`Duration::ZERO` takes a free permit and does not
    /// wait); then `UNAVAILABLE` ("the server is busy, try again shortly") without a reason. A
    /// cancelled wait takes no permit.
    pub(super) async fn admit(&self) -> Result<LifecyclePermit, AppError> {
        let wait = Arc::clone(&self.permits).acquire_owned();
        match tokio::time::timeout(self.wait, wait).await {
            Ok(Ok(permit)) => Ok(LifecyclePermit { _permit: permit }),
            // The semaphore is never closed: only the deadline ends a wait without a permit.
            Ok(Err(_)) | Err(_) => Err(AppError::unavailable(LIFECYCLE_BUSY)),
        }
    }

    /// The permits free now.
    pub(super) fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use sekvent_error::ErrorCode;

    use super::*;

    #[test]
    fn the_permit_counts_follow_the_pool_size() {
        let expected: [(u32, usize, usize); 9] = [
            (0, 1, 1),
            (1, 1, 1),
            (2, 1, 1),
            (3, 1, 1),
            (4, 2, 1),
            (5, 2, 2),
            (6, 3, 2),
            (7, 3, 3),
            (8, 4, 3),
        ];
        for (size, lookup, lifecycle) in expected {
            assert_eq!(lookup_permits(size), lookup, "lookup permits of {size}");
            assert_eq!(
                lifecycle_permits(size),
                lifecycle,
                "lifecycle permits of {size}"
            );
            assert_eq!(LookupAdmission::new(size).available(), lookup, "{size}");
            assert_eq!(
                LifecycleAdmission::new(size).available(),
                lifecycle,
                "{size}"
            );
        }
    }

    #[test]
    fn a_lookup_permit_is_taken_without_waiting_and_comes_back_on_drop() {
        let admission = LookupAdmission::new(4);
        let first = admission.try_admit().expect("a first permit");
        let second = admission.clone().try_admit().expect("a second permit");
        assert!(
            admission.try_admit().is_none(),
            "two permits of a pool of 4"
        );
        drop(first);
        assert_eq!(admission.available(), 1);
        let third = admission.try_admit().expect("the permit came back");
        drop((second, third));
        assert_eq!(admission.available(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn admit_returns_a_permit_that_comes_back_on_drop() {
        let admission = LifecycleAdmission::new(5);
        let first = admission.admit().await.expect("a first permit");
        let second = admission.admit().await.expect("a second permit");
        assert_eq!(admission.available(), 0);
        drop(first);
        assert_eq!(admission.available(), 1);
        drop(second);
        assert_eq!(admission.available(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_admit_gets_the_permit_another_call_drops() {
        let admission = LifecycleAdmission::new(3);
        let held = admission.admit().await.expect("the one permit");
        let mut waiting = pin!(admission.admit());
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        drop(held);
        let permit = waiting.await.expect("the dropped permit was handed on");
        assert_eq!(admission.available(), 0);
        drop(permit);
        assert_eq!(admission.available(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn admit_gives_up_after_ten_seconds_without_a_reason() {
        let admission = LifecycleAdmission::new(3);
        let _held = admission.admit().await.expect("the one permit");
        let started = tokio::time::Instant::now();
        let mut waiting = pin!(admission.admit());
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(9_999)).await;
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(1)).await;
        let error = waiting.await.expect_err("no permit came free");
        assert_eq!(started.elapsed(), LIFECYCLE_WAIT);
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "the server is busy, try again shortly");
        assert_eq!(error.message(), crate::messages::LIFECYCLE_BUSY);
        assert_eq!(error.reason(), None);
        assert_eq!(error.domain(), None);
        assert_eq!(error.retry_after(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_value_with_its_own_wait_shares_the_permits() {
        let admission = LifecycleAdmission::new(3);
        let brief = admission.clone().with_wait(Duration::from_secs(2));
        let held = brief.admit().await.expect("the one permit");
        assert_eq!(admission.available(), 0);

        let started = tokio::time::Instant::now();
        let error = brief.admit().await.expect_err("no permit came free");
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), None);

        let started = tokio::time::Instant::now();
        let error = admission
            .admit()
            .await
            .expect_err("the original waits its own 10 s");
        assert_eq!(started.elapsed(), LIFECYCLE_WAIT);
        assert_eq!(error.code(), ErrorCode::Unavailable);

        let at_once = admission.clone().with_wait(Duration::ZERO);
        let started = tokio::time::Instant::now();
        let error = at_once.admit().await.expect_err("no wait at all");
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(error.code(), ErrorCode::Unavailable);
        drop(held);
        let permit = at_once
            .admit()
            .await
            .expect("a free permit is taken at once");
        assert_eq!(brief.available(), 0);
        drop(permit);
        assert_eq!(brief.available(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_wait_takes_no_permit() {
        let admission = LifecycleAdmission::new(3);
        let held = admission.admit().await.expect("the one permit");
        let mut waiting = Box::pin(admission.admit());
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        drop(waiting);
        drop(held);
        assert_eq!(admission.available(), 1);
        let again = admission.admit().await.expect("the permit is free");
        assert_eq!(format!("{again:?}"), "LifecyclePermit(..)");
    }

    #[tokio::test(start_paused = true)]
    async fn clones_share_the_lifecycle_permits() {
        let admission = LifecycleAdmission::new(5);
        let clone = admission.clone();
        let first = admission.admit().await.expect("a first permit");
        let second = admission.admit().await.expect("a second permit");
        assert_eq!(clone.available(), 0);

        let error = clone
            .admit()
            .await
            .expect_err("the clone shares the held permits");
        assert_eq!(error.code(), ErrorCode::Unavailable);

        let mut on_clone = pin!(clone.admit());
        assert!(futures::poll!(on_clone.as_mut()).is_pending());
        drop(first);
        let from_clone = on_clone
            .await
            .expect("a permit dropped on the original admits the clone");

        let mut on_original = pin!(admission.admit());
        assert!(futures::poll!(on_original.as_mut()).is_pending());
        drop(from_clone);
        let from_original = on_original
            .await
            .expect("a permit dropped on the clone admits the original");
        drop((second, from_original));
        assert_eq!(admission.available(), 2);
    }
}
