//! Timeouts, retries with budgets, rate gates, bulkheads, circuit breakers
//! and a TTL cache.
//!
//! The building blocks are transport-agnostic: they work on
//! [`AppError`](sekvent_error::AppError) and [`CallContext`], so the same
//! policy serves the HTTP client, gRPC clients and any other outbound call.
//!
//! - [`Backoff`]: exponential delays with optional jitter and injectable randomness.
//! - [`RetryBudget`] and [`RetryPolicy`]: bounded, budgeted retries of
//!   transient failures of idempotent operations.
//! - [`RateGate`]: at most N calls per rolling window, shared process-wide.
//! - [`Timeout`]: a per-attempt timeout that never outlives the call deadline.
//! - [`Bulkhead`]: a concurrency cap with an optional bounded wait queue.
//! - [`CircuitBreaker`]: stops calling an unhealthy dependency for a while.
//! - [`Policy`] composes all of them in a fixed order; [`PolicySpec`] is its
//!   serializable, layerable description; [`PolicyLayer`] adapts it to tower.
//! - [`TtlCache`]: a small in-memory cache with per-key single flight and
//!   optional stale serving when a reload fails transiently.
//!
//! # Time
//!
//! Every wait uses tokio's clock, so tests drive it with
//! `tokio::time::pause` and `advance`. Deadlines are read from the context
//! and measured against tokio's clock as well (see [`remaining`]). The
//! breaker and the cache take an injectable [`MonotonicClock`].

#![forbid(unsafe_code)]

mod backoff;
mod breaker;
mod budget;
mod bulkhead;
mod cache;
mod error;
mod policy;
mod rate_gate;
mod retry;
mod spec;
mod timeout;
mod tower_layer;

use std::time::Duration;

use sekvent_context::CallContext;

pub use backoff::{Backoff, BackoffIter, Jitter};
pub use breaker::{
    BreakerMetrics, BreakerPermit, BreakerState, BreakerWindow, CircuitBreaker,
    CircuitBreakerConfig, FailureClassifier, MonotonicClock, StateTransition, TokioClock,
    WallClock,
};
pub use budget::RetryBudget;
pub use bulkhead::{Bulkhead, BulkheadPermit};
pub use cache::{TtlCache, TtlCacheBuilder};
pub use error::PolicyError;
pub use policy::Policy;
pub use rate_gate::RateGate;
pub use retry::RetryPolicy;
pub use spec::{BreakerWindowSpec, PolicySpec};
pub use timeout::Timeout;
pub use tower_layer::{PolicyLayer, PolicyRequest, PolicyService};

/// Time left before `ctx`'s deadline, measured on tokio's clock.
///
/// `None` without a deadline; zero once it has passed. Unlike
/// [`CallContext::remaining`], this follows `tokio::time::pause` and
/// `advance`, which keeps deadline logic deterministic under test.
pub fn remaining(ctx: &CallContext) -> Option<Duration> {
    ctx.deadline().map(|deadline| {
        tokio::time::Instant::from_std(deadline)
            .saturating_duration_since(tokio::time::Instant::now())
    })
}

/// The deadline of `ctx` as a tokio instant, if any.
pub(crate) fn deadline_of(ctx: &CallContext) -> Option<tokio::time::Instant> {
    ctx.deadline().map(tokio::time::Instant::from_std)
}

/// The error returned when the call's cancellation token fires.
pub(crate) fn cancelled_error() -> sekvent_error::AppError {
    sekvent_error::AppError::cancelled("the call was cancelled")
}

/// The error returned when the call's deadline has passed.
pub(crate) fn deadline_error() -> sekvent_error::AppError {
    sekvent_error::AppError::deadline_exceeded("the call deadline was exceeded")
}

/// `Err` when `ctx` is already cancelled or past its deadline, so no work
/// (and no shared quota) is spent on a call nobody waits for.
pub(crate) fn ensure_live(ctx: &CallContext) -> Result<(), sekvent_error::AppError> {
    if ctx.cancel_token().is_cancelled() {
        return Err(cancelled_error());
    }
    if remaining(ctx) == Some(Duration::ZERO) {
        return Err(deadline_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn remaining_follows_the_paused_clock() {
        let now = tokio::time::Instant::now().into_std();
        let ctx = CallContext::new().with_deadline(now + Duration::from_secs(5));
        assert_eq!(remaining(&ctx), Some(Duration::from_secs(5)));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(remaining(&ctx), Some(Duration::from_secs(3)));
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(remaining(&ctx), Some(Duration::ZERO));
        assert_eq!(remaining(&CallContext::new()), None);
        assert!(deadline_of(&CallContext::new()).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn ensure_live_rejects_dead_contexts() {
        assert!(ensure_live(&CallContext::new()).is_ok());
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let cancelled = CallContext::new().with_cancel(token);
        assert_eq!(
            ensure_live(&cancelled).unwrap_err().code(),
            sekvent_error::ErrorCode::Cancelled
        );
        let expired = CallContext::new().with_deadline(tokio::time::Instant::now().into_std());
        assert_eq!(
            ensure_live(&expired).unwrap_err().code(),
            sekvent_error::ErrorCode::DeadlineExceeded
        );
    }

    #[test]
    fn helper_errors_have_the_right_codes() {
        assert_eq!(
            cancelled_error().code(),
            sekvent_error::ErrorCode::Cancelled
        );
        assert_eq!(
            deadline_error().code(),
            sekvent_error::ErrorCode::DeadlineExceeded
        );
    }
}
