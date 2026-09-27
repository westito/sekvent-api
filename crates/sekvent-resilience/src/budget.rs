use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::PolicyError;

/// Fixed-point scale: one retry token is this many units.
const UNIT: u64 = 1000;

/// A retry budget: a token bucket that keeps retries a bounded fraction of
/// successful traffic.
///
/// Each success deposits `ratio` tokens (up to `max_tokens`); each retry
/// withdraws one. Independently, `min_per_sec` retries per second are always
/// allowed so a low-traffic caller can still retry. Accounting is integer
/// fixed-point (thousandths of a token), so it never drifts.
///
/// Shared by every call that uses the same [`RetryPolicy`](crate::RetryPolicy).
#[derive(Debug)]
pub struct RetryBudget {
    deposit: u64,
    capacity: u64,
    min_per_sec: u32,
    state: Mutex<BudgetState>,
}

#[derive(Debug)]
struct BudgetState {
    balance: u64,
    floor_window: Option<Instant>,
    floor_used: u32,
}

impl Default for RetryBudget {
    /// Ratio 0.2, ten retries per second floor, at most 100 banked tokens.
    fn default() -> Self {
        Self::from_parts(200, 10, 100 * UNIT)
    }
}

impl RetryBudget {
    /// A budget depositing `ratio` tokens per success (`0.0..=1000.0`,
    /// resolved to thousandths) with a floor of `min_per_sec` retries per
    /// second. At most 100 tokens are banked; see [`RetryBudget::with_max_tokens`].
    pub fn new(ratio: f64, min_per_sec: u32) -> Result<Self, PolicyError> {
        Ok(Self::from_parts(
            ratio_to_units(ratio)?,
            min_per_sec,
            100 * UNIT,
        ))
    }

    fn from_parts(deposit: u64, min_per_sec: u32, capacity: u64) -> Self {
        Self {
            deposit,
            capacity,
            min_per_sec,
            state: Mutex::new(BudgetState {
                balance: 0,
                floor_window: None,
                floor_used: 0,
            }),
        }
    }

    /// Cap the banked balance at `tokens` whole tokens, so a long healthy
    /// period cannot fund an unbounded retry storm.
    #[must_use]
    pub fn with_max_tokens(mut self, tokens: u32) -> Self {
        self.capacity = u64::from(tokens) * UNIT;
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        state.balance = state.balance.min(self.capacity);
        self
    }

    /// Record a successful call.
    pub fn deposit(&self) {
        let mut state = self.lock();
        state.balance = state
            .balance
            .saturating_add(self.deposit)
            .min(self.capacity);
    }

    /// Try to take one retry. Uses the per-second floor first, then the
    /// banked balance. Returns `false` when the budget is exhausted.
    pub fn try_withdraw(&self) -> bool {
        let now = Instant::now();
        let mut state = self.lock();
        if self.min_per_sec > 0 {
            let fresh = state
                .floor_window
                .is_none_or(|start| now.saturating_duration_since(start) >= Duration::from_secs(1));
            if fresh {
                state.floor_window = Some(now);
                state.floor_used = 0;
            }
            if state.floor_used < self.min_per_sec {
                state.floor_used += 1;
                return true;
            }
        }
        if state.balance >= UNIT {
            state.balance -= UNIT;
            return true;
        }
        false
    }

    /// Whole retry tokens currently banked (the per-second floor not included).
    pub fn available(&self) -> u64 {
        self.lock().balance / UNIT
    }

    /// The per-second floor.
    pub fn min_per_sec(&self) -> u32 {
        self.min_per_sec
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BudgetState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Convert a ratio to thousandths of a token, rejecting nonsense.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn ratio_to_units(ratio: f64) -> Result<u64, PolicyError> {
    if !ratio.is_finite() || !(0.0..=1000.0).contains(&ratio) {
        return Err(PolicyError::new(
            "retry_budget.ratio",
            "must be a finite number between 0 and 1000",
        ));
    }
    // In range 0..=1_000_000 after the check above, so the cast is exact.
    Ok((ratio * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn budget_exhausts_and_refills_from_successes() {
        let budget = RetryBudget::new(0.5, 0).unwrap();
        assert!(!budget.try_withdraw(), "an empty budget allows no retry");
        budget.deposit();
        assert!(!budget.try_withdraw(), "half a token is not enough");
        budget.deposit();
        assert_eq!(budget.available(), 1);
        assert!(budget.try_withdraw());
        assert!(!budget.try_withdraw());
        for _ in 0..4 {
            budget.deposit();
        }
        assert!(budget.try_withdraw());
        assert!(budget.try_withdraw());
        assert!(!budget.try_withdraw());
    }

    #[tokio::test(start_paused = true)]
    async fn floor_allows_a_few_retries_per_second() {
        let budget = RetryBudget::new(0.0, 2).unwrap();
        assert_eq!(budget.min_per_sec(), 2);
        assert!(budget.try_withdraw());
        assert!(budget.try_withdraw());
        assert!(!budget.try_withdraw());
        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(!budget.try_withdraw());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(budget.try_withdraw());
        assert!(budget.try_withdraw());
        assert!(!budget.try_withdraw());
    }

    #[tokio::test(start_paused = true)]
    async fn floor_is_used_before_balance() {
        let budget = RetryBudget::new(1.0, 1).unwrap();
        budget.deposit();
        assert!(budget.try_withdraw());
        assert_eq!(budget.available(), 1, "the floor paid for the first retry");
        assert!(budget.try_withdraw());
        assert_eq!(budget.available(), 0);
        assert!(!budget.try_withdraw());
    }

    #[test]
    fn balance_is_capped() {
        let budget = RetryBudget::new(1.0, 0).unwrap().with_max_tokens(3);
        for _ in 0..10 {
            budget.deposit();
        }
        assert_eq!(budget.available(), 3);
        let budget = RetryBudget::default();
        for _ in 0..10_000 {
            budget.deposit();
        }
        assert_eq!(budget.available(), 100);
    }

    #[test]
    fn fixed_point_does_not_drift() {
        let budget = RetryBudget::new(0.1, 0).unwrap();
        for _ in 0..10 {
            budget.deposit();
        }
        assert_eq!(
            budget.available(),
            1,
            "ten deposits of 0.1 make exactly one token"
        );
    }

    #[test]
    fn ratio_is_validated() {
        assert!(RetryBudget::new(-0.1, 0).is_err());
        assert!(RetryBudget::new(f64::INFINITY, 0).is_err());
        assert!(RetryBudget::new(1001.0, 0).is_err());
        assert_eq!(ratio_to_units(0.2).unwrap(), 200);
        assert_eq!(ratio_to_units(0.0012).unwrap(), 1);
    }
}
