//! Schedule arithmetic without I/O: tick grids, jitter, the decision at a
//! tick and the catch-up rule.

#[cfg(feature = "cron")]
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::time::Instant;

/// Longest single sleep while waiting for a wall-clock time, so clock
/// corrections are noticed.
pub(crate) const WALL_POLL: Duration = Duration::from_mins(1);
/// Longest wait for a guard permit's release after a run.
pub(crate) const RELEASE_WAIT: Duration = Duration::from_secs(5);
/// Longest wait for the guard's answer to a trigger.
pub(crate) const TRIGGER_ACQUIRE_WAIT: Duration = Duration::from_secs(5);
/// How late a tick may start by default.
pub(crate) const DEFAULT_MISFIRE_GRACE: Duration = Duration::from_mins(1);
/// The smallest misfire grace a job accepts: timers fire slightly late, so a
/// shorter grace would misfire every tick.
pub(crate) const MIN_MISFIRE_GRACE: Duration = Duration::from_secs(1);
/// Most cron ticks counted in one pass-over; a longer gap is counted as this.
#[cfg(feature = "cron")]
pub(crate) const PASS_OVER_COUNT_CAP: u64 = 10_000;
/// Largest jitter a cron job accepts.
#[cfg(feature = "cron")]
pub(crate) const MAX_CRON_JITTER: Duration = Duration::from_hours(1);

const FAR_FUTURE: Duration = Duration::from_hours(100 * 365 * 24);
const NANOS_PER_SEC: u128 = 1_000_000_000;

fn duration_from_nanos(nanos: u128) -> Option<Duration> {
    let secs = u64::try_from(nanos / NANOS_PER_SEC).ok()?;
    let subsec = u32::try_from(nanos % NANOS_PER_SEC).ok()?;
    Some(Duration::new(secs, subsec))
}

/// `first + index × period` on tokio's clock, capped a century ahead.
pub(crate) fn local_tick(first: Instant, period: Duration, index: u64) -> Instant {
    let offset = duration_from_nanos(period.as_nanos().saturating_mul(u128::from(index)))
        .map_or(FAR_FUTURE, |offset| offset.min(FAR_FUTURE));
    first.checked_add(offset).unwrap_or(first)
}

/// The index of the first tick of `first + k × period` at or after `floor`.
pub(crate) fn local_index_at_or_after(first: Instant, period: Duration, floor: Instant) -> u64 {
    let gap = floor.saturating_duration_since(first).as_nanos();
    u64::try_from(gap.div_ceil(period.as_nanos().max(1))).unwrap_or(u64::MAX)
}

/// The index of the first tick of `first + k × period` strictly after `floor`.
pub(crate) fn local_index_after(first: Instant, period: Duration, floor: Instant) -> u64 {
    floor.checked_duration_since(first).map_or(0, |gap| {
        let whole = gap.as_nanos() / period.as_nanos().max(1);
        u64::try_from(whole.saturating_add(1)).unwrap_or(u64::MAX)
    })
}

fn nanos_since_epoch(t: SystemTime) -> Option<u128> {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_nanos())
}

fn epoch_plus_nanos(nanos: u128) -> Option<SystemTime> {
    SystemTime::UNIX_EPOCH.checked_add(duration_from_nanos(nanos)?)
}

/// The first tick of `UNIX_EPOCH + k × period` at or after `t`.
pub(crate) fn epoch_at_or_after(t: SystemTime, period: Duration) -> Option<SystemTime> {
    let period = period.as_nanos().max(1);
    let since = nanos_since_epoch(t).unwrap_or(0);
    epoch_plus_nanos(since.div_ceil(period).checked_mul(period)?)
}

/// The last tick of `UNIX_EPOCH + k × period` at or before `t`.
pub(crate) fn epoch_at_or_before(t: SystemTime, period: Duration) -> Option<SystemTime> {
    let period = period.as_nanos().max(1);
    epoch_plus_nanos(nanos_since_epoch(t)? / period * period)
}

/// The first tick of `UNIX_EPOCH + k × period` strictly after `t`.
pub(crate) fn epoch_after(t: SystemTime, period: Duration) -> Option<SystemTime> {
    let period = period.as_nanos().max(1);
    let Some(since) = nanos_since_epoch(t) else {
        return Some(SystemTime::UNIX_EPOCH);
    };
    epoch_plus_nanos((since / period).checked_add(1)?.checked_mul(period)?)
}

#[cfg(feature = "cron")]
pub(crate) use cron::{cron_after, cron_at_or_after, cron_at_or_before, parse_cron};

#[cfg(feature = "cron")]
mod cron {
    use std::time::SystemTime;

    use chrono::{DateTime, Timelike, Utc};
    use croner::Cron;
    use croner::errors::CronError;
    use croner::parser::{CronParser, Seconds, Year};

    /// Five fields, or six with leading seconds; no year field.
    pub(crate) fn parse_cron(pattern: &str) -> Result<Cron, CronError> {
        CronParser::builder()
            .seconds(Seconds::Optional)
            .year(Year::Disallowed)
            .build()
            .parse(pattern)
    }

    fn floor_second(t: SystemTime) -> DateTime<Utc> {
        let at = DateTime::<Utc>::from(t);
        at.with_nanosecond(0).unwrap_or(at)
    }

    fn ceil_second(t: SystemTime) -> Option<DateTime<Utc>> {
        let floor = floor_second(t);
        if SystemTime::from(floor) == t {
            Some(floor)
        } else {
            floor.checked_add_signed(chrono::TimeDelta::seconds(1))
        }
    }

    /// The first occurrence strictly after `t`, in UTC.
    pub(crate) fn cron_after(cron: &Cron, t: SystemTime) -> Option<SystemTime> {
        cron.find_next_occurrence(&floor_second(t), false)
            .ok()
            .map(SystemTime::from)
    }

    /// The first occurrence at or after `t`, in UTC.
    pub(crate) fn cron_at_or_after(cron: &Cron, t: SystemTime) -> Option<SystemTime> {
        cron.find_next_occurrence(&ceil_second(t)?, true)
            .ok()
            .map(SystemTime::from)
    }

    /// The last occurrence at or before `t`, in UTC.
    pub(crate) fn cron_at_or_before(cron: &Cron, t: SystemTime) -> Option<SystemTime> {
        cron.find_previous_occurrence(&floor_second(t), true)
            .ok()
            .map(SystemTime::from)
    }
}

/// Ticks that follow the wall clock: singleton intervals and cron.
#[derive(Debug, Clone)]
pub(crate) enum WallGrid {
    /// `UNIX_EPOCH + k × period`.
    Epoch(Duration),
    /// A parsed cron pattern, in UTC.
    #[cfg(feature = "cron")]
    Cron(Arc<croner::Cron>),
}

/// Ticks passed over in one step, none of them run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PassedOver {
    /// How many; for cron at most `PASS_OVER_COUNT_CAP`.
    pub(crate) count: u64,
    /// The first one.
    pub(crate) first: SystemTime,
    /// The last one.
    pub(crate) last: SystemTime,
}

impl WallGrid {
    /// The first tick at or after `t`.
    pub(crate) fn at_or_after(&self, t: SystemTime) -> Option<SystemTime> {
        match self {
            Self::Epoch(period) => epoch_at_or_after(t, *period),
            #[cfg(feature = "cron")]
            Self::Cron(cron) => cron_at_or_after(cron, t),
        }
    }

    /// The last tick at or before `t`.
    pub(crate) fn at_or_before(&self, t: SystemTime) -> Option<SystemTime> {
        match self {
            Self::Epoch(period) => epoch_at_or_before(t, *period),
            #[cfg(feature = "cron")]
            Self::Cron(cron) => cron_at_or_before(cron, t),
        }
    }

    /// The first tick after `prev` that is at or after `floor`, so ticks
    /// already too late to run are passed over in one step.
    pub(crate) fn next(&self, prev: SystemTime, floor: SystemTime) -> Option<SystemTime> {
        match self {
            Self::Epoch(period) => {
                let regular = prev.checked_add(*period)?;
                Some(regular.max(epoch_at_or_after(floor, *period)?))
            }
            #[cfg(feature = "cron")]
            Self::Cron(cron) => {
                if floor > prev {
                    cron_at_or_after(cron, floor)
                } else {
                    cron_after(cron, prev)
                }
            }
        }
    }

    /// The first tick strictly after `t`.
    pub(crate) fn after(&self, t: SystemTime) -> Option<SystemTime> {
        match self {
            Self::Epoch(period) => epoch_after(t, *period),
            #[cfg(feature = "cron")]
            Self::Cron(cron) => cron_after(cron, t),
        }
    }

    /// The ticks strictly between `prev` and `next`, both ticks of this
    /// grid; `None` when there are none.
    pub(crate) fn between(&self, prev: SystemTime, next: SystemTime) -> Option<PassedOver> {
        match self {
            Self::Epoch(period) => {
                let steps = next.duration_since(prev).ok()?.as_nanos() / period.as_nanos().max(1);
                let count = u64::try_from(steps.checked_sub(1)?).unwrap_or(u64::MAX);
                if count == 0 {
                    return None;
                }
                Some(PassedOver {
                    count,
                    first: prev.checked_add(*period)?,
                    last: next.checked_sub(*period)?,
                })
            }
            #[cfg(feature = "cron")]
            Self::Cron(cron) => {
                let first = cron_after(cron, prev).filter(|first| *first < next)?;
                let mut count = 1;
                let mut at = first;
                while count < PASS_OVER_COUNT_CAP {
                    let Some(tick) = cron_after(cron, at).filter(|tick| *tick < next) else {
                        break;
                    };
                    at = tick;
                    count += 1;
                }
                let last = cron_at_or_before(cron, next.checked_sub(Duration::from_secs(1))?)?;
                Some(PassedOver { count, first, last })
            }
        }
    }
}

/// What happens at a tick, before any guard is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtTick {
    /// Start a run.
    Run,
    /// A run of the job is in progress here: skip.
    Overlap,
    /// Later than the misfire grace: skip.
    Misfire,
}

/// The decision at a tick `lateness` after its (jittered) time.
pub(crate) fn at_tick(busy: bool, lateness: Duration, grace: Duration) -> AtTick {
    if busy {
        AtTick::Overlap
    } else if lateness > grace {
        AtTick::Misfire
    } else {
        AtTick::Run
    }
}

/// Whether a guarded job catches up on `prev`, the latest tick at or before
/// `now`: the guard's last tick is older (or unknown) and `prev` is still
/// within the grace.
pub(crate) fn catch_up_due(
    prev: SystemTime,
    last: Option<SystemTime>,
    now: SystemTime,
    grace: Duration,
) -> bool {
    last.is_none_or(|last| last < prev)
        && now
            .duration_since(prev)
            .is_ok_and(|lateness| lateness <= grace)
}

/// Pause between guard attempts at one tick.
pub(crate) fn guard_retry(grace: Duration) -> Duration {
    (grace / 4).clamp(Duration::from_millis(100), Duration::from_secs(5))
}

/// A uniform delay in `[0, max]` at nanosecond resolution from one random word.
pub(crate) fn jitter_from(max: Duration, word: u64) -> Duration {
    let span = u128::from(u64::try_from(max.as_nanos()).unwrap_or(u64::MAX)) + 1;
    let pick = (u128::from(word) * span) >> 64;
    duration_from_nanos(pick).unwrap_or(max)
}

/// How long to sleep before looking at the wall clock again for `fire`;
/// `None` when it is due.
pub(crate) fn wall_wait(fire: SystemTime, now: SystemTime) -> Option<Duration> {
    let wait = fire
        .duration_since(now)
        .ok()
        .filter(|wait| !wait.is_zero())?;
    Some(wait.min(WALL_POLL))
}

/// Milliseconds since the Unix epoch, for logs (0 before it).
pub(crate) fn unix_millis(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    fn epoch(value: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + secs(value)
    }

    #[tokio::test(start_paused = true)]
    async fn the_in_process_grid_is_anchored_at_its_first_tick() {
        let first = Instant::now() + secs(10);
        assert_eq!(local_tick(first, secs(10), 0), first);
        assert_eq!(local_tick(first, secs(10), 3), first + secs(30));
        assert_eq!(
            local_tick(first, secs(10), u64::MAX),
            first + FAR_FUTURE,
            "huge indexes are capped a century ahead"
        );

        assert_eq!(local_index_at_or_after(first, secs(10), first - secs(5)), 0);
        assert_eq!(local_index_at_or_after(first, secs(10), first), 0);
        assert_eq!(local_index_at_or_after(first, secs(10), first + secs(1)), 1);
        assert_eq!(
            local_index_at_or_after(first, secs(10), first + secs(20)),
            2
        );
        assert_eq!(
            local_index_at_or_after(first, secs(10), first + secs(21)),
            3
        );

        assert_eq!(local_index_after(first, secs(10), first - secs(5)), 0);
        assert_eq!(local_index_after(first, secs(10), first), 1, "exclusive");
        assert_eq!(local_index_after(first, secs(10), first + secs(1)), 1);
        assert_eq!(local_index_after(first, secs(10), first + secs(20)), 3);
        assert_eq!(local_index_after(first, secs(10), first + secs(21)), 3);
    }

    #[test]
    fn epoch_ticks_are_aligned_to_the_unix_epoch() {
        let period = secs(15 * 60);
        assert_eq!(epoch_at_or_after(epoch(0), period), Some(epoch(0)));
        assert_eq!(epoch_at_or_after(epoch(1), period), Some(epoch(900)));
        assert_eq!(epoch_at_or_after(epoch(900), period), Some(epoch(900)));
        assert_eq!(
            epoch_at_or_after(epoch(900) + Duration::from_nanos(1), period),
            Some(epoch(1800))
        );
        assert_eq!(epoch_at_or_before(epoch(1799), period), Some(epoch(900)));
        assert_eq!(epoch_at_or_before(epoch(1800), period), Some(epoch(1800)));

        let before_epoch = SystemTime::UNIX_EPOCH - secs(5);
        assert_eq!(epoch_at_or_after(before_epoch, period), Some(epoch(0)));
        assert_eq!(epoch_at_or_before(before_epoch, period), None);

        assert_eq!(epoch_after(epoch(0), period), Some(epoch(900)));
        assert_eq!(epoch_after(epoch(1), period), Some(epoch(900)));
        assert_eq!(epoch_after(epoch(900), period), Some(epoch(1800)));
        assert_eq!(epoch_after(before_epoch, period), Some(epoch(0)));

        let millis = Duration::from_millis(1_500);
        assert_eq!(
            epoch_at_or_after(epoch(1_000_001), millis),
            Some(epoch(1_000_002))
        );
        assert_eq!(
            epoch_at_or_before(epoch(1_000_001), millis),
            Some(epoch(1_000_000) + Duration::from_millis(500))
        );
    }

    #[test]
    fn the_epoch_grid_steps_and_skips_ticks_already_too_late() {
        let grid = WallGrid::Epoch(secs(10));
        assert_eq!(grid.at_or_after(epoch(1_000_003)), Some(epoch(1_000_010)));
        assert_eq!(grid.at_or_before(epoch(1_000_003)), Some(epoch(1_000_000)));
        assert_eq!(
            grid.next(epoch(1_000_010), epoch(1_000_000)),
            Some(epoch(1_000_020))
        );
        assert_eq!(
            grid.next(epoch(1_000_010), epoch(1_000_045)),
            Some(epoch(1_000_050))
        );
        assert_eq!(grid.next(SystemTime::UNIX_EPOCH, epoch(0)), Some(epoch(10)));
    }

    #[test]
    fn the_epoch_grid_counts_the_ticks_it_passes_over() {
        let grid = WallGrid::Epoch(secs(10));
        assert_eq!(grid.after(epoch(1_000_010)), Some(epoch(1_000_020)));
        assert_eq!(grid.after(epoch(1_000_015)), Some(epoch(1_000_020)));
        assert_eq!(grid.between(epoch(1_000_010), epoch(1_000_020)), None);
        assert_eq!(grid.between(epoch(1_000_010), epoch(1_000_010)), None);
        assert_eq!(grid.between(epoch(1_000_020), epoch(1_000_010)), None);
        assert_eq!(
            grid.between(epoch(1_000_010), epoch(1_000_050)),
            Some(PassedOver {
                count: 3,
                first: epoch(1_000_020),
                last: epoch(1_000_040),
            })
        );
    }

    #[test]
    fn the_tick_decision_follows_the_table() {
        let grace = secs(60);
        assert_eq!(at_tick(true, Duration::ZERO, grace), AtTick::Overlap);
        assert_eq!(at_tick(true, secs(600), grace), AtTick::Overlap);
        assert_eq!(at_tick(false, secs(61), grace), AtTick::Misfire);
        assert_eq!(at_tick(false, secs(60), grace), AtTick::Run);
        assert_eq!(at_tick(false, Duration::ZERO, grace), AtTick::Run);
    }

    #[test]
    fn catch_up_runs_only_for_a_newer_tick_within_the_grace() {
        let prev = epoch(1_000_000);
        let now = epoch(1_000_003);
        let grace = secs(60);
        assert!(catch_up_due(prev, None, now, grace));
        assert!(catch_up_due(prev, Some(epoch(999_990)), now, grace));
        assert!(!catch_up_due(prev, Some(prev), now, grace));
        assert!(!catch_up_due(prev, Some(epoch(1_000_010)), now, grace));
        assert!(!catch_up_due(prev, None, now, secs(2)));
        assert!(catch_up_due(prev, None, now, secs(3)));
        assert!(!catch_up_due(prev, None, epoch(999_999), grace));
    }

    #[test]
    fn guard_retries_are_a_quarter_of_the_grace_within_bounds() {
        assert_eq!(guard_retry(secs(60)), secs(5));
        assert_eq!(guard_retry(secs(4)), secs(1));
        assert_eq!(guard_retry(Duration::ZERO), Duration::from_millis(100));
    }

    #[test]
    fn jitter_is_uniform_over_the_closed_range() {
        let max = secs(4);
        assert_eq!(jitter_from(max, 0), Duration::ZERO);
        assert_eq!(jitter_from(max, 1 << 63), secs(2));
        assert_eq!(jitter_from(max, u64::MAX), max);
        assert_eq!(jitter_from(Duration::ZERO, u64::MAX), Duration::ZERO);
        assert_eq!(
            jitter_from(Duration::MAX, u64::MAX),
            Duration::from_nanos(u64::MAX)
        );
    }

    #[test]
    fn wall_waits_are_capped_so_clock_corrections_are_noticed() {
        let now = epoch(1_000);
        assert_eq!(wall_wait(epoch(1_000), now), None);
        assert_eq!(wall_wait(epoch(999), now), None);
        assert_eq!(wall_wait(epoch(1_030), now), Some(secs(30)));
        assert_eq!(wall_wait(epoch(5_000), now), Some(WALL_POLL));
    }

    #[test]
    fn unix_millis_saturate_before_the_epoch() {
        assert_eq!(unix_millis(epoch(2)), 2_000);
        assert_eq!(unix_millis(SystemTime::UNIX_EPOCH - secs(1)), 0);
    }

    #[cfg(feature = "cron")]
    mod cron_patterns {
        use super::*;

        /// 2026-01-01T00:00:00Z, a Thursday.
        const NEW_YEAR_2026: u64 = 1_767_225_600;

        fn at(offset: u64) -> SystemTime {
            epoch(NEW_YEAR_2026 + offset)
        }

        fn grid(pattern: &str) -> WallGrid {
            WallGrid::Cron(Arc::new(parse_cron(pattern).unwrap()))
        }

        #[test]
        fn five_fields_fire_on_the_minute_and_six_take_leading_seconds() {
            let five = grid("*/15 * * * *");
            assert_eq!(five.at_or_after(at(7 * 60)), Some(at(15 * 60)));
            let six = grid("30 */15 * * * *");
            assert_eq!(six.at_or_after(at(7 * 60)), Some(at(15 * 60 + 30)));
            let seconds = grid("0 */15 * * * *");
            assert_eq!(seconds.at_or_after(at(0)), Some(at(0)), "inclusive");
            assert_eq!(seconds.next(at(0), at(0)), Some(at(15 * 60)));
        }

        #[test]
        fn years_and_malformed_patterns_are_rejected() {
            assert!(parse_cron("0 0 0 * * * 2030").is_err());
            assert!(parse_cron("61 * * * *").is_err());
            assert!(parse_cron("* * *").is_err());
            assert!(parse_cron("").is_err());
        }

        #[test]
        fn sub_second_instants_round_toward_the_right_tick() {
            let cron = parse_cron("*/15 * * * *").unwrap();
            let half = Duration::from_millis(500);
            assert_eq!(cron_after(&cron, at(15 * 60) - half), Some(at(15 * 60)));
            assert_eq!(cron_after(&cron, at(15 * 60)), Some(at(30 * 60)));
            assert_eq!(
                cron_at_or_after(&cron, at(15 * 60) + half),
                Some(at(30 * 60))
            );
            assert_eq!(cron_at_or_after(&cron, at(15 * 60)), Some(at(15 * 60)));
            assert_eq!(cron_at_or_before(&cron, at(15 * 60)), Some(at(15 * 60)));
            assert_eq!(cron_at_or_before(&cron, at(15 * 60) - half), Some(at(0)));
        }

        #[test]
        fn day_of_month_or_day_of_week_matches_either() {
            let grid = grid("0 0 13 * 5");
            let day = 24 * 3600;
            let mut ticks = Vec::new();
            let mut prev = at(0);
            for _ in 0..4 {
                prev = grid.next(prev, at(0)).unwrap();
                ticks.push(prev);
            }
            assert_eq!(
                ticks,
                vec![at(day), at(8 * day), at(12 * day), at(15 * day)]
            );
        }

        #[test]
        fn the_cron_grid_skips_ticks_already_too_late() {
            let grid = grid("*/15 * * * *");
            assert_eq!(
                grid.next(at(15 * 60), at(50 * 60)),
                Some(at(60 * 60)),
                "a floor past the previous tick starts from the floor"
            );
            assert_eq!(grid.at_or_before(at(50 * 60)), Some(at(45 * 60)));
        }

        #[test]
        fn the_cron_grid_counts_the_ticks_it_passes_over_up_to_a_cap() {
            let quarters = grid("*/15 * * * *");
            assert_eq!(quarters.after(at(15 * 60)), Some(at(30 * 60)));
            assert_eq!(quarters.between(at(15 * 60), at(30 * 60)), None);
            assert_eq!(
                quarters.between(at(0), at(60 * 60)),
                Some(PassedOver {
                    count: 3,
                    first: at(15 * 60),
                    last: at(45 * 60),
                })
            );

            let every_second = grid("* * * * * *");
            let passed = every_second.between(at(0), at(20_000)).unwrap();
            assert_eq!(
                passed.count, PASS_OVER_COUNT_CAP,
                "counting stops at the cap"
            );
            assert_eq!((passed.first, passed.last), (at(1), at(19_999)));
        }
    }
}
