use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use sekvent_context::Clock;
use sekvent_error::AppError;

use crate::PolicyError;

/// A time source that never goes backwards, for measuring intervals.
pub trait MonotonicClock: Send + Sync + 'static {
    /// Time elapsed since an arbitrary, fixed origin.
    fn elapsed(&self) -> Duration;
}

/// Monotonic time from tokio's clock; follows `tokio::time::pause`.
#[derive(Debug, Clone, Copy)]
pub struct TokioClock {
    origin: tokio::time::Instant,
}

impl TokioClock {
    /// A clock whose origin is now.
    pub fn new() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
        }
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for TokioClock {
    fn elapsed(&self) -> Duration {
        tokio::time::Instant::now().saturating_duration_since(self.origin)
    }
}

/// Adapts a wall [`Clock`] (such as `ManualClock`) to [`MonotonicClock`],
/// measuring from the Unix epoch. Backward jumps read as no time passing.
#[derive(Debug, Clone)]
pub struct WallClock<C>(C);

impl<C: Clock> WallClock<C> {
    /// Wrap `clock`.
    pub fn new(clock: C) -> Self {
        Self(clock)
    }
}

impl<C: Clock> MonotonicClock for WallClock<C> {
    fn elapsed(&self) -> Duration {
        self.0
            .now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
    }
}

/// Decides whether an error counts as a failure of the dependency.
pub type FailureClassifier = Arc<dyn Fn(&AppError) -> bool + Send + Sync>;

type StateHook = Arc<dyn Fn(&StateTransition) + Send + Sync>;

/// The state of a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakerState {
    /// Calls flow; outcomes are recorded.
    Closed,
    /// Calls are rejected until the open period ends.
    Open,
    /// A limited number of probe calls decide between closing and reopening.
    HalfOpen,
}

impl BreakerState {
    /// Lowercase name for logs and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

impl fmt::Display for BreakerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A state change, passed to the `on_state_change` hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTransition {
    /// The breaker's name.
    pub name: String,
    /// The previous state.
    pub from: BreakerState,
    /// The new state.
    pub to: BreakerState,
}

/// The sliding window over which the failure rate is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BreakerWindow {
    /// The last `size` calls.
    Count {
        /// Number of calls remembered.
        size: u32,
    },
    /// The calls of the last `duration` (tracked in ten buckets).
    Time {
        /// Window length.
        duration: Duration,
    },
}

/// Parameters of a [`CircuitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct CircuitBreakerConfig {
    /// The sliding window.
    pub window: BreakerWindow,
    /// Failure fraction (`0 < rate <= 1`) at or above which the circuit opens.
    pub failure_rate: f64,
    /// Calls the window must hold before the rate is evaluated.
    pub min_calls: u32,
    /// How long the circuit stays open before probing.
    pub wait_in_open: Duration,
    /// Probe calls allowed while half-open; all must succeed to close.
    pub permitted_in_half_open: u32,
}

impl Default for CircuitBreakerConfig {
    /// Last 20 calls, 50 %, at least 10 calls, 30 s open, 3 probes.
    fn default() -> Self {
        Self {
            window: BreakerWindow::Count { size: 20 },
            failure_rate: 0.5,
            min_calls: 10,
            wait_in_open: Duration::from_secs(30),
            permitted_in_half_open: 3,
        }
    }
}

/// Snapshot of a breaker's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BreakerMetrics {
    /// Current state.
    pub state: BreakerState,
    /// Calls in the current window (closed state).
    pub calls: u32,
    /// Failures in the current window (closed state).
    pub failures: u32,
    /// Failures per thousand calls in the current window.
    pub failure_rate_permille: u32,
    /// Calls rejected since creation.
    pub rejected: u64,
}

/// A circuit breaker: after too many failures it rejects calls for a while
/// instead of piling load onto an unhealthy dependency.
///
/// - **Closed**: calls flow and outcomes enter a sliding window. Once the
///   window holds at least `min_calls` and the failure fraction reaches
///   `failure_rate`, the circuit opens.
/// - **Open**: calls fail fast with `UNAVAILABLE` and a `retry_after` equal
///   to the remaining open time. After `wait_in_open` it becomes half-open.
/// - **Half-open**: `permitted_in_half_open` probe calls run; one failure
///   reopens the circuit, all of them succeeding closes it.
///
/// Only errors for which the classifier returns `true` are failures; by
/// default that is [`ErrorCode::trips_breaker`](sekvent_error::ErrorCode::trips_breaker).
/// Other errors mean the dependency answered, so they count as successes.
pub struct CircuitBreaker {
    name: String,
    window_kind: BreakerWindow,
    threshold_permille: u32,
    min_calls: u32,
    wait_in_open: Duration,
    permitted_in_half_open: u32,
    clock: Arc<dyn MonotonicClock>,
    classifier: FailureClassifier,
    hook: Option<StateHook>,
    rejected: AtomicU64,
    inner: Mutex<Inner>,
}

impl fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("name", &self.name)
            .field("window", &self.window_kind)
            .field("threshold_permille", &self.threshold_permille)
            .field("min_calls", &self.min_calls)
            .field("wait_in_open", &self.wait_in_open)
            .field("permitted_in_half_open", &self.permitted_in_half_open)
            .finish_non_exhaustive()
    }
}

struct Inner {
    state: State,
    generation: u64,
    window: Window,
}

#[derive(Clone, Copy)]
enum State {
    Closed,
    Open { until: Duration },
    HalfOpen { issued: u32, succeeded: u32 },
}

impl State {
    fn public(self) -> BreakerState {
        match self {
            Self::Closed => BreakerState::Closed,
            Self::Open { .. } => BreakerState::Open,
            Self::HalfOpen { .. } => BreakerState::HalfOpen,
        }
    }
}

const TIME_BUCKETS: u64 = 10;
const TIME_BUCKET_SLOTS: usize = 10;

enum Window {
    Count {
        size: usize,
        outcomes: VecDeque<bool>,
        failures: u32,
    },
    Time {
        width_nanos: u128,
        buckets: Vec<Bucket>,
    },
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    index: u64,
    calls: u32,
    failures: u32,
}

impl Window {
    fn new(kind: BreakerWindow) -> Self {
        match kind {
            BreakerWindow::Count { size } => {
                let size = usize::try_from(size).unwrap_or(usize::MAX);
                Self::Count {
                    size,
                    outcomes: VecDeque::with_capacity(size.min(4096)),
                    failures: 0,
                }
            }
            BreakerWindow::Time { duration } => Self::Time {
                width_nanos: (duration.as_nanos() / u128::from(TIME_BUCKETS)).max(1),
                buckets: vec![Bucket::default(); TIME_BUCKET_SLOTS],
            },
        }
    }

    fn bucket_index(width_nanos: u128, now: Duration) -> u64 {
        u64::try_from(now.as_nanos() / width_nanos).unwrap_or(u64::MAX)
    }

    fn record(&mut self, now: Duration, failed: bool) {
        match self {
            Self::Count {
                size,
                outcomes,
                failures,
            } => {
                outcomes.push_back(failed);
                if failed {
                    *failures += 1;
                }
                while outcomes.len() > *size {
                    if outcomes.pop_front() == Some(true) {
                        *failures -= 1;
                    }
                }
            }
            Self::Time {
                width_nanos,
                buckets,
            } => {
                let index = Self::bucket_index(*width_nanos, now);
                let slot = usize::try_from(index % TIME_BUCKETS).unwrap_or(0);
                let bucket = &mut buckets[slot];
                if bucket.index != index {
                    *bucket = Bucket {
                        index,
                        calls: 0,
                        failures: 0,
                    };
                }
                bucket.calls = bucket.calls.saturating_add(1);
                if failed {
                    bucket.failures = bucket.failures.saturating_add(1);
                }
            }
        }
    }

    /// `(calls, failures)` currently in the window.
    fn totals(&self, now: Duration) -> (u32, u32) {
        match self {
            Self::Count {
                outcomes, failures, ..
            } => (u32::try_from(outcomes.len()).unwrap_or(u32::MAX), *failures),
            Self::Time {
                width_nanos,
                buckets,
            } => {
                let current = Self::bucket_index(*width_nanos, now);
                buckets
                    .iter()
                    .filter(|bucket| {
                        bucket.index <= current
                            && bucket.index.saturating_add(TIME_BUCKETS) > current
                    })
                    .fold((0_u32, 0_u32), |(calls, failures), bucket| {
                        (
                            calls.saturating_add(bucket.calls),
                            failures.saturating_add(bucket.failures),
                        )
                    })
            }
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Count {
                outcomes, failures, ..
            } => {
                outcomes.clear();
                *failures = 0;
            }
            Self::Time { buckets, .. } => buckets.fill(Bucket::default()),
        }
    }
}

fn permille(failures: u32, calls: u32) -> u32 {
    if calls == 0 {
        return 0;
    }
    u32::try_from(u64::from(failures) * 1000 / u64::from(calls)).unwrap_or(1000)
}

/// Permission to make one call through a [`CircuitBreaker`].
///
/// Report the outcome with [`BreakerPermit::record`] (or the explicit
/// variants). Dropping it unreported — for example when the call is
/// cancelled — records nothing and frees a half-open probe slot.
#[must_use = "report the call outcome through the permit"]
pub struct BreakerPermit<'a> {
    breaker: &'a CircuitBreaker,
    generation: u64,
    probe: bool,
    reported: bool,
}

impl fmt::Debug for BreakerPermit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BreakerPermit")
            .field("breaker", &self.breaker.name)
            .field("probe", &self.probe)
            .finish_non_exhaustive()
    }
}

impl BreakerPermit<'_> {
    /// Report the call's result, classifying errors with the breaker's classifier.
    pub fn record<T>(self, result: &Result<T, AppError>) {
        let failed = result
            .as_ref()
            .err()
            .is_some_and(|error| (self.breaker.classifier)(error));
        self.finish(failed);
    }

    /// Report a success.
    pub fn record_success(self) {
        self.finish(false);
    }

    /// Report a failure of the dependency.
    pub fn record_failure(self) {
        self.finish(true);
    }

    fn finish(mut self, failed: bool) {
        self.reported = true;
        self.breaker.on_outcome(self.generation, failed);
    }
}

impl Drop for BreakerPermit<'_> {
    fn drop(&mut self) {
        if !self.reported && self.probe {
            self.breaker.release_probe(self.generation);
        }
    }
}

impl CircuitBreaker {
    /// A breaker named `name` (used in logs and transitions).
    pub fn new(name: impl Into<String>, config: CircuitBreakerConfig) -> Result<Self, PolicyError> {
        let threshold_permille = rate_to_permille(config.failure_rate)?;
        match config.window {
            BreakerWindow::Count { size: 0 } => {
                return Err(PolicyError::new(
                    "breaker.window",
                    "must hold at least one call",
                ));
            }
            BreakerWindow::Time { duration } if duration.is_zero() => {
                return Err(PolicyError::new(
                    "breaker.window",
                    "must be longer than zero",
                ));
            }
            _ => {}
        }
        if config.min_calls == 0 {
            return Err(PolicyError::new("breaker.min_calls", "must be at least 1"));
        }
        if config.permitted_in_half_open == 0 {
            return Err(PolicyError::new(
                "breaker.permitted_in_half_open",
                "must be at least 1",
            ));
        }
        Ok(Self {
            name: name.into(),
            window_kind: config.window,
            threshold_permille,
            min_calls: config.min_calls,
            wait_in_open: config.wait_in_open,
            permitted_in_half_open: config.permitted_in_half_open,
            clock: Arc::new(TokioClock::new()),
            classifier: Arc::new(|error: &AppError| error.code().trips_breaker()),
            hook: None,
            rejected: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                state: State::Closed,
                generation: 0,
                window: Window::new(config.window),
            }),
        })
    }

    /// Measure time with `clock` instead of tokio's clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn MonotonicClock>) -> Self {
        self.clock = clock;
        self
    }

    /// Replace the failure classifier.
    #[must_use]
    pub fn with_classifier(
        mut self,
        classifier: impl Fn(&AppError) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.classifier = Arc::new(classifier);
        self
    }

    /// Call `hook` after every state change (outside the breaker's lock).
    #[must_use]
    pub fn on_state_change(
        mut self,
        hook: impl Fn(&StateTransition) + Send + Sync + 'static,
    ) -> Self {
        self.hook = Some(Arc::new(hook));
        self
    }

    /// The breaker's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current state (an elapsed open period reads as half-open).
    pub fn state(&self) -> BreakerState {
        let now = self.clock.elapsed();
        let mut inner = self.lock();
        let transition = self.expire_open(&mut inner, now);
        let state = inner.state.public();
        drop(inner);
        self.announce(transition.as_ref());
        state
    }

    /// Counters for dashboards and logs.
    pub fn metrics(&self) -> BreakerMetrics {
        let now = self.clock.elapsed();
        let mut inner = self.lock();
        let transition = self.expire_open(&mut inner, now);
        let (calls, failures) = inner.window.totals(now);
        let state = inner.state.public();
        drop(inner);
        self.announce(transition.as_ref());
        BreakerMetrics {
            state,
            calls,
            failures,
            failure_rate_permille: permille(failures, calls),
            rejected: self.rejected.load(Ordering::Relaxed),
        }
    }

    /// Ask to make a call. Rejected with `UNAVAILABLE` while open, or while
    /// half-open with every probe slot taken.
    pub fn acquire(&self) -> Result<BreakerPermit<'_>, AppError> {
        let now = self.clock.elapsed();
        let mut inner = self.lock();
        let transition = self.expire_open(&mut inner, now);
        let state = inner.state;
        let outcome = match state {
            State::Closed => Ok(false),
            State::Open { until } => Err(Some(until.saturating_sub(now))),
            State::HalfOpen { issued, succeeded } if issued < self.permitted_in_half_open => {
                inner.state = State::HalfOpen {
                    issued: issued + 1,
                    succeeded,
                };
                Ok(true)
            }
            State::HalfOpen { .. } => Err(None),
        };
        let generation = inner.generation;
        drop(inner);
        self.announce(transition.as_ref());
        match outcome {
            Ok(probe) => Ok(BreakerPermit {
                breaker: self,
                generation,
                probe,
                reported: false,
            }),
            Err(retry_after) => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                let error = AppError::unavailable("the circuit breaker is open")
                    .with_reason("CIRCUIT_OPEN")
                    .with_metadata("breaker", self.name.clone());
                Err(match retry_after {
                    Some(after) => error.with_retry_after(after),
                    None => error,
                })
            }
        }
    }

    /// Run `fut` through the breaker and record its outcome.
    pub async fn call<Fut, T>(&self, fut: Fut) -> Result<T, AppError>
    where
        Fut: Future<Output = Result<T, AppError>>,
    {
        let permit = self.acquire()?;
        let result = fut.await;
        permit.record(&result);
        result
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn expire_open(&self, inner: &mut Inner, now: Duration) -> Option<StateTransition> {
        let state = inner.state;
        match state {
            State::Open { until } if now >= until => Some(self.transition(
                inner,
                State::HalfOpen {
                    issued: 0,
                    succeeded: 0,
                },
            )),
            _ => None,
        }
    }

    fn transition(&self, inner: &mut Inner, to: State) -> StateTransition {
        let from = inner.state.public();
        inner.state = to;
        inner.generation = inner.generation.wrapping_add(1);
        if matches!(to, State::Closed) {
            inner.window.reset();
        }
        StateTransition {
            name: self.name.clone(),
            from,
            to: to.public(),
        }
    }

    fn announce(&self, transition: Option<&StateTransition>) {
        let Some(transition) = transition else {
            return;
        };
        tracing::info!(
            breaker = %transition.name,
            from = %transition.from,
            to = %transition.to,
            "circuit breaker state changed"
        );
        if let Some(hook) = &self.hook {
            hook(transition);
        }
    }

    fn on_outcome(&self, generation: u64, failed: bool) {
        let now = self.clock.elapsed();
        let mut inner = self.lock();
        if inner.generation != generation {
            return;
        }
        let state = inner.state;
        let transition = match state {
            State::Closed => {
                inner.window.record(now, failed);
                let (calls, failures) = inner.window.totals(now);
                (calls >= self.min_calls && permille(failures, calls) >= self.threshold_permille)
                    .then(|| self.open(&mut inner, now))
            }
            State::HalfOpen { .. } if failed => Some(self.open(&mut inner, now)),
            State::HalfOpen { issued, succeeded } => {
                let succeeded = succeeded + 1;
                if succeeded >= self.permitted_in_half_open {
                    Some(self.transition(&mut inner, State::Closed))
                } else {
                    inner.state = State::HalfOpen { issued, succeeded };
                    None
                }
            }
            State::Open { .. } => None,
        };
        drop(inner);
        self.announce(transition.as_ref());
    }

    fn open(&self, inner: &mut Inner, now: Duration) -> StateTransition {
        let until = now.saturating_add(self.wait_in_open);
        self.transition(inner, State::Open { until })
    }

    fn release_probe(&self, generation: u64) {
        let mut inner = self.lock();
        if inner.generation != generation {
            return;
        }
        if let State::HalfOpen { issued, succeeded } = inner.state {
            inner.state = State::HalfOpen {
                issued: issued.saturating_sub(1),
                succeeded,
            };
        }
    }
}

/// Convert a failure fraction to a per-thousand threshold.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn rate_to_permille(rate: f64) -> Result<u32, PolicyError> {
    if !rate.is_finite() || rate <= 0.0 || rate > 1.0 {
        return Err(PolicyError::new(
            "breaker.failure_rate",
            "must be a fraction above 0 and at most 1",
        ));
    }
    // In range 0..=1000 after the check above, so the cast is exact.
    Ok(((rate * 1000.0).round() as u32).max(1))
}

#[cfg(test)]
mod tests {
    use sekvent_context::ManualClock;
    use sekvent_error::ErrorCode;

    use super::*;

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    struct Harness {
        clock: ManualClock,
        transitions: Arc<Mutex<Vec<(BreakerState, BreakerState)>>>,
        breaker: CircuitBreaker,
    }

    fn harness(config: CircuitBreakerConfig) -> Harness {
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH + secs(1_000_000));
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&transitions);
        let breaker = CircuitBreaker::new("orders", config)
            .unwrap()
            .with_clock(Arc::new(WallClock::new(clock.clone())))
            .on_state_change(move |t| seen.lock().unwrap().push((t.from, t.to)));
        Harness {
            clock,
            transitions,
            breaker,
        }
    }

    fn config() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            window: BreakerWindow::Count { size: 10 },
            failure_rate: 0.5,
            min_calls: 4,
            wait_in_open: secs(30),
            permitted_in_half_open: 2,
        }
    }

    fn fail(breaker: &CircuitBreaker) {
        breaker
            .acquire()
            .unwrap()
            .record::<()>(&Err(AppError::unavailable("down")));
    }

    fn succeed(breaker: &CircuitBreaker) {
        breaker.acquire().unwrap().record(&Ok(()));
    }

    #[test]
    fn full_cycle_closed_open_half_open_closed() {
        let h = harness(config());
        fail(&h.breaker);
        fail(&h.breaker);
        fail(&h.breaker);
        assert_eq!(h.breaker.state(), BreakerState::Closed, "below min_calls");
        succeed(&h.breaker);
        assert_eq!(h.breaker.state(), BreakerState::Open, "3 of 4 failed");

        let error = h.breaker.acquire().unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.retry_after(), Some(secs(30)));
        assert_eq!(error.reason(), Some("CIRCUIT_OPEN"));
        h.clock.advance(secs(12));
        assert_eq!(
            h.breaker.acquire().unwrap_err().retry_after(),
            Some(secs(18))
        );

        h.clock.advance(secs(18));
        assert_eq!(h.breaker.state(), BreakerState::HalfOpen);
        let first = h.breaker.acquire().unwrap();
        let second = h.breaker.acquire().unwrap();
        let rejected = h.breaker.acquire().unwrap_err();
        assert_eq!(rejected.code(), ErrorCode::Unavailable);
        assert_eq!(rejected.retry_after(), None);
        first.record_success();
        assert_eq!(h.breaker.state(), BreakerState::HalfOpen);
        second.record_success();
        assert_eq!(h.breaker.state(), BreakerState::Closed);

        assert_eq!(
            *h.transitions.lock().unwrap(),
            vec![
                (BreakerState::Closed, BreakerState::Open),
                (BreakerState::Open, BreakerState::HalfOpen),
                (BreakerState::HalfOpen, BreakerState::Closed),
            ]
        );
        let metrics = h.breaker.metrics();
        assert_eq!(metrics.calls, 0, "closing resets the window");
        assert_eq!(metrics.rejected, 3);
    }

    #[test]
    fn half_open_failure_reopens() {
        let h = harness(config());
        for _ in 0..4 {
            fail(&h.breaker);
        }
        assert_eq!(h.breaker.state(), BreakerState::Open);
        h.clock.advance(secs(30));
        let probe = h.breaker.acquire().unwrap();
        probe.record_failure();
        assert_eq!(h.breaker.state(), BreakerState::Open);
        assert_eq!(
            h.breaker.acquire().unwrap_err().retry_after(),
            Some(secs(30))
        );
        assert_eq!(
            h.transitions.lock().unwrap().last(),
            Some(&(BreakerState::HalfOpen, BreakerState::Open))
        );
    }

    #[test]
    fn non_tripping_errors_count_as_successes() {
        let h = harness(config());
        for _ in 0..10 {
            h.breaker
                .acquire()
                .unwrap()
                .record::<()>(&Err(AppError::invalid_argument("bad input")));
        }
        assert_eq!(h.breaker.state(), BreakerState::Closed);
        let metrics = h.breaker.metrics();
        assert_eq!((metrics.calls, metrics.failures), (10, 0));
    }

    #[test]
    fn classifier_is_overridable() {
        let breaker = CircuitBreaker::new("orders", config())
            .unwrap()
            .with_clock(Arc::new(WallClock::new(ManualClock::new(
                SystemTime::UNIX_EPOCH,
            ))))
            .with_classifier(|error| error.code() == ErrorCode::Internal);
        for _ in 0..4 {
            breaker
                .acquire()
                .unwrap()
                .record::<()>(&Err(AppError::unavailable("down")));
        }
        assert_eq!(breaker.state(), BreakerState::Closed);
        for _ in 0..4 {
            breaker
                .acquire()
                .unwrap()
                .record::<()>(&Err(AppError::internal("boom")));
        }
        assert_eq!(breaker.state(), BreakerState::Open);
    }

    #[test]
    fn count_window_forgets_old_calls() {
        let h = harness(CircuitBreakerConfig {
            min_calls: 10,
            ..config()
        });
        for _ in 0..4 {
            fail(&h.breaker);
        }
        for _ in 0..10 {
            succeed(&h.breaker);
        }
        let metrics = h.breaker.metrics();
        assert_eq!((metrics.calls, metrics.failures), (10, 0));
        for _ in 0..4 {
            fail(&h.breaker);
        }
        assert_eq!(h.breaker.metrics().failure_rate_permille, 400);
        assert_eq!(h.breaker.state(), BreakerState::Closed);
        fail(&h.breaker);
        assert_eq!(
            h.breaker.state(),
            BreakerState::Open,
            "5 of the last 10 failed"
        );
    }

    #[test]
    fn time_window_forgets_old_calls() {
        let h = harness(CircuitBreakerConfig {
            window: BreakerWindow::Time { duration: secs(10) },
            ..config()
        });
        for _ in 0..3 {
            fail(&h.breaker);
        }
        h.clock.advance(secs(11));
        assert_eq!(h.breaker.metrics().calls, 0);
        fail(&h.breaker);
        succeed(&h.breaker);
        succeed(&h.breaker);
        assert_eq!(
            h.breaker.state(),
            BreakerState::Closed,
            "old failures expired"
        );
        h.clock.advance(secs(5));
        fail(&h.breaker);
        let metrics = h.breaker.metrics();
        assert_eq!((metrics.calls, metrics.failures), (4, 2));
        assert_eq!(metrics.state, BreakerState::Open);
    }

    #[test]
    fn stale_and_dropped_permits() {
        let h = harness(CircuitBreakerConfig {
            permitted_in_half_open: 1,
            ..config()
        });
        let stale = h.breaker.acquire().unwrap();
        for _ in 0..4 {
            fail(&h.breaker);
        }
        stale.record_failure();
        assert_eq!(h.breaker.state(), BreakerState::Open);
        h.clock.advance(secs(30));
        let probe = h.breaker.acquire().unwrap();
        assert!(h.breaker.acquire().is_err());
        drop(probe);
        let probe = h.breaker.acquire().unwrap();
        assert!(format!("{probe:?}").contains("orders"));
        probe.record_success();
        assert_eq!(h.breaker.state(), BreakerState::Closed);
        drop(h.breaker.acquire().unwrap());
        assert_eq!(
            h.breaker.metrics().calls,
            0,
            "an unreported closed permit records nothing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn call_records_outcomes_on_the_tokio_clock() {
        let breaker = CircuitBreaker::new("orders", config()).unwrap();
        assert_eq!(breaker.name(), "orders");
        for _ in 0..4 {
            let _ = breaker
                .call(async { Err::<(), _>(AppError::unavailable("down")) })
                .await;
        }
        assert_eq!(breaker.state(), BreakerState::Open);
        tokio::time::advance(secs(30)).await;
        assert_eq!(
            breaker.call(async { Ok::<_, AppError>(1) }).await.unwrap(),
            1
        );
        assert_eq!(
            breaker.call(async { Ok::<_, AppError>(2) }).await.unwrap(),
            2
        );
        assert_eq!(breaker.state(), BreakerState::Closed);
        assert!(format!("{breaker:?}").contains("orders"));
    }

    #[test]
    fn validation() {
        let bad = |config| CircuitBreaker::new("x", config).unwrap_err().parameter();
        assert_eq!(
            bad(CircuitBreakerConfig {
                failure_rate: 0.0,
                ..config()
            }),
            "breaker.failure_rate"
        );
        assert_eq!(
            bad(CircuitBreakerConfig {
                failure_rate: 1.5,
                ..config()
            }),
            "breaker.failure_rate"
        );
        assert_eq!(
            bad(CircuitBreakerConfig {
                window: BreakerWindow::Count { size: 0 },
                ..config()
            }),
            "breaker.window"
        );
        assert_eq!(
            bad(CircuitBreakerConfig {
                window: BreakerWindow::Time {
                    duration: Duration::ZERO
                },
                ..config()
            }),
            "breaker.window"
        );
        assert_eq!(
            bad(CircuitBreakerConfig {
                min_calls: 0,
                ..config()
            }),
            "breaker.min_calls"
        );
        assert_eq!(
            bad(CircuitBreakerConfig {
                permitted_in_half_open: 0,
                ..config()
            }),
            "breaker.permitted_in_half_open"
        );
        assert_eq!(rate_to_permille(0.0001).unwrap(), 1);
        assert!(CircuitBreaker::new("x", CircuitBreakerConfig::default()).is_ok());
    }

    #[test]
    fn names_and_clocks() {
        assert_eq!(BreakerState::HalfOpen.to_string(), "half_open");
        assert_eq!(BreakerState::Closed.as_str(), "closed");
        assert_eq!(BreakerState::Open.as_str(), "open");
        assert_eq!(permille(0, 0), 0);
        let wall = WallClock::new(ManualClock::new(SystemTime::UNIX_EPOCH - secs(5)));
        assert_eq!(wall.elapsed(), Duration::ZERO);
        let tokio_clock = TokioClock::default();
        assert!(tokio_clock.elapsed() < secs(60));
    }
}
