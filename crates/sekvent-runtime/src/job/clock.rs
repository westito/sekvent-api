use std::time::SystemTime;

use sekvent_context::Clock;
use tokio::time::Instant;

/// A wall clock that moves with tokio's clock: `start` plus tokio time
/// elapsed since `new`.
///
/// Under `tokio::time::pause` it makes cron and singleton schedules, which
/// follow the wall clock, advance exactly with the paused clock.
#[derive(Debug, Clone)]
pub struct TokioWallClock {
    start: SystemTime,
    origin: Instant,
}

impl TokioWallClock {
    /// A clock reading `start` now.
    pub fn new(start: SystemTime) -> Self {
        Self {
            start,
            origin: Instant::now(),
        }
    }
}

impl Clock for TokioWallClock {
    fn now(&self) -> SystemTime {
        let elapsed = Instant::now().saturating_duration_since(self.origin);
        self.start.checked_add(elapsed).unwrap_or(self.start)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_clock_follows_paused_tokio_time() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let clock = TokioWallClock::new(start);
        assert_eq!(clock.now(), start);

        tokio::time::advance(Duration::from_millis(1_500)).await;
        assert_eq!(clock.now(), start + Duration::from_millis(1_500));
        assert_eq!(clock.clone().now_unix_millis(), 1_001_500);
        assert!(format!("{clock:?}").starts_with("TokioWallClock {"));
    }
}
