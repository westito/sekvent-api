use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sekvent_context::CallContext;
use sekvent_error::AppError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{PolicyError, cancelled_error, remaining};

/// Caps concurrent calls to a dependency so one slow dependency cannot tie
/// up every task.
///
/// Without a queue, a call beyond the cap is rejected immediately. With a
/// queue, up to `max_queue` callers wait (first come, first served) for at
/// most `max_wait` each, and never past their deadline. Rejection is
/// `RESOURCE_EXHAUSTED` with a `retry_after` hint.
#[derive(Debug)]
pub struct Bulkhead {
    semaphore: Arc<Semaphore>,
    max_concurrent: u32,
    max_queue: u32,
    max_wait: Duration,
    retry_after: Duration,
    queued: AtomicU32,
}

/// A slot in a [`Bulkhead`]; the slot frees when this is dropped.
#[derive(Debug)]
pub struct BulkheadPermit {
    _permit: OwnedSemaphorePermit,
}

impl Bulkhead {
    /// At most `max_concurrent` calls at once, no queue.
    pub fn new(max_concurrent: u32) -> Result<Self, PolicyError> {
        if max_concurrent == 0 {
            return Err(PolicyError::new(
                "bulkhead.max_concurrent",
                "must be at least 1",
            ));
        }
        let permits = usize::try_from(max_concurrent).unwrap_or(usize::MAX);
        Ok(Self {
            semaphore: Arc::new(Semaphore::new(permits.min(Semaphore::MAX_PERMITS))),
            max_concurrent,
            max_queue: 0,
            max_wait: Duration::ZERO,
            retry_after: Duration::from_secs(1),
            queued: AtomicU32::new(0),
        })
    }

    /// Let up to `max_queue` callers wait at most `max_wait` for a slot.
    #[must_use]
    pub fn with_queue(mut self, max_queue: u32, max_wait: Duration) -> Self {
        self.max_queue = max_queue;
        self.max_wait = max_wait;
        self
    }

    /// The `retry_after` hint attached to rejections (default one second).
    #[must_use]
    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = retry_after;
        self
    }

    /// Maximum concurrent calls.
    pub fn max_concurrent(&self) -> u32 {
        self.max_concurrent
    }
    /// Maximum queued callers.
    pub fn max_queue(&self) -> u32 {
        self.max_queue
    }
    /// Calls currently holding a slot.
    pub fn in_flight(&self) -> u32 {
        let free = u32::try_from(self.semaphore.available_permits()).unwrap_or(u32::MAX);
        self.max_concurrent.saturating_sub(free)
    }
    /// Callers currently waiting for a slot.
    pub fn queued(&self) -> u32 {
        self.queued.load(Ordering::Acquire)
    }

    /// Take a slot, queueing if configured.
    pub async fn acquire(&self, ctx: &CallContext) -> Result<BulkheadPermit, AppError> {
        if let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() {
            return Ok(BulkheadPermit { _permit: permit });
        }
        if self.max_queue == 0 || self.max_wait.is_zero() {
            return Err(self.rejection("the bulkhead is full"));
        }
        let joined = self
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < self.max_queue).then_some(queued + 1)
            });
        if joined.is_err() {
            return Err(self.rejection("the bulkhead queue is full"));
        }
        let _leave = LeaveQueue(&self.queued);

        let left = remaining(ctx);
        let bounded_by_deadline = left.is_some_and(|left| left < self.max_wait);
        let bound = left.map_or(self.max_wait, |left| left.min(self.max_wait));
        let waiting = tokio::time::timeout(bound, Arc::clone(&self.semaphore).acquire_owned());
        tokio::select! {
            biased;
            () = ctx.cancelled() => Err(cancelled_error()),
            outcome = waiting => match outcome {
                Ok(Ok(permit)) => Ok(BulkheadPermit { _permit: permit }),
                Ok(Err(_closed)) => Err(AppError::unavailable("the bulkhead is closed")),
                Err(_elapsed) if bounded_by_deadline => Err(AppError::deadline_exceeded(
                    "the call deadline passed while waiting for a bulkhead slot",
                )),
                Err(_elapsed) => Err(self.rejection("timed out waiting for a bulkhead slot")),
            },
        }
    }

    /// Run `fut` while holding a slot.
    pub async fn call<Fut, T>(&self, ctx: &CallContext, fut: Fut) -> Result<T, AppError>
    where
        Fut: Future<Output = Result<T, AppError>>,
    {
        let _permit = self.acquire(ctx).await?;
        fut.await
    }

    fn rejection(&self, message: &str) -> AppError {
        AppError::resource_exhausted(message)
            .with_reason("BULKHEAD_FULL")
            .with_retry_after(self.retry_after)
    }
}

struct LeaveQueue<'a>(&'a AtomicU32);

impl Drop for LeaveQueue<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::*;

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn rejects_zero_capacity() {
        assert_eq!(
            Bulkhead::new(0).unwrap_err().parameter(),
            "bulkhead.max_concurrent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rejects_immediately_without_a_queue() {
        let bulkhead = Bulkhead::new(2)
            .unwrap()
            .with_retry_after(Duration::from_millis(250));
        let ctx = CallContext::new();
        let first = bulkhead.acquire(&ctx).await.unwrap();
        let _second = bulkhead.acquire(&ctx).await.unwrap();
        assert_eq!(bulkhead.in_flight(), 2);
        let error = bulkhead.acquire(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(error.retry_after(), Some(Duration::from_millis(250)));
        assert_eq!(error.reason(), Some("BULKHEAD_FULL"));
        drop(first);
        assert_eq!(bulkhead.in_flight(), 1);
        bulkhead.acquire(&ctx).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn queues_then_rejects_when_the_queue_is_full() {
        let bulkhead = Arc::new(
            Bulkhead::new(1)
                .unwrap()
                .with_queue(1, Duration::from_secs(5)),
        );
        assert_eq!(bulkhead.max_concurrent(), 1);
        assert_eq!(bulkhead.max_queue(), 1);
        let ctx = CallContext::new();
        let held = bulkhead.acquire(&ctx).await.unwrap();

        let waiter = {
            let bulkhead = Arc::clone(&bulkhead);
            tokio::spawn(async move {
                let started = Instant::now();
                let _permit = bulkhead.acquire(&CallContext::new()).await.unwrap();
                started.elapsed()
            })
        };
        settle().await;
        assert_eq!(bulkhead.queued(), 1);

        let error = bulkhead.acquire(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(1)));

        tokio::time::advance(Duration::from_secs(2)).await;
        drop(held);
        assert_eq!(waiter.await.unwrap(), Duration::from_secs(2));
        assert_eq!(bulkhead.queued(), 0);
        assert_eq!(bulkhead.in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_wait_is_bounded() {
        let bulkhead = Bulkhead::new(1)
            .unwrap()
            .with_queue(4, Duration::from_secs(5));
        let ctx = CallContext::new();
        let _held = bulkhead.acquire(&ctx).await.unwrap();
        let started = Instant::now();
        let error = bulkhead.acquire(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        assert_eq!(bulkhead.queued(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_wait_never_passes_the_deadline() {
        let bulkhead = Bulkhead::new(1)
            .unwrap()
            .with_queue(4, Duration::from_secs(5));
        let _held = bulkhead.acquire(&CallContext::new()).await.unwrap();
        let ctx =
            CallContext::new().with_deadline(Instant::now().into_std() + Duration::from_secs(1));
        let started = Instant::now();
        let error = bulkhead.acquire(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(started.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn queued_wait_stops_on_cancellation() {
        let bulkhead = Bulkhead::new(1)
            .unwrap()
            .with_queue(4, Duration::from_secs(5));
        let _held = bulkhead.acquire(&CallContext::new()).await.unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let ctx = CallContext::new().with_cancel(token);
        let error = bulkhead.acquire(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
    }

    #[tokio::test(start_paused = true)]
    async fn call_holds_a_slot_for_the_duration() {
        let bulkhead = Bulkhead::new(1).unwrap();
        let ctx = CallContext::new();
        let value = bulkhead
            .call(&ctx, async {
                assert_eq!(bulkhead.in_flight(), 1);
                Ok::<_, AppError>(3)
            })
            .await
            .unwrap();
        assert_eq!(value, 3);
        assert_eq!(bulkhead.in_flight(), 0);
    }
}
