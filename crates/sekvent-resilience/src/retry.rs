use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rand::SeedableRng;
use rand::rngs::SmallRng;
use sekvent_context::CallContext;
use sekvent_error::AppError;

use crate::{Backoff, RetryBudget, cancelled_error, deadline_error, remaining};

/// Bounded, budgeted retries of transient failures.
///
/// A failed attempt is retried only when all of these hold:
/// - the operation was declared idempotent by the caller;
/// - the error is transient ([`AppError::is_transient`]);
/// - fewer than `max_attempts` attempts were made;
/// - the delay (the larger of the backoff and the error's `retry_after`)
///   ends before the call deadline — the policy never sleeps past
///   [`CallContext::deadline`], it returns the last error instead;
/// - the shared [`RetryBudget`], if any, has a token.
///
/// Cancellation of the context stops both a running attempt and a pending
/// sleep with `CANCELLED`. Cloning shares the budget and the jitter source.
#[derive(Clone)]
pub struct RetryPolicy {
    max_attempts: u32,
    backoff: Backoff,
    budget: Option<Arc<RetryBudget>>,
    rng: Arc<Mutex<SmallRng>>,
}

impl fmt::Debug for RetryPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetryPolicy")
            .field("max_attempts", &self.max_attempts)
            .field("backoff", &self.backoff)
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

impl Default for RetryPolicy {
    /// Three attempts, [`Backoff::default`], [`RetryBudget::default`].
    fn default() -> Self {
        Self::new(3, Backoff::default()).with_budget(Arc::new(RetryBudget::default()))
    }
}

impl RetryPolicy {
    /// Up to `max_attempts` attempts in total (a value of 0 is treated as 1)
    /// with `backoff` between them, without a budget.
    pub fn new(max_attempts: u32, backoff: Backoff) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            backoff,
            budget: None,
            rng: Arc::new(Mutex::new(SmallRng::from_rng(&mut rand::rng()))),
        }
    }

    /// A single attempt: never retries.
    pub fn none() -> Self {
        Self::new(1, Backoff::default())
    }

    /// Draw retries from `budget` (share one budget across every call to
    /// the same dependency).
    #[must_use]
    pub fn with_budget(mut self, budget: Arc<RetryBudget>) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Retry without a budget.
    #[must_use]
    pub fn without_budget(mut self) -> Self {
        self.budget = None;
        self
    }

    /// Seed the jitter source, for reproducible delays.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.rng = Arc::new(Mutex::new(SmallRng::seed_from_u64(seed)));
        self
    }

    /// Maximum attempts, the first one included.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }
    /// The backoff between attempts.
    pub fn backoff(&self) -> &Backoff {
        &self.backoff
    }
    /// The shared budget, if any.
    pub fn budget(&self) -> Option<&Arc<RetryBudget>> {
        self.budget.as_ref()
    }

    /// Run `op`, retrying as described on [`RetryPolicy`].
    ///
    /// `idempotent` is the caller's promise that repeating the operation is
    /// safe; without it nothing is retried. An already expired deadline
    /// fails with `DEADLINE_EXCEEDED` before the first attempt.
    pub async fn retry<F, Fut, T>(
        &self,
        ctx: &CallContext,
        idempotent: bool,
        mut op: F,
    ) -> Result<T, AppError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, AppError>>,
    {
        if ctx.cancel_token().is_cancelled() {
            return Err(cancelled_error());
        }
        if remaining(ctx) == Some(Duration::ZERO) {
            return Err(deadline_error());
        }
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            let outcome = tokio::select! {
                biased;
                () = ctx.cancelled() => return Err(cancelled_error()),
                outcome = op() => outcome,
            };
            let error = match outcome {
                Ok(value) => {
                    if let Some(budget) = &self.budget {
                        budget.deposit();
                    }
                    return Ok(value);
                }
                Err(error) => error,
            };
            let Some(delay) = self.next_delay(ctx, idempotent, attempt, &error) else {
                return Err(error);
            };
            tracing::debug!(
                attempt,
                delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                code = %error.code(),
                "retrying after a transient failure"
            );
            tokio::select! {
                biased;
                () = ctx.cancelled() => return Err(cancelled_error()),
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// The delay before the next attempt, or `None` to give up.
    fn next_delay(
        &self,
        ctx: &CallContext,
        idempotent: bool,
        attempt: u32,
        error: &AppError,
    ) -> Option<Duration> {
        if !idempotent || !error.is_transient() || attempt >= self.max_attempts {
            return None;
        }
        let mut delay = {
            let mut rng = self.rng.lock().unwrap_or_else(PoisonError::into_inner);
            self.backoff.delay(attempt - 1, &mut *rng)
        };
        if let Some(hint) = error.retry_after() {
            delay = delay.max(hint);
        }
        if remaining(ctx).is_some_and(|left| delay >= left) {
            tracing::debug!(
                attempt,
                "not retrying: the next attempt would start past the deadline"
            );
            return None;
        }
        if let Some(budget) = &self.budget
            && !budget.try_withdraw()
        {
            tracing::debug!(attempt, "not retrying: the retry budget is exhausted");
            return None;
        }
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use sekvent_error::ErrorCode;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn constant(ms: u64) -> Backoff {
        Backoff::constant(Duration::from_millis(ms))
    }

    fn ctx_with_deadline(after: Duration) -> CallContext {
        CallContext::new().with_deadline(Instant::now().into_std() + after)
    }

    #[tokio::test(start_paused = true)]
    async fn retries_transient_failures_until_success() {
        let policy = RetryPolicy::new(3, constant(100));
        let calls = Cell::new(0_u32);
        let started = Instant::now();
        let result = policy
            .retry(&CallContext::new(), true, || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move {
                    if n < 3 {
                        Err(AppError::unavailable("down"))
                    } else {
                        Ok(n)
                    }
                }
            })
            .await;
        assert_eq!(result.unwrap(), 3);
        assert_eq!(started.elapsed(), Duration::from_millis(200));
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_attempts() {
        let policy = RetryPolicy::new(3, constant(10));
        let calls = Cell::new(0_u32);
        let error = policy
            .retry(&CallContext::new(), true, || {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(AppError::unavailable("down")) }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn never_retries_non_idempotent_operations() {
        let policy = RetryPolicy::new(5, constant(10));
        let calls = Cell::new(0_u32);
        let error = policy
            .retry(&CallContext::new(), false, || {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(AppError::unavailable("down")) }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn never_retries_non_transient_codes() {
        let policy = RetryPolicy::new(5, constant(10));
        let calls = Cell::new(0_u32);
        let error = policy
            .retry(&CallContext::new(), true, || {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(AppError::invalid_argument("bad")) }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stops_when_the_budget_is_exhausted_and_resumes_after_refill() {
        let budget = Arc::new(RetryBudget::new(1.0, 0).unwrap());
        let policy = RetryPolicy::new(5, constant(10)).with_budget(Arc::clone(&budget));
        let calls = Cell::new(0_u32);
        let failing = || {
            calls.set(calls.get() + 1);
            async { Err::<(), _>(AppError::unavailable("down")) }
        };
        policy
            .retry(&CallContext::new(), true, failing)
            .await
            .unwrap_err();
        assert_eq!(calls.get(), 1, "an empty budget allows no retry");

        policy
            .retry(&CallContext::new(), true, || async { Ok(()) })
            .await
            .unwrap();
        policy
            .retry(&CallContext::new(), true, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(budget.available(), 2);

        calls.set(0);
        policy
            .retry(&CallContext::new(), true, || {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(AppError::unavailable("down")) }
            })
            .await
            .unwrap_err();
        assert_eq!(calls.get(), 3, "two banked tokens pay for two retries");
        assert_eq!(budget.available(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn honours_retry_after() {
        let policy = RetryPolicy::new(2, constant(10));
        let calls = Cell::new(0_u32);
        let started = Instant::now();
        policy
            .retry(&CallContext::new(), true, || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move {
                    if n == 1 {
                        Err(AppError::resource_exhausted("slow down")
                            .with_retry_after(Duration::from_secs(5)))
                    } else {
                        Ok(())
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_beyond_the_deadline_returns_the_error_without_sleeping() {
        let policy = RetryPolicy::new(5, constant(10));
        let ctx = ctx_with_deadline(Duration::from_secs(2));
        let started = Instant::now();
        let calls = Cell::new(0_u32);
        let error = policy
            .retry(&ctx, true, || {
                calls.set(calls.get() + 1);
                async {
                    Err::<(), _>(
                        AppError::unavailable("later").with_retry_after(Duration::from_secs(10)),
                    )
                }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(calls.get(), 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_is_capped_by_the_deadline() {
        let policy = RetryPolicy::new(10, constant(1000));
        let ctx = ctx_with_deadline(Duration::from_millis(2500));
        let started = Instant::now();
        let calls = Cell::new(0_u32);
        policy
            .retry(&ctx, true, || {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(AppError::unavailable("down")) }
            })
            .await
            .unwrap_err();
        assert_eq!(calls.get(), 3, "attempts at 0 s, 1 s and 2 s");
        assert_eq!(started.elapsed(), Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_deadline_fails_before_the_first_attempt() {
        let ctx = ctx_with_deadline(Duration::from_secs(1));
        tokio::time::advance(Duration::from_secs(2)).await;
        let calls = Cell::new(0_u32);
        let error = RetryPolicy::default()
            .retry(&ctx, true, || {
                calls.set(calls.get() + 1);
                async { Ok(()) }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(calls.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_attempts_and_sleeps() {
        let token = CancellationToken::new();
        let ctx = CallContext::new().with_cancel(token.clone());
        token.cancel();
        let error = RetryPolicy::default()
            .retry(&ctx, true, || async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);

        let token = CancellationToken::new();
        let ctx = CallContext::new().with_cancel(token.clone());
        let error = RetryPolicy::new(3, constant(60_000))
            .retry(&ctx, true, || {
                token.cancel();
                async { Err::<(), _>(AppError::unavailable("down")) }
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ErrorCode::Cancelled,
            "cancelled while sleeping"
        );

        let token = CancellationToken::new();
        let ctx = CallContext::new().with_cancel(token.clone());
        let error = RetryPolicy::none()
            .retry(&ctx, true, || {
                token.cancel();
                futures::future::pending::<Result<(), AppError>>()
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ErrorCode::Cancelled,
            "cancelled while running"
        );
    }

    #[test]
    fn accessors_and_defaults() {
        let policy = RetryPolicy::default().with_seed(1);
        assert_eq!(policy.max_attempts(), 3);
        assert_eq!(policy.backoff(), &Backoff::default());
        assert!(policy.budget().is_some());
        assert!(policy.clone().without_budget().budget().is_none());
        assert_eq!(RetryPolicy::new(0, Backoff::default()).max_attempts(), 1);
        assert_eq!(RetryPolicy::none().max_attempts(), 1);
        assert!(format!("{policy:?}").starts_with("RetryPolicy"));
    }
}
