use std::future::Future;
use std::time::Duration;

use sekvent_context::CallContext;
use sekvent_error::AppError;

use crate::remaining;

/// A deadline-aware timeout.
///
/// The effective limit of one call is the smaller of the configured
/// duration and the time left before the context's deadline, so a callee
/// never runs longer than its caller is willing to wait. Without either,
/// the call is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timeout {
    limit: Option<Duration>,
}

impl Timeout {
    /// Limit each call to `limit` (and to the context deadline).
    pub fn new(limit: Duration) -> Self {
        Self { limit: Some(limit) }
    }

    /// Only the context deadline limits the call.
    pub fn deadline_only() -> Self {
        Self { limit: None }
    }

    /// The configured limit, if any.
    pub fn limit(&self) -> Option<Duration> {
        self.limit
    }

    /// `min(configured, ctx remaining)`; `None` when neither is set.
    pub fn effective(&self, ctx: &CallContext) -> Option<Duration> {
        match (self.limit, remaining(ctx)) {
            (Some(limit), Some(left)) => Some(limit.min(left)),
            (limit, left) => limit.or(left),
        }
    }

    /// Run `fut` within the effective limit; expiry is `DEADLINE_EXCEEDED`.
    pub async fn call<Fut, T>(&self, ctx: &CallContext, fut: Fut) -> Result<T, AppError>
    where
        Fut: Future<Output = Result<T, AppError>>,
    {
        match self.effective(ctx) {
            None => fut.await,
            Some(limit) if limit.is_zero() => Err(expired(limit)),
            Some(limit) => tokio::time::timeout(limit, fut)
                .await
                .unwrap_or_else(|_| Err(expired(limit))),
        }
    }
}

fn expired(limit: Duration) -> AppError {
    AppError::deadline_exceeded("the call did not complete in time")
        .with_metadata("timeout_ms", limit.as_millis().to_string())
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;
    use tokio::time::Instant;

    use super::*;

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    fn ctx_with_deadline(after: Duration) -> CallContext {
        CallContext::new().with_deadline(Instant::now().into_std() + after)
    }

    #[tokio::test(start_paused = true)]
    async fn effective_is_the_smaller_limit() {
        assert_eq!(
            Timeout::new(secs(5)).effective(&CallContext::new()),
            Some(secs(5))
        );
        assert_eq!(
            Timeout::new(secs(5)).effective(&ctx_with_deadline(secs(2))),
            Some(secs(2))
        );
        assert_eq!(
            Timeout::new(secs(1)).effective(&ctx_with_deadline(secs(2))),
            Some(secs(1))
        );
        assert_eq!(
            Timeout::deadline_only().effective(&ctx_with_deadline(secs(3))),
            Some(secs(3))
        );
        assert_eq!(
            Timeout::deadline_only().effective(&CallContext::new()),
            None
        );
        assert_eq!(Timeout::default().limit(), None);
        assert_eq!(Timeout::new(secs(4)).limit(), Some(secs(4)));
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_is_deadline_exceeded() {
        let started = Instant::now();
        let error = Timeout::new(secs(10))
            .call(
                &ctx_with_deadline(secs(3)),
                futures::future::pending::<Result<(), AppError>>(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(
            error.metadata().get("timeout_ms").map(String::as_str),
            Some("3000")
        );
        assert_eq!(started.elapsed(), secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn completes_within_the_limit() {
        let value = Timeout::new(secs(1))
            .call(&CallContext::new(), async { Ok::<_, AppError>(7) })
            .await
            .unwrap();
        assert_eq!(value, 7);
        let value = Timeout::deadline_only()
            .call(&CallContext::new(), async { Ok::<_, AppError>(8) })
            .await
            .unwrap();
        assert_eq!(value, 8);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_deadline_fails_without_polling() {
        let ctx = ctx_with_deadline(secs(1));
        tokio::time::advance(secs(2)).await;
        let polled = std::cell::Cell::new(false);
        let error = Timeout::new(secs(5))
            .call(&ctx, async {
                polled.set(true);
                Ok::<(), AppError>(())
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert!(!polled.get());
    }
}
