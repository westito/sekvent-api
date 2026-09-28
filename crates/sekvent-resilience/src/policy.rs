use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sekvent_context::CallContext;
use sekvent_error::{AppError, ErrorCode};

use crate::{Bulkhead, CircuitBreaker, RateGate, RetryPolicy, Timeout, ensure_live, remaining};

/// A composed per-call resilience policy.
///
/// [`Policy::call`] applies the parts in this fixed order, outermost first:
///
/// 1. **rate gate** — one permit per logical call, waited for within the deadline;
/// 2. **bulkhead** — one slot held for the whole logical call, retries included;
/// 3. **circuit breaker** — fails fast while open and records the final
///    outcome of the logical call once;
/// 4. **retry** around **timeout** — each attempt gets its own timeout,
///    capped by the call deadline;
/// 5. the operation itself.
///
/// Every part is optional except the timeout, which without a configured
/// limit still enforces the context deadline. Cloning is cheap and shares
/// all state (budget, gate, slots, breaker).
///
/// A call whose context is already cancelled or past its deadline fails
/// before any part runs, so it spends no rate permit, slot or breaker
/// permit. The breaker only learns about outcomes the dependency caused:
/// when the operation never ran, or the call ended because the caller
/// cancelled or its own deadline ran out, the breaker permit is released
/// unrecorded. A per-attempt timeout firing while the caller still had
/// time left does count as a failure — the dependency was too slow.
#[derive(Debug, Clone)]
pub struct Policy {
    name: String,
    timeout: Timeout,
    retry: RetryPolicy,
    rate_gate: Option<Arc<RateGate>>,
    bulkhead: Option<Arc<Bulkhead>>,
    breaker: Option<Arc<CircuitBreaker>>,
}

impl Default for Policy {
    /// Named `default`: deadline-only timeout, no retries, nothing else.
    fn default() -> Self {
        Self::new("default")
    }
}

impl Policy {
    /// A policy named `name` that only enforces the context deadline.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            timeout: Timeout::deadline_only(),
            retry: RetryPolicy::none(),
            rate_gate: None,
            bulkhead: None,
            breaker: None,
        }
    }

    /// Set the per-attempt timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Timeout) -> Self {
        self.timeout = timeout;
        self
    }
    /// Set the retry policy.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
    /// Gate calls through a (possibly shared) rate gate.
    #[must_use]
    pub fn with_rate_gate(mut self, gate: Arc<RateGate>) -> Self {
        self.rate_gate = Some(gate);
        self
    }
    /// Limit concurrency with a (possibly shared) bulkhead.
    #[must_use]
    pub fn with_bulkhead(mut self, bulkhead: Arc<Bulkhead>) -> Self {
        self.bulkhead = Some(bulkhead);
        self
    }
    /// Guard calls with a (possibly shared) circuit breaker.
    #[must_use]
    pub fn with_breaker(mut self, breaker: Arc<CircuitBreaker>) -> Self {
        self.breaker = Some(breaker);
        self
    }

    /// The policy's name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The per-attempt timeout.
    pub fn timeout(&self) -> &Timeout {
        &self.timeout
    }
    /// The retry policy.
    pub fn retry(&self) -> &RetryPolicy {
        &self.retry
    }
    /// The rate gate, if any.
    pub fn rate_gate(&self) -> Option<&Arc<RateGate>> {
        self.rate_gate.as_ref()
    }
    /// The bulkhead, if any.
    pub fn bulkhead(&self) -> Option<&Arc<Bulkhead>> {
        self.bulkhead.as_ref()
    }
    /// The circuit breaker, if any.
    pub fn breaker(&self) -> Option<&Arc<CircuitBreaker>> {
        self.breaker.as_ref()
    }

    /// Run `op` under the policy (see the type docs for the order).
    ///
    /// `idempotent` is the caller's promise that repeating `op` is safe;
    /// without it `op` runs at most once.
    pub async fn call<F, Fut, T>(
        &self,
        ctx: &CallContext,
        idempotent: bool,
        mut op: F,
    ) -> Result<T, AppError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, AppError>>,
    {
        ensure_live(ctx)?;
        if let Some(gate) = &self.rate_gate {
            gate.acquire_within(ctx).await?;
        }
        let _slot = match &self.bulkhead {
            Some(bulkhead) => Some(bulkhead.acquire(ctx).await?),
            None => None,
        };
        let permit = match &self.breaker {
            Some(breaker) => {
                ensure_live(ctx)?;
                Some(breaker.acquire()?)
            }
            None => None,
        };
        let invoked = AtomicBool::new(false);
        let timeout = &self.timeout;
        let result = self
            .retry
            .retry(ctx, idempotent, || {
                let attempt = op();
                let invoked = &invoked;
                timeout.call(ctx, async move {
                    invoked.store(true, Ordering::Relaxed);
                    attempt.await
                })
            })
            .await;
        if let Some(permit) = permit {
            if invoked.load(Ordering::Relaxed) && !caused_by_caller(ctx, &result) {
                permit.record(&result);
            } else {
                permit.release();
            }
        }
        result
    }
}

/// Whether `result` failed because of the caller — its own cancellation or
/// its own deadline running out — rather than because of the dependency.
fn caused_by_caller<T>(ctx: &CallContext, result: &Result<T, AppError>) -> bool {
    let Err(error) = result else {
        return false;
    };
    match error.code() {
        ErrorCode::Cancelled => ctx.cancel_token().is_cancelled(),
        ErrorCode::DeadlineExceeded => remaining(ctx) == Some(Duration::ZERO),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{Backoff, BreakerState, BreakerWindow, CircuitBreakerConfig};

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    fn retrying(attempts: u32) -> RetryPolicy {
        RetryPolicy::new(attempts, Backoff::constant(Duration::from_millis(100)))
    }

    #[tokio::test(start_paused = true)]
    async fn each_attempt_gets_its_own_timeout() {
        let policy = Policy::new("orders")
            .with_timeout(Timeout::new(secs(1)))
            .with_retry(retrying(3));
        let calls = Cell::new(0_u32);
        let started = Instant::now();
        let value = policy
            .call(&CallContext::new(), true, || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move {
                    if n == 1 {
                        futures::future::pending::<()>().await;
                    }
                    Ok(n)
                }
            })
            .await
            .unwrap();
        assert_eq!(value, 2);
        assert_eq!(started.elapsed(), Duration::from_millis(1100));
    }

    #[tokio::test(start_paused = true)]
    async fn non_idempotent_calls_run_once() {
        let policy = Policy::new("orders").with_retry(retrying(3));
        let calls = Cell::new(0_u32);
        let error = policy
            .call(&CallContext::new(), false, || {
                calls.set(calls.get() + 1);
                async { Err::<(), _>(AppError::unavailable("down")) }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn breaker_sees_one_outcome_per_logical_call() {
        let breaker = Arc::new(
            CircuitBreaker::new(
                "orders",
                CircuitBreakerConfig {
                    window: BreakerWindow::Count { size: 10 },
                    failure_rate: 0.5,
                    min_calls: 4,
                    wait_in_open: secs(30),
                    permitted_in_half_open: 1,
                },
            )
            .unwrap(),
        );
        let policy = Policy::new("orders")
            .with_retry(retrying(3))
            .with_breaker(Arc::clone(&breaker));
        let calls = Cell::new(0_u32);
        for _ in 0..3 {
            policy
                .call(&CallContext::new(), true, || {
                    calls.set(calls.get() + 1);
                    async { Err::<(), _>(AppError::unavailable("down")) }
                })
                .await
                .unwrap_err();
        }
        assert_eq!(calls.get(), 9);
        assert_eq!(breaker.metrics().calls, 3);
        assert_eq!(breaker.state(), BreakerState::Closed);
        policy
            .call(&CallContext::new(), true, || async {
                Err::<(), _>(AppError::unavailable("down"))
            })
            .await
            .unwrap_err();
        assert_eq!(breaker.state(), BreakerState::Open);

        calls.set(0);
        let error = policy
            .call(&CallContext::new(), true, || {
                calls.set(calls.get() + 1);
                async { Ok(()) }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.retry_after(), Some(secs(30)));
        assert_eq!(
            calls.get(),
            0,
            "an open breaker never reaches the operation"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bulkhead_and_rate_gate_run_before_the_operation() {
        let bulkhead = Arc::new(Bulkhead::new(1).unwrap());
        let gate = Arc::new(RateGate::new(1, secs(60)).unwrap());
        let policy = Policy::new("orders")
            .with_bulkhead(Arc::clone(&bulkhead))
            .with_rate_gate(Arc::clone(&gate));
        assert!(policy.rate_gate().is_some());
        assert!(policy.bulkhead().is_some());
        assert!(policy.breaker().is_none());

        let held = bulkhead.acquire(&CallContext::new()).await.unwrap();
        let error = policy
            .call(&CallContext::new(), true, || async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceExhausted, "bulkhead full");
        drop(held);

        let deadline = CallContext::new().with_deadline(Instant::now().into_std() + secs(1));
        let error = policy
            .call(&deadline, true, || async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            ErrorCode::ResourceExhausted,
            "gate closed until the window rolls"
        );
        assert_eq!(error.retry_after(), Some(secs(60)));
    }

    fn probing_breaker() -> Arc<CircuitBreaker> {
        Arc::new(
            CircuitBreaker::new(
                "orders",
                CircuitBreakerConfig {
                    window: BreakerWindow::Count { size: 4 },
                    failure_rate: 0.5,
                    min_calls: 1,
                    wait_in_open: secs(30),
                    permitted_in_half_open: 1,
                },
            )
            .unwrap(),
        )
    }

    async fn trip(policy: &Policy) {
        policy
            .call(&CallContext::new(), false, || async {
                Err::<(), _>(AppError::unavailable("down"))
            })
            .await
            .unwrap_err();
    }

    #[tokio::test(start_paused = true)]
    async fn dead_contexts_spend_nothing() {
        let gate = Arc::new(RateGate::new(1, secs(60)).unwrap());
        let breaker = probing_breaker();
        let policy = Policy::new("orders")
            .with_rate_gate(Arc::clone(&gate))
            .with_breaker(Arc::clone(&breaker));
        let calls = Cell::new(0_u32);

        let expired = CallContext::new().with_deadline(Instant::now().into_std());
        let error = policy
            .call(&expired, true, || {
                calls.set(calls.get() + 1);
                async { Ok(()) }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);

        let token = CancellationToken::new();
        token.cancel();
        let cancelled = CallContext::new().with_cancel(token);
        let error = policy
            .call(&cancelled, true, || {
                calls.set(calls.get() + 1);
                async { Ok(()) }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);

        assert_eq!(calls.get(), 0);
        assert_eq!(breaker.metrics().calls, 0);
        assert!(gate.try_acquire(), "no rate permit was spent");
    }

    #[tokio::test(start_paused = true)]
    async fn the_callers_own_deadline_does_not_count_against_the_dependency() {
        let breaker = probing_breaker();
        let policy = Policy::new("orders").with_breaker(Arc::clone(&breaker));
        trip(&policy).await;
        assert_eq!(breaker.state(), BreakerState::Open);
        tokio::time::advance(secs(30)).await;

        let ctx = CallContext::new().with_deadline(Instant::now().into_std() + secs(1));
        let error = policy
            .call(&ctx, true, futures::future::pending::<Result<(), AppError>>)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(
            breaker.state(),
            BreakerState::HalfOpen,
            "the probe was released, not failed"
        );

        policy
            .call(&CallContext::new(), true, || async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(
            breaker.state(),
            BreakerState::Closed,
            "the freed slot let a real probe through"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_callers_cancellation_does_not_count_against_the_dependency() {
        let breaker = probing_breaker();
        let policy = Policy::new("orders").with_breaker(Arc::clone(&breaker));
        let token = CancellationToken::new();
        let ctx = CallContext::new().with_cancel(token.clone());
        tokio::spawn(async move {
            tokio::time::sleep(secs(1)).await;
            token.cancel();
        });
        let error = policy
            .call(&ctx, true, futures::future::pending::<Result<(), AppError>>)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(
            breaker.metrics().calls,
            0,
            "the call reached the dependency, but the caller walked away"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_dependency_counts_as_a_failure() {
        let breaker = probing_breaker();
        let policy = Policy::new("orders")
            .with_timeout(Timeout::new(secs(1)))
            .with_breaker(Arc::clone(&breaker));
        let ctx = CallContext::new().with_deadline(Instant::now().into_std() + secs(10));
        let error = policy
            .call(&ctx, true, futures::future::pending::<Result<(), AppError>>)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(
            breaker.state(),
            BreakerState::Open,
            "one timed-out attempt is one failure, enough to open"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_operation_that_never_ran_is_not_recorded() {
        let breaker = probing_breaker();
        let policy = Policy::new("orders")
            .with_timeout(Timeout::new(Duration::ZERO))
            .with_breaker(Arc::clone(&breaker));
        let polled = Cell::new(false);
        let error = policy
            .call(&CallContext::new(), true, || {
                let polled = &polled;
                async move {
                    polled.set(true);
                    Ok(())
                }
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert!(!polled.get());
        assert_eq!(breaker.metrics().calls, 0);
    }

    #[test]
    fn only_caller_side_failures_are_attributed_to_the_caller() {
        let ctx = CallContext::new();
        assert!(!caused_by_caller(&ctx, &Ok::<(), AppError>(())));
        assert!(!caused_by_caller::<()>(
            &ctx,
            &Err(AppError::cancelled("upstream cancelled"))
        ));
        assert!(!caused_by_caller::<()>(
            &ctx,
            &Err(AppError::deadline_exceeded("upstream 504"))
        ));
        assert!(!caused_by_caller::<()>(
            &ctx,
            &Err(AppError::unavailable("down"))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn default_policy_enforces_only_the_deadline() {
        let policy = Policy::default();
        assert_eq!(policy.name(), "default");
        assert_eq!(policy.timeout().limit(), None);
        assert_eq!(policy.retry().max_attempts(), 1);
        let ctx = CallContext::new().with_deadline(Instant::now().into_std() + secs(2));
        let started = Instant::now();
        let error = policy
            .call(&ctx, true, || {
                futures::future::pending::<Result<(), AppError>>()
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(started.elapsed(), secs(2));
    }
}
