use std::collections::VecDeque;
use std::time::Duration;

use sekvent_context::CallContext;
use sekvent_error::AppError;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::{PolicyError, cancelled_error, deadline_of};

/// Longest accepted window: a year and a day, well inside what an instant
/// can represent.
const MAX_WINDOW: Duration = Duration::from_hours(366 * 24);

/// At most `permits` acquisitions in any rolling `window`, shared by every
/// task holding the gate (wrap it in an `Arc`).
///
/// Typical use is an upstream quota such as "50 calls per 60 seconds for the
/// whole process". Waiters are served strictly first-come, first-served: a
/// waiter queues on a fair lock and, once at the head, sleeps until the
/// oldest grant leaves the window. Time is tokio's clock, so tests can use
/// `tokio::time::pause`.
#[derive(Debug)]
pub struct RateGate {
    permits: usize,
    window: Duration,
    grants: Mutex<VecDeque<Instant>>,
}

impl RateGate {
    /// A gate allowing `permits` acquisitions per rolling `window` (longer
    /// than zero, at most 366 days).
    pub fn new(permits: u32, window: Duration) -> Result<Self, PolicyError> {
        if permits == 0 {
            return Err(PolicyError::new("rate_gate.permits", "must be at least 1"));
        }
        if window.is_zero() {
            return Err(PolicyError::new(
                "rate_gate.window",
                "must be longer than zero",
            ));
        }
        if window > MAX_WINDOW {
            return Err(PolicyError::new(
                "rate_gate.window",
                "must be at most 366 days",
            ));
        }
        let permits = usize::try_from(permits).unwrap_or(usize::MAX);
        Ok(Self {
            permits,
            window,
            grants: Mutex::new(VecDeque::with_capacity(permits.min(1024))),
        })
    }

    /// Permits per window.
    pub fn permits(&self) -> usize {
        self.permits
    }

    /// The rolling window.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Wait for a permit, in arrival order. Dropping the future gives up
    /// the place in the queue without consuming a permit.
    pub async fn acquire(&self) {
        let mut grants = self.grants.lock().await;
        loop {
            let now = Instant::now();
            match self.wait_needed(&mut grants, now) {
                None => {
                    grants.push_back(now);
                    return;
                }
                Some(until) => tokio::time::sleep_until(until).await,
            }
        }
    }

    /// Wait for a permit within the call's deadline and cancellation.
    ///
    /// Fails fast with `RESOURCE_EXHAUSTED` (and a `retry_after` hint) when
    /// the gate cannot open before the deadline, with `DEADLINE_EXCEEDED`
    /// when the deadline passes while queued, and with `CANCELLED` on
    /// cancellation. Cancellation and expiry are checked before a permit is
    /// taken, so a call nobody waits for never spends quota.
    pub async fn acquire_within(&self, ctx: &CallContext) -> Result<(), AppError> {
        let deadline = deadline_of(ctx);
        let wait = async {
            let mut grants = self.grants.lock().await;
            loop {
                let now = Instant::now();
                match self.wait_needed(&mut grants, now) {
                    None => {
                        grants.push_back(now);
                        return Ok(());
                    }
                    Some(until) => {
                        if deadline.is_some_and(|deadline| until > deadline) {
                            return Err(AppError::resource_exhausted(
                                "rate limit reached; no permit before the deadline",
                            )
                            .with_retry_after(until.saturating_duration_since(now)));
                        }
                        tokio::time::sleep_until(until).await;
                    }
                }
            }
        };
        let expiry = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => futures::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = ctx.cancelled() => Err(cancelled_error()),
            () = expiry => Err(AppError::deadline_exceeded(
                "the call deadline passed while waiting for a rate permit",
            )),
            outcome = wait => outcome,
        }
    }

    /// Take a permit if one is free right now and nobody is queued ahead.
    pub fn try_acquire(&self) -> bool {
        let Ok(mut grants) = self.grants.try_lock() else {
            return false;
        };
        let now = Instant::now();
        if self.wait_needed(&mut grants, now).is_none() {
            grants.push_back(now);
            true
        } else {
            false
        }
    }

    /// Drop grants that left the window; `Some(instant)` when the gate is
    /// full and the next permit frees up at `instant`.
    fn wait_needed(&self, grants: &mut VecDeque<Instant>, now: Instant) -> Option<Instant> {
        while grants
            .front()
            .is_some_and(|granted| now.saturating_duration_since(*granted) >= self.window)
        {
            grants.pop_front();
        }
        if grants.len() < self.permits {
            None
        } else {
            // The window is bounded at construction, so the fallbacks only
            // guard against an instant at the edge of the platform's range.
            grants.front().map(|oldest| {
                oldest
                    .checked_add(self.window)
                    .or_else(|| now.checked_add(self.window))
                    .unwrap_or(now)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sekvent_error::ErrorCode;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn rejects_nonsense() {
        assert_eq!(
            RateGate::new(0, SECOND).unwrap_err().parameter(),
            "rate_gate.permits"
        );
        assert_eq!(
            RateGate::new(1, Duration::ZERO).unwrap_err().parameter(),
            "rate_gate.window"
        );
        assert_eq!(
            RateGate::new(1, Duration::from_hours(367 * 24))
                .unwrap_err()
                .parameter(),
            "rate_gate.window"
        );
        assert_eq!(
            RateGate::new(1, Duration::MAX).unwrap_err().parameter(),
            "rate_gate.window"
        );
        assert!(RateGate::new(1, MAX_WINDOW).is_ok());
        let gate = RateGate::new(3, SECOND).unwrap();
        assert_eq!(gate.permits(), 3);
        assert_eq!(gate.window(), SECOND);
    }

    #[tokio::test(start_paused = true)]
    async fn window_rolls_over() {
        let gate = RateGate::new(2, Duration::from_secs(10)).unwrap();
        assert!(gate.try_acquire());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(gate.try_acquire());
        assert!(!gate.try_acquire());
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(gate.try_acquire(), "the first grant left the window");
        assert!(!gate.try_acquire());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(gate.try_acquire(), "the second grant left the window");
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waits_for_the_oldest_grant_to_expire() {
        let gate = RateGate::new(2, Duration::from_secs(60)).unwrap();
        let started = Instant::now();
        gate.acquire().await;
        gate.acquire().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
        gate.acquire().await;
        assert_eq!(started.elapsed(), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn waiters_are_served_in_arrival_order() {
        let gate = Arc::new(RateGate::new(1, SECOND).unwrap());
        gate.acquire().await;
        let started = Instant::now();
        let (tx, mut rx) = mpsc::unbounded_channel();
        for id in 0..4 {
            let gate = Arc::clone(&gate);
            let tx = tx.clone();
            tokio::spawn(async move {
                gate.acquire().await;
                tx.send((id, started.elapsed())).unwrap();
            });
            settle().await;
        }
        drop(tx);
        assert!(!gate.try_acquire(), "no barging past queued waiters");
        let mut order = Vec::new();
        while let Some(entry) = rx.recv().await {
            order.push(entry);
        }
        assert_eq!(
            order,
            vec![
                (0, SECOND),
                (1, 2 * SECOND),
                (2, 3 * SECOND),
                (3, 4 * SECOND)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_waiter_does_not_consume_a_permit() {
        let gate = RateGate::new(1, SECOND).unwrap();
        gate.acquire().await;
        let waiting = tokio::time::timeout(Duration::from_millis(500), gate.acquire()).await;
        assert!(waiting.is_err());
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(gate.try_acquire());
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_within_respects_the_deadline() {
        let gate = RateGate::new(1, Duration::from_secs(60)).unwrap();
        let ctx = CallContext::new();
        gate.acquire_within(&ctx).await.unwrap();

        let ctx = CallContext::new().with_deadline(Instant::now().into_std() + SECOND);
        let error = gate.acquire_within(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(60)));

        let ctx = CallContext::new().with_deadline(Instant::now().into_std() + 2 * SECOND);
        let started = Instant::now();
        let error = gate.acquire_within(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "fails fast instead of waiting"
        );

        tokio::time::advance(Duration::from_secs(59)).await;
        let ctx = CallContext::new().with_deadline(Instant::now().into_std() + 2 * SECOND);
        let started = Instant::now();
        gate.acquire_within(&ctx).await.unwrap();
        assert_eq!(started.elapsed(), SECOND);
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_within_times_out_while_queued() {
        let gate = Arc::new(RateGate::new(1, Duration::from_secs(10)).unwrap());
        gate.acquire().await;
        let holder = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.acquire().await })
        };
        settle().await;
        let ctx = CallContext::new().with_deadline(Instant::now().into_std() + SECOND);
        let error = gate.acquire_within(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        holder.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn dead_calls_never_consume_quota() {
        let gate = RateGate::new(1, Duration::from_secs(10)).unwrap();
        let expired = CallContext::new().with_deadline(Instant::now().into_std());
        let error = gate.acquire_within(&expired).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);

        let token = CancellationToken::new();
        token.cancel();
        let cancelled = CallContext::new().with_cancel(token);
        let error = gate.acquire_within(&cancelled).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);

        assert!(gate.try_acquire(), "the permit is still free");
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_within_stops_on_cancellation() {
        let gate = RateGate::new(1, Duration::from_secs(10)).unwrap();
        gate.acquire().await;
        let token = CancellationToken::new();
        let ctx = CallContext::new().with_cancel(token.clone());
        token.cancel();
        let error = gate.acquire_within(&ctx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
    }
}
