use std::fmt;
use std::time::Duration;

/// When a unit starts and stops relative to the others.
///
/// Stages start in declaration order, each only once the previous one is
/// fully started, and drain in the reverse order: ingress stops taking
/// traffic before the workers and components it feeds, and those stop before
/// the infrastructure they use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Stage {
    /// Connections, pools, caches and dependency probes.
    Infrastructure,
    /// Application components built on the infrastructure.
    Components,
    /// Background jobs, consumers and schedulers.
    Workers,
    /// Listeners that accept outside traffic.
    Ingress,
}

impl Stage {
    /// Every stage, in start order.
    pub const ALL: [Stage; 4] = [
        Self::Infrastructure,
        Self::Components,
        Self::Workers,
        Self::Ingress,
    ];

    /// Lowercase name, as used in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Infrastructure => "infrastructure",
            Self::Components => "components",
            Self::Workers => "workers",
            Self::Ingress => "ingress",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the supervisor does when a unit's future completes on its own,
/// before its stage was asked to drain.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub enum UnitPolicy {
    /// Any exit, successful or not, shuts the whole runtime down. A failed
    /// exit also makes [`Runtime::run`](crate::Runtime::run) return the error.
    #[default]
    Critical,
    /// Run the unit again after a backoff. Once the restarts are exhausted
    /// the next exit is treated as a critical failure.
    ///
    /// An exit before the unit reported ready counts as a restart too, and
    /// the unit keeps holding up its stage's start until one of its runs
    /// reports ready.
    Restart(RestartPolicy),
    /// Log the exit and carry on without the unit.
    BestEffort,
}

/// Exponential backoff between restarts of a [`UnitPolicy::Restart`] unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RestartPolicy {
    /// Delay before the first restart.
    pub initial: Duration,
    /// Upper bound for any delay.
    pub max: Duration,
    /// Factor applied to the delay after each restart; at least `1.0`.
    pub multiplier: f64,
    /// Consecutive restarts allowed before escalating; `None` restarts
    /// forever. A run that lasts
    /// [`restart_reset_after`](crate::RuntimeBuilder::restart_reset_after)
    /// resets the count and the backoff.
    pub max_restarts: Option<u32>,
}

impl Default for RestartPolicy {
    /// 100 ms doubling up to 30 s, restarting forever.
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(30),
            multiplier: 2.0,
            max_restarts: None,
        }
    }
}

impl RestartPolicy {
    /// The delay before restart number `restart + 1` (so `delay(0)` is
    /// [`initial`](Self::initial)), capped at [`max`](Self::max).
    pub fn delay(&self, restart: u32) -> Duration {
        let exponent = i32::try_from(restart).unwrap_or(i32::MAX);
        let seconds = self.initial.as_secs_f64() * self.multiplier.powi(exponent);
        Duration::try_from_secs_f64(seconds).map_or(self.max, |delay| delay.min(self.max))
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if !self.multiplier.is_finite() || self.multiplier < 1.0 {
            return Err("the restart multiplier must be a finite number of at least 1.0");
        }
        if self.initial > self.max {
            return Err("the initial restart delay must not exceed the maximum delay");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_are_ordered_and_named() {
        assert!(Stage::ALL.windows(2).all(|pair| pair[0] < pair[1]));
        let names: Vec<String> = Stage::ALL.iter().map(ToString::to_string).collect();
        assert_eq!(
            names,
            ["infrastructure", "components", "workers", "ingress"]
        );
        assert_eq!(UnitPolicy::default(), UnitPolicy::Critical);
    }

    #[test]
    fn delays_grow_and_cap() {
        let policy = RestartPolicy {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(5),
            multiplier: 2.0,
            max_restarts: None,
        };
        let delays: Vec<u64> = (0..5).map(|n| policy.delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 5, 5]);
        assert_eq!(policy.delay(u32::MAX), Duration::from_secs(5));
    }

    #[test]
    fn a_constant_policy_never_grows() {
        let policy = RestartPolicy {
            multiplier: 1.0,
            ..RestartPolicy::default()
        };
        assert_eq!(policy.delay(10), Duration::from_millis(100));
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn invalid_policies_are_rejected() {
        let shrinking = RestartPolicy {
            multiplier: 0.5,
            ..RestartPolicy::default()
        };
        assert!(shrinking.validate().is_err());
        let nan = RestartPolicy {
            multiplier: f64::NAN,
            ..RestartPolicy::default()
        };
        assert!(nan.validate().is_err());
        let inverted = RestartPolicy {
            initial: Duration::from_secs(10),
            max: Duration::from_secs(1),
            ..RestartPolicy::default()
        };
        assert!(inverted.validate().is_err());
    }
}
