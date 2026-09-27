use std::time::Duration;

use tokio::time::Instant;

/// Total time [`await_until!`](crate::await_until) waits unless told otherwise.
pub const DEFAULT_AWAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause between two checks unless told otherwise.
pub const DEFAULT_AWAIT_INTERVAL: Duration = Duration::from_millis(20);

/// Wait until an async condition holds, panicking after a bounded time.
///
/// For tests that must observe a side effect that has no completion signal
/// of its own (a row appearing, a counter moving). When the code under test
/// can signal completion (a channel, a join handle), await that instead.
///
/// The condition is any `bool` expression and may contain `.await`; it is
/// re-evaluated after every interval. The clock is Tokio's, so under
/// `tokio::time::pause` the wait is instant and deterministic.
///
/// ```ignore
/// await_until!(repo.count().await == 3);
/// await_until!(done.load(Ordering::SeqCst), timeout = Duration::from_secs(2));
/// await_until!(ready(), timeout = Duration::from_secs(2), interval = Duration::from_millis(5));
/// ```
#[macro_export]
macro_rules! await_until {
    ($cond:expr $(,)?) => {
        $crate::await_until!(
            $cond,
            timeout = $crate::DEFAULT_AWAIT_TIMEOUT,
            interval = $crate::DEFAULT_AWAIT_INTERVAL
        )
    };
    ($cond:expr, timeout = $timeout:expr $(,)?) => {
        $crate::await_until!(
            $cond,
            timeout = $timeout,
            interval = $crate::DEFAULT_AWAIT_INTERVAL
        )
    };
    ($cond:expr, timeout = $timeout:expr, interval = $interval:expr $(,)?) => {{
        let mut poller = $crate::__private::Poller::new($timeout, $interval);
        loop {
            if $cond {
                break;
            }
            poller.pause(::core::stringify!($cond)).await;
        }
    }};
}

/// The timing behind [`await_until!`](crate::await_until). Not public API.
#[derive(Debug)]
pub struct Poller {
    timeout: Duration,
    interval: Duration,
    deadline: Instant,
}

impl Poller {
    /// Start the clock.
    pub fn new(timeout: Duration, interval: Duration) -> Self {
        Self {
            timeout,
            interval: interval.max(Duration::from_millis(1)),
            deadline: Instant::now() + timeout,
        }
    }

    /// Sleep until the next check, or panic naming `what` once the deadline
    /// has passed.
    pub async fn pause(&mut self, what: &str) {
        let now = Instant::now();
        assert!(
            now < self.deadline,
            "await_until!: `{what}` still false after {:?}",
            self.timeout
        );
        tokio::time::sleep(self.interval.min(self.deadline - now)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn returns_as_soon_as_the_condition_holds() {
        let checks = AtomicU32::new(0);
        let start = Instant::now();
        crate::await_until!(checks.fetch_add(1, Ordering::SeqCst) >= 3);
        assert_eq!(checks.load(Ordering::SeqCst), 4);
        assert_eq!(start.elapsed(), DEFAULT_AWAIT_INTERVAL * 3);
    }

    #[tokio::test(start_paused = true)]
    async fn observes_progress_made_by_another_task() {
        let counter = Arc::new(AtomicU32::new(0));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let worker = {
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                rx.await.unwrap();
                counter.store(7, Ordering::SeqCst);
            })
        };
        tx.send(()).unwrap();
        crate::await_until!(
            async { counter.load(Ordering::SeqCst) == 7 }.await,
            timeout = Duration::from_secs(1),
        );
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "await_until!: `false` still false after 50ms")]
    async fn panics_with_the_condition_after_the_timeout() {
        crate::await_until!(
            false,
            timeout = Duration::from_millis(50),
            interval = Duration::from_millis(20)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_check_happens_at_the_deadline() {
        let start = Instant::now();
        let mut poller = Poller::new(Duration::from_millis(50), Duration::from_millis(20));
        poller.pause("x").await;
        poller.pause("x").await;
        poller.pause("x").await;
        assert_eq!(start.elapsed(), Duration::from_millis(50));
    }

    #[test]
    fn a_zero_interval_is_raised_to_one_millisecond() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let poller = runtime.block_on(async { Poller::new(Duration::ZERO, Duration::ZERO) });
        assert_eq!(poller.interval, Duration::from_millis(1));
    }
}
