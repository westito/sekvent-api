use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::PolicyError;

/// How much randomness is mixed into each backoff delay.
///
/// Jitter spreads retries of many clients over time so a recovering
/// dependency is not hit by synchronized waves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Jitter {
    /// Exactly the exponential delay.
    None,
    /// Uniform in `[0, delay]`.
    #[default]
    Full,
    /// Half the delay plus uniform in `[0, delay / 2]`.
    Equal,
}

impl Jitter {
    /// The lowercase name used in configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
            Self::Equal => "equal",
        }
    }
}

impl fmt::Display for Jitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Jitter {
    type Err = PolicyError;

    /// Parse `none`, `full` or `equal`, case-insensitively.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "full" => Ok(Self::Full),
            "equal" => Ok(Self::Equal),
            _ => Err(PolicyError::new(
                "backoff.jitter",
                "expected none, full or equal",
            )),
        }
    }
}

/// Exponential backoff: `initial * multiplier^attempt`, capped at `max`,
/// with optional [`Jitter`].
///
/// Randomness is injected: [`Backoff::delay`] and [`Backoff::iter_with_rng`]
/// take any [`rand::Rng`], so tests pass a seeded generator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    multiplier: f64,
    jitter: Jitter,
}

impl Default for Backoff {
    /// 100 ms doubling up to 5 s, with full jitter.
    fn default() -> Self {
        Self::exponential(Duration::from_millis(100), Duration::from_secs(5))
    }
}

impl Backoff {
    /// Doubling delays from `initial` up to `max`, with full jitter. A `max`
    /// below `initial` is raised to `initial`.
    pub fn exponential(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max: max.max(initial),
            multiplier: 2.0,
            jitter: Jitter::Full,
        }
    }

    /// The same delay every time, without jitter.
    pub fn constant(delay: Duration) -> Self {
        Self {
            initial: delay,
            max: delay,
            multiplier: 1.0,
            jitter: Jitter::None,
        }
    }

    /// Fully specified backoff. The multiplier must be finite and at least
    /// one, and `max` must not be below `initial`.
    pub fn new(
        initial: Duration,
        max: Duration,
        multiplier: f64,
        jitter: Jitter,
    ) -> Result<Self, PolicyError> {
        if !multiplier.is_finite() || multiplier < 1.0 {
            return Err(PolicyError::new(
                "backoff.multiplier",
                "must be a finite number of at least 1",
            ));
        }
        if max < initial {
            return Err(PolicyError::new(
                "backoff.max",
                "must not be below backoff.initial",
            ));
        }
        Ok(Self {
            initial,
            max,
            multiplier,
            jitter,
        })
    }

    /// Replace the jitter mode.
    #[must_use]
    pub fn with_jitter(mut self, jitter: Jitter) -> Self {
        self.jitter = jitter;
        self
    }

    /// The first delay.
    pub fn initial(&self) -> Duration {
        self.initial
    }
    /// The delay cap.
    pub fn max(&self) -> Duration {
        self.max
    }
    /// The growth factor per attempt.
    pub fn multiplier(&self) -> f64 {
        self.multiplier
    }
    /// The jitter mode.
    pub fn jitter(&self) -> Jitter {
        self.jitter
    }

    /// The delay before retry number `attempt` (zero-based) without jitter.
    pub fn base_delay(&self, attempt: u32) -> Duration {
        let mut delay = self.initial.min(self.max);
        for _ in 0..attempt {
            if delay >= self.max {
                break;
            }
            let next = Duration::try_from_secs_f64(delay.as_secs_f64() * self.multiplier)
                .map_or(self.max, |grown| grown.min(self.max));
            if next <= delay {
                break;
            }
            delay = next;
        }
        delay
    }

    /// The delay before retry number `attempt` (zero-based), jittered with `rng`.
    pub fn delay<R: Rng + ?Sized>(&self, attempt: u32, rng: &mut R) -> Duration {
        let base = self.base_delay(attempt);
        match self.jitter {
            Jitter::None => base,
            Jitter::Full => uniform_up_to(base, rng),
            Jitter::Equal => {
                let half = base / 2;
                half + uniform_up_to(base.saturating_sub(half), rng)
            }
        }
    }

    /// An endless iterator of delays, jittered by a generator seeded from
    /// the thread-local entropy source.
    pub fn iter(&self) -> BackoffIter<SmallRng> {
        self.iter_with_rng(SmallRng::from_rng(&mut rand::rng()))
    }

    /// An endless iterator of delays, jittered by `rng`.
    pub fn iter_with_rng<R: Rng>(&self, rng: R) -> BackoffIter<R> {
        BackoffIter {
            backoff: *self,
            attempt: 0,
            rng,
        }
    }
}

/// Uniform in `[0, upper]` at nanosecond resolution (saturating at
/// `u64::MAX` nanoseconds, about 584 years).
fn uniform_up_to<R: Rng + ?Sized>(upper: Duration, rng: &mut R) -> Duration {
    let nanos = u64::try_from(upper.as_nanos()).unwrap_or(u64::MAX);
    if nanos == 0 {
        return Duration::ZERO;
    }
    let span = u128::from(nanos) + 1;
    let pick = (u128::from(rng.next_u64()) * span) >> 64;
    Duration::from_nanos(u64::try_from(pick).unwrap_or(nanos))
}

/// Endless iterator over [`Backoff`] delays. Combine with `take(n)`.
#[derive(Debug, Clone)]
pub struct BackoffIter<R> {
    backoff: Backoff,
    attempt: u32,
    rng: R,
}

impl IntoIterator for &Backoff {
    type Item = Duration;
    type IntoIter = BackoffIter<SmallRng>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<R: Rng> Iterator for BackoffIter<R> {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        let delay = self.backoff.delay(self.attempt, &mut self.rng);
        self.attempt = self.attempt.saturating_add(1);
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use rand::TryRng;

    use super::*;

    /// Always returns the same word.
    struct FixedRng(u64);

    impl TryRng for FixedRng {
        type Error = Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Infallible> {
            Ok(u32::try_from(self.0 >> 32).unwrap_or(u32::MAX))
        }
        fn try_next_u64(&mut self) -> Result<u64, Infallible> {
            Ok(self.0)
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
            dst.fill(0);
            Ok(())
        }
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn base_delays_grow_and_cap() {
        let backoff = Backoff::exponential(ms(100), ms(1000)).with_jitter(Jitter::None);
        let delays: Vec<_> = backoff.iter_with_rng(FixedRng(0)).take(6).collect();
        assert_eq!(
            delays,
            vec![ms(100), ms(200), ms(400), ms(800), ms(1000), ms(1000)]
        );
        assert_eq!(backoff.base_delay(u32::MAX), ms(1000));
    }

    #[test]
    fn exponential_raises_max_to_initial() {
        let backoff = Backoff::exponential(ms(500), ms(100));
        assert_eq!(backoff.max(), ms(500));
        assert_eq!(backoff.initial(), ms(500));
        assert_eq!(backoff.jitter(), Jitter::Full);
        assert!((backoff.multiplier() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn constant_never_grows() {
        let backoff = Backoff::constant(ms(50));
        let mut rng = FixedRng(u64::MAX);
        assert_eq!(backoff.delay(0, &mut rng), ms(50));
        assert_eq!(backoff.delay(1000, &mut rng), ms(50));
    }

    #[test]
    fn huge_growth_saturates_at_max() {
        let backoff =
            Backoff::new(Duration::from_secs(1), Duration::MAX, 1e300, Jitter::None).unwrap();
        assert_eq!(backoff.base_delay(1), Duration::MAX);
    }

    #[test]
    fn full_jitter_stays_within_bounds() {
        let backoff = Backoff::exponential(ms(100), ms(1000));
        assert_eq!(backoff.delay(0, &mut FixedRng(0)), Duration::ZERO);
        assert_eq!(backoff.delay(0, &mut FixedRng(u64::MAX)), ms(100));
        let mut rng = SmallRng::seed_from_u64(7);
        for attempt in 0..20 {
            let delay = backoff.delay(attempt, &mut rng);
            assert!(delay <= backoff.base_delay(attempt));
        }
    }

    #[test]
    fn equal_jitter_keeps_half() {
        let backoff = Backoff::exponential(ms(100), ms(1000)).with_jitter(Jitter::Equal);
        assert_eq!(backoff.delay(0, &mut FixedRng(0)), ms(50));
        assert_eq!(backoff.delay(0, &mut FixedRng(u64::MAX)), ms(100));
    }

    #[test]
    fn zero_delay_has_no_jitter() {
        let backoff = Backoff::exponential(Duration::ZERO, Duration::ZERO);
        assert_eq!(backoff.delay(3, &mut FixedRng(u64::MAX)), Duration::ZERO);
    }

    #[test]
    fn seeded_iterators_are_reproducible() {
        let backoff = Backoff::default();
        let first: Vec<_> = backoff
            .iter_with_rng(SmallRng::seed_from_u64(42))
            .take(8)
            .collect();
        let second: Vec<_> = backoff
            .iter_with_rng(SmallRng::seed_from_u64(42))
            .take(8)
            .collect();
        assert_eq!(first, second);
        assert_eq!(backoff.iter().take(3).count(), 3);
    }

    #[test]
    fn new_validates() {
        assert_eq!(
            Backoff::new(ms(1), ms(2), 0.5, Jitter::None)
                .unwrap_err()
                .parameter(),
            "backoff.multiplier"
        );
        assert!(Backoff::new(ms(1), ms(2), f64::NAN, Jitter::None).is_err());
        assert_eq!(
            Backoff::new(ms(3), ms(2), 2.0, Jitter::None)
                .unwrap_err()
                .parameter(),
            "backoff.max"
        );
        let ok = Backoff::new(ms(1), ms(2), 1.5, Jitter::Equal).unwrap();
        assert_eq!(ok.jitter(), Jitter::Equal);
    }

    #[test]
    fn jitter_names_round_trip() {
        for jitter in [Jitter::None, Jitter::Full, Jitter::Equal] {
            assert_eq!(jitter.to_string().parse::<Jitter>().unwrap(), jitter);
        }
        assert_eq!(" FULL ".parse::<Jitter>().unwrap(), Jitter::Full);
        assert!("sometimes".parse::<Jitter>().is_err());
        assert_eq!(Jitter::default(), Jitter::Full);
        let mut rng = FixedRng(5);
        assert_eq!(rng.try_next_u32(), Ok(0));
        let mut bytes = [1_u8; 2];
        rng.try_fill_bytes(&mut bytes).unwrap();
        assert_eq!(bytes, [0, 0]);
    }
}
