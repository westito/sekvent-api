use std::future::Future;
use std::sync::Arc;

use sekvent_context::CallContext;
use sekvent_error::AppError;

use crate::{Bulkhead, CircuitBreaker, RateGate, RetryPolicy, Timeout};

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
        if let Some(gate) = &self.rate_gate {
            gate.acquire_within(ctx).await?;
        }
        let _slot = match &self.bulkhead {
            Some(bulkhead) => Some(bulkhead.acquire(ctx).await?),
            None => None,
        };
        let permit = match &self.breaker {
            Some(breaker) => Some(breaker.acquire()?),
            None => None,
        };
        let timeout = &self.timeout;
        let result = self
            .retry
            .retry(ctx, idempotent, || timeout.call(ctx, op()))
            .await;
        if let Some(permit) = permit {
            permit.record(&result);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Duration;

    use sekvent_error::ErrorCode;
    use tokio::time::Instant;

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
