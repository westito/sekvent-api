use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Source of wall-clock time. Inject it instead of calling
/// `SystemTime::now()` so time-dependent logic is testable.
pub trait Clock: Send + Sync + 'static {
    /// Current wall-clock time.
    fn now(&self) -> SystemTime;

    /// Milliseconds since the Unix epoch (0 for times before it).
    fn now_unix_millis(&self) -> u64 {
        self.now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// The real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that only moves when told to. Cloning shares the same time.
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<Mutex<SystemTime>>,
}

impl ManualClock {
    /// A clock frozen at `start`.
    pub fn new(start: SystemTime) -> Self {
        Self {
            now: Arc::new(Mutex::new(start)),
        }
    }
    /// Move time forward.
    pub fn advance(&self, by: Duration) {
        let mut now = self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *now += by;
    }
    /// Jump to an exact time.
    pub fn set(&self, to: SystemTime) {
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = to;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch_plus(by: Duration) -> SystemTime {
        SystemTime::UNIX_EPOCH.checked_add(by).unwrap()
    }

    #[test]
    fn the_system_clock_reads_the_wall_clock() {
        let clock = SystemClock;
        let before = SystemTime::now();
        let now = clock.now();
        assert!(now >= before);
        assert!(now <= SystemTime::now());
        assert!(SystemClock.now_unix_millis() > 0);
    }

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let start = epoch_plus(Duration::from_secs(1_700_000_000));
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);
        assert_eq!(clock.now_unix_millis(), 1_700_000_000_000);

        clock.advance(Duration::from_millis(1_500));
        assert_eq!(clock.now(), start + Duration::from_millis(1_500));
        assert_eq!(clock.now_unix_millis(), 1_700_000_001_500);

        let later = epoch_plus(Duration::from_hours(1));
        clock.set(later);
        assert_eq!(clock.now(), later);
        assert_eq!(clock.now_unix_millis(), 3_600_000);
    }

    #[test]
    fn clones_share_the_same_time() {
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH);
        let shared = clock.clone();
        shared.advance(Duration::from_mins(1));
        assert_eq!(clock.now_unix_millis(), 60_000);
    }

    #[test]
    fn unix_millis_saturate_at_both_ends() {
        let before_epoch = SystemTime::UNIX_EPOCH
            .checked_sub(Duration::from_secs(1))
            .unwrap();
        assert_eq!(ManualClock::new(before_epoch).now_unix_millis(), 0);

        let far_future = epoch_plus(Duration::from_secs(1 << 62));
        assert_eq!(ManualClock::new(far_future).now_unix_millis(), u64::MAX);
    }

    #[test]
    fn a_poisoned_clock_keeps_working() {
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH);
        let shared = clock.clone();
        let poisoner = std::thread::spawn(move || {
            let _guard = shared.now.lock().unwrap();
            panic!("poison the clock");
        });
        assert!(poisoner.join().is_err());
        assert!(clock.now.is_poisoned());

        clock.advance(Duration::from_secs(2));
        assert_eq!(clock.now_unix_millis(), 2_000);
        clock.set(epoch_plus(Duration::from_secs(5)));
        assert_eq!(clock.now(), epoch_plus(Duration::from_secs(5)));
    }
}
