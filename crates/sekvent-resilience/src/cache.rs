use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use sekvent_error::{AppError, WireError};
use tokio::sync::watch;

use crate::{MonotonicClock, PolicyError, TokioClock};

/// Entries a cache holds unless [`TtlCacheBuilder::max_entries`] says otherwise.
const DEFAULT_MAX_ENTRIES: usize = 10_000;
/// Longest pause between reloads of a key that is served stale.
const MAX_RELOAD_PAUSE: Duration = Duration::from_secs(5);

/// What a finished load hands to the callers waiting on it.
type Outcome<V> = Option<Result<V, WireError>>;

/// A small in-memory cache with per-key single flight. Cheap to clone;
/// clones share entries.
///
/// Values stay fresh for the cache's TTL, measured on a [`MonotonicClock`]
/// ([`TokioClock`] unless another is injected). [`get`](Self::get) only
/// returns fresh values. [`get_or_try_insert`](Self::get_or_try_insert)
/// loads a missing or expired value once per key, however many callers
/// wait: the first caller runs its loader, the others receive its value, or
/// its error rebuilt from the wire form (code, message, reason, metadata),
/// while the loader's caller gets the original. A caller dropped mid-load
/// hands the load to one of the waiters. No lock is held across an await.
///
/// With [`stale_if_error`](TtlCacheBuilder::stale_if_error), a reload that
/// fails with a transient error ([`AppError::is_transient`]) within the
/// window past expiry returns the expired value instead, to the loader's
/// caller and to the waiters; the key is then not reloaded for the TTL or
/// five seconds, whichever is shorter. Other errors are returned as they are.
///
/// Inserting into a full cache first drops entries past their stale window,
/// then the one that expires soonest. The cache is meant for small hot sets
/// (tokens, permissions, settings), not as a general-purpose cache.
pub struct TtlCache<K, V> {
    inner: Arc<Inner<K, V>>,
}

struct Inner<K, V> {
    ttl: Duration,
    max_entries: usize,
    stale_window: Duration,
    clock: Arc<dyn MonotonicClock>,
    state: Mutex<State<K, V>>,
}

struct State<K, V> {
    entries: HashMap<K, Entry<V>>,
    flights: HashMap<K, Flight<V>>,
    next_flight: u64,
}

struct Entry<V> {
    value: V,
    /// Fresh before this point on the cache's clock.
    expires_at: Duration,
    /// Servable after a failed reload before this point.
    stale_until: Duration,
    /// After serving stale, no reload before this point.
    reload_after: Option<Duration>,
}

struct Flight<V> {
    id: u64,
    done: watch::Sender<Outcome<V>>,
}

enum Join<V> {
    Ready(V),
    Wait(watch::Receiver<Outcome<V>>),
    Lead(u64),
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// A cache keeping values for `ttl` (which must be positive), at most
    /// 10 000 entries, without stale serving.
    pub fn new(ttl: Duration) -> Result<Self, PolicyError> {
        Self::builder(ttl).build()
    }

    /// A builder for a cache keeping values for `ttl`.
    pub fn builder(ttl: Duration) -> TtlCacheBuilder<K, V> {
        TtlCacheBuilder {
            ttl,
            max_entries: DEFAULT_MAX_ENTRIES,
            stale_window: Duration::ZERO,
            clock: None,
            marker: PhantomData,
        }
    }

    /// A fresh value; never a stale one.
    pub fn get(&self, key: &K) -> Option<V> {
        let now = self.inner.now();
        let state = self.inner.lock();
        state
            .entries
            .get(key)
            .filter(|entry| now < entry.expires_at)
            .map(|entry| entry.value.clone())
    }

    /// Store `value`, fresh for the TTL, replacing any entry for `key`.
    pub fn insert(&self, key: K, value: V) {
        let now = self.inner.now();
        let mut state = self.inner.lock();
        self.inner.store(&mut state, key, value, now);
    }

    /// Drop the entry for `key`; whether there was one. A load already
    /// running for `key` still stores its value.
    pub fn invalidate(&self, key: &K) -> bool {
        self.inner.lock().entries.remove(key).is_some()
    }

    /// Drop every entry.
    pub fn clear(&self) {
        self.inner.lock().entries.clear();
    }

    /// Entries held, fresh or within their stale window.
    pub fn len(&self) -> usize {
        let now = self.inner.now();
        let mut state = self.inner.lock();
        state.entries.retain(|_, entry| now < entry.stale_until);
        state.entries.len()
    }

    /// Whether [`len`](Self::len) is zero.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The fresh value, or `load`'s, loading once per key however many wait.
    ///
    /// See the [type docs](Self) for how waiters, errors and stale values
    /// are handled.
    pub async fn get_or_try_insert<F, Fut>(&self, key: K, load: F) -> Result<V, AppError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, AppError>>,
    {
        let id = loop {
            match self.inner.join(&key) {
                Join::Ready(value) => return Ok(value),
                Join::Lead(id) => break id,
                Join::Wait(mut done) => {
                    let outcome: Outcome<V> = match done.wait_for(Option::is_some).await {
                        Ok(outcome) => Option::clone(&outcome),
                        // The loading caller went away; try to take over.
                        Err(_) => None,
                    };
                    match outcome {
                        Some(Ok(value)) => return Ok(value),
                        Some(Err(wire)) => return Err(AppError::from_wire(wire)),
                        None => {}
                    }
                }
            }
        };
        let mut flight = FlightGuard {
            inner: &self.inner,
            key,
            id,
            armed: true,
        };
        let outcome = load().await;
        flight.complete(outcome)
    }

    #[cfg(test)]
    fn waiters(&self, key: &K) -> usize {
        self.inner
            .lock()
            .flights
            .get(key)
            .map_or(0, |flight| flight.done.receiver_count())
    }
}

impl<K, V> Inner<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    fn lock(&self) -> MutexGuard<'_, State<K, V>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn join(&self, key: &K) -> Join<V> {
        let now = self.now();
        let mut state = self.lock();
        if let Some(entry) = state.entries.get(key) {
            let paused = entry.reload_after.is_some_and(|until| now < until);
            if now < entry.expires_at || (paused && now < entry.stale_until) {
                return Join::Ready(entry.value.clone());
            }
        }
        if let Some(flight) = state.flights.get(key) {
            return Join::Wait(flight.done.subscribe());
        }
        let id = state.next_flight;
        state.next_flight = state.next_flight.wrapping_add(1);
        let (done, _) = watch::channel(None);
        state.flights.insert(key.clone(), Flight { id, done });
        Join::Lead(id)
    }

    fn store(&self, state: &mut State<K, V>, key: K, value: V, now: Duration) {
        if !state.entries.contains_key(&key) && state.entries.len() >= self.max_entries {
            state.entries.retain(|_, entry| now < entry.stale_until);
            if state.entries.len() >= self.max_entries
                && let Some(victim) = state
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.expires_at)
                    .map(|(key, _)| key.clone())
            {
                state.entries.remove(&victim);
            }
        }
        let expires_at = now.saturating_add(self.ttl);
        state.entries.insert(
            key,
            Entry {
                value,
                expires_at,
                stale_until: expires_at.saturating_add(self.stale_window),
                reload_after: None,
            },
        );
    }

    /// The expired value to serve instead of `error`, if the rules allow.
    fn stale_for(
        &self,
        state: &mut State<K, V>,
        key: &K,
        error: &AppError,
        now: Duration,
    ) -> Option<V> {
        if !error.is_transient() {
            return None;
        }
        let entry = state.entries.get_mut(key)?;
        if now >= entry.stale_until {
            return None;
        }
        entry.reload_after = Some(now.saturating_add(self.ttl.min(MAX_RELOAD_PAUSE)));
        tracing::warn!(
            code = %error.code(),
            reason = error.reason().unwrap_or_default(),
            "cache reload failed; serving the expired value"
        );
        Some(entry.value.clone())
    }
}

/// The loading caller's claim on a key's flight. Dropped before
/// [`complete`](Self::complete) has published the outcome (a panic while
/// storing included), it ends the flight so a waiter can take over.
struct FlightGuard<'a, K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    inner: &'a Inner<K, V>,
    key: K,
    id: u64,
    armed: bool,
}

impl<K, V> FlightGuard<'_, K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn complete(&mut self, outcome: Result<V, AppError>) -> Result<V, AppError> {
        let now = self.inner.now();
        let mut state = self.inner.lock();
        let (result, shared) = match outcome {
            Ok(value) => {
                self.inner
                    .store(&mut state, self.key.clone(), value.clone(), now);
                (Ok(value.clone()), Ok(value))
            }
            Err(error) => {
                if let Some(value) = self.inner.stale_for(&mut state, &self.key, &error, now) {
                    (Ok(value.clone()), Ok(value))
                } else {
                    let wire = error.to_wire();
                    (Err(error), Err(wire))
                }
            }
        };
        let flight = self.take_flight(&mut state);
        self.armed = false;
        if let Some(flight) = flight {
            flight.done.send_replace(Some(shared));
        }
        result
    }

    fn take_flight(&self, state: &mut State<K, V>) -> Option<Flight<V>> {
        if state
            .flights
            .get(&self.key)
            .is_some_and(|flight| flight.id == self.id)
        {
            state.flights.remove(&self.key)
        } else {
            None
        }
    }
}

impl<K, V> Drop for FlightGuard<'_, K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn drop(&mut self) {
        if self.armed {
            let mut state = self.inner.lock();
            // Dropping the sender wakes the waiters, which retry.
            drop(self.take_flight(&mut state));
        }
    }
}

impl<K, V> Clone for TtlCache<K, V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> fmt::Debug for TtlCache<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TtlCache")
            .field("ttl", &self.inner.ttl)
            .field("max_entries", &self.inner.max_entries)
            .field("stale_if_error", &self.inner.stale_window)
            .finish_non_exhaustive()
    }
}

/// Builds a [`TtlCache`].
pub struct TtlCacheBuilder<K, V> {
    ttl: Duration,
    max_entries: usize,
    stale_window: Duration,
    clock: Option<Arc<dyn MonotonicClock>>,
    marker: PhantomData<fn() -> (K, V)>,
}

impl<K, V> TtlCacheBuilder<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Hold at most `max` entries (must be positive; default 10 000).
    #[must_use]
    pub fn max_entries(mut self, max: usize) -> Self {
        self.max_entries = max;
        self
    }

    /// Serve an expired value up to `window` past its expiry when reloading
    /// fails transiently. Zero (the default) never serves stale values.
    #[must_use]
    pub fn stale_if_error(mut self, window: Duration) -> Self {
        self.stale_window = window;
        self
    }

    /// Measure TTLs on `clock` instead of [`TokioClock`].
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn MonotonicClock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The cache; fails when the TTL or the entry limit is zero.
    pub fn build(self) -> Result<TtlCache<K, V>, PolicyError> {
        if self.ttl.is_zero() {
            return Err(PolicyError::new("cache.ttl", "must be positive"));
        }
        if self.max_entries == 0 {
            return Err(PolicyError::new("cache.max_entries", "must be positive"));
        }
        Ok(TtlCache {
            inner: Arc::new(Inner {
                ttl: self.ttl,
                max_entries: self.max_entries,
                stale_window: self.stale_window,
                clock: self.clock.unwrap_or_else(|| Arc::new(TokioClock::new())),
                state: Mutex::new(State {
                    entries: HashMap::new(),
                    flights: HashMap::new(),
                    next_flight: 0,
                }),
            }),
        })
    }
}

impl<K, V> fmt::Debug for TtlCacheBuilder<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TtlCacheBuilder")
            .field("ttl", &self.ttl)
            .field("max_entries", &self.max_entries)
            .field("stale_if_error", &self.stale_window)
            .field("custom_clock", &self.clock.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    use sekvent_context::ManualClock;
    use sekvent_error::ErrorCode;
    use tokio::sync::Notify;
    use tokio::time::advance;

    use super::*;
    use crate::WallClock;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn cache(ttl: u64) -> TtlCache<&'static str, u32> {
        TtlCache::new(secs(ttl)).unwrap()
    }

    fn stale_cache(ttl: u64, window: u64) -> TtlCache<&'static str, u32> {
        TtlCache::builder(secs(ttl))
            .stale_if_error(secs(window))
            .build()
            .unwrap()
    }

    async fn load_value(cache: &TtlCache<&'static str, u32>, key: &'static str, value: u32) {
        assert_eq!(
            cache
                .get_or_try_insert(key, || async move { Ok(value) })
                .await
                .unwrap(),
            value
        );
    }

    async fn wait_for_waiters(cache: &TtlCache<&'static str, u32>, key: &'static str, n: usize) {
        while cache.waiters(&key) < n {
            tokio::task::yield_now().await;
        }
    }

    /// A value whose clone panics when it holds zero.
    #[derive(Debug, PartialEq)]
    struct Fragile(u32);

    impl Clone for Fragile {
        fn clone(&self) -> Self {
            assert_ne!(self.0, 0, "cloning a fragile value");
            Self(self.0)
        }
    }

    #[test]
    fn parameters_are_validated() {
        let error = TtlCache::<u8, u8>::new(Duration::ZERO).unwrap_err();
        assert_eq!(error.parameter(), "cache.ttl");
        let error = TtlCache::<u8, u8>::builder(secs(1))
            .max_entries(0)
            .build()
            .unwrap_err();
        assert_eq!(error.parameter(), "cache.max_entries");
        assert_eq!(error.reason(), "must be positive");
    }

    #[test]
    fn debug_output() {
        let builder = TtlCache::<u8, u8>::builder(secs(3)).max_entries(7);
        assert_eq!(
            format!("{builder:?}"),
            "TtlCacheBuilder { ttl: 3s, max_entries: 7, stale_if_error: 0ns, custom_clock: false, .. }"
        );
        let cache = builder.stale_if_error(secs(9)).build().unwrap();
        assert_eq!(
            format!("{cache:?}"),
            "TtlCache { ttl: 3s, max_entries: 7, stale_if_error: 9s, .. }"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hit_miss_and_expiry() {
        let cache = cache(10);
        assert_eq!(cache.get(&"a"), None);
        assert!(cache.is_empty());
        cache.insert("a", 1);
        assert_eq!(cache.get(&"a"), Some(1));
        assert_eq!(cache.len(), 1);
        advance(secs(9)).await;
        assert_eq!(cache.get(&"a"), Some(1));
        advance(secs(1)).await;
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn clones_share_entries() {
        let cache = cache(10);
        let other = cache.clone();
        cache.insert("a", 1);
        assert_eq!(other.get(&"a"), Some(1));
        other.insert("a", 2);
        assert_eq!(cache.get(&"a"), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn loads_once_and_caches() {
        let cache = cache(10);
        let loads = &AtomicUsize::new(0);
        for _ in 0..3 {
            let value = cache
                .get_or_try_insert("a", move || async move {
                    loads.fetch_add(1, Ordering::SeqCst);
                    Ok(5)
                })
                .await
                .unwrap();
            assert_eq!(value, 5);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        advance(secs(10)).await;
        let value = cache
            .get_or_try_insert("a", move || async move {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(6)
            })
            .await
            .unwrap();
        assert_eq!(value, 6);
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn errors_are_not_cached() {
        let cache = cache(10);
        let error = cache
            .get_or_try_insert("a", || async { Err(AppError::not_found("missing")) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(cache.get(&"a"), None);
        load_value(&cache, "a", 1).await;
        assert_eq!(cache.get(&"a"), Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn twenty_callers_one_load() {
        let cache = cache(60);
        let loads = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let cache = cache.clone();
            let loads = Arc::clone(&loads);
            let release = Arc::clone(&release);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_try_insert("k", || async move {
                        loads.fetch_add(1, Ordering::SeqCst);
                        release.notified().await;
                        Ok(42)
                    })
                    .await
            }));
        }
        wait_for_waiters(&cache, "k", 19).await;
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        release.notify_one();
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), 42);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(cache.waiters(&"k"), 0);
        assert_eq!(cache.get(&"k"), Some(42));
    }

    #[tokio::test(start_paused = true)]
    async fn waiters_share_the_error_code_and_reason() {
        let cache = cache(60);
        let release = Arc::new(Notify::new());
        let leader = Arc::new(AtomicUsize::new(usize::MAX));
        let mut tasks = Vec::new();
        for index in 0..3 {
            let cache = cache.clone();
            let release = Arc::clone(&release);
            let leader = Arc::clone(&leader);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_try_insert("k", || async move {
                        leader.store(index, Ordering::SeqCst);
                        release.notified().await;
                        Err(AppError::unavailable("upstream down")
                            .with_reason("UPSTREAM_DOWN")
                            .with_metadata("upstream", "billing")
                            .with_source(std::io::Error::other("refused")))
                    })
                    .await
            }));
        }
        wait_for_waiters(&cache, "k", 2).await;
        release.notify_one();
        let leader = leader.load(Ordering::SeqCst);
        for (index, task) in tasks.into_iter().enumerate() {
            let error = task.await.unwrap().unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unavailable);
            assert_eq!(error.reason(), Some("UPSTREAM_DOWN"));
            assert_eq!(error.message(), "upstream down");
            assert_eq!(error.metadata().get("upstream").unwrap(), "billing");
            let has_source = std::error::Error::source(&error).is_some();
            assert_eq!(has_source, index == leader, "caller {index}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_leader_hands_over_to_a_waiter() {
        let cache = cache(60);
        let loads = Arc::new(AtomicUsize::new(0));
        let first = {
            let cache = cache.clone();
            let loads = Arc::clone(&loads);
            tokio::spawn(async move {
                cache
                    .get_or_try_insert("k", || async move {
                        loads.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<Result<u32, AppError>>().await
                    })
                    .await
            })
        };
        while loads.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let second = {
            let cache = cache.clone();
            let loads = Arc::clone(&loads);
            tokio::spawn(async move {
                cache
                    .get_or_try_insert("k", || async move {
                        loads.fetch_add(1, Ordering::SeqCst);
                        Ok(2)
                    })
                    .await
            })
        };
        wait_for_waiters(&cache, "k", 1).await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(second.await.unwrap().unwrap(), 2);
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert_eq!(cache.get(&"k"), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_panic_while_storing_does_not_strand_the_key() {
        let cache: TtlCache<&'static str, Fragile> = TtlCache::new(secs(10)).unwrap();
        let release = Arc::new(Notify::new());
        let leader = {
            let cache = cache.clone();
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                cache
                    .get_or_try_insert("k", || async move {
                        release.notified().await;
                        Ok(Fragile(0))
                    })
                    .await
            })
        };
        while cache.inner.lock().flights.is_empty() {
            tokio::task::yield_now().await;
        }
        let waiter = {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .get_or_try_insert("k", || async { Ok(Fragile(7)) })
                    .await
            })
        };
        while cache.waiters(&"k") < 1 {
            tokio::task::yield_now().await;
        }
        release.notify_one();
        assert!(leader.await.unwrap_err().is_panic());
        let taken_over = tokio::time::timeout(secs(30), waiter)
            .await
            .expect("the waiter hung")
            .unwrap()
            .unwrap();
        assert_eq!(taken_over, Fragile(7));
        assert!(cache.inner.lock().flights.is_empty());
        let later = tokio::time::timeout(
            secs(30),
            cache.get_or_try_insert("k", || async { Ok(Fragile(8)) }),
        )
        .await
        .expect("a later caller hung")
        .unwrap();
        assert_eq!(later, Fragile(7));
    }
    #[tokio::test(start_paused = true)]
    async fn stale_within_the_window_for_transient_errors() {
        let cache = stale_cache(10, 60);
        load_value(&cache, "a", 1).await;
        advance(secs(15)).await;
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.len(), 1);

        let loads = &AtomicUsize::new(0);
        let unavailable = move || async move {
            loads.fetch_add(1, Ordering::SeqCst);
            Err(AppError::unavailable("down").with_reason("DOWN"))
        };
        assert_eq!(cache.get_or_try_insert("a", unavailable).await.unwrap(), 1);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        // Still expired for plain reads.
        assert_eq!(cache.get(&"a"), None);

        // The reload pause: min(ttl, 5 s) = 5 s without loading.
        assert_eq!(cache.get_or_try_insert("a", unavailable).await.unwrap(), 1);
        advance(secs(4)).await;
        assert_eq!(cache.get_or_try_insert("a", unavailable).await.unwrap(), 1);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        advance(secs(1)).await;
        assert_eq!(cache.get_or_try_insert("a", unavailable).await.unwrap(), 1);
        assert_eq!(loads.load(Ordering::SeqCst), 2);

        // A successful reload replaces the stale value.
        advance(secs(5)).await;
        load_value(&cache, "a", 2).await;
        assert_eq!(cache.get(&"a"), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn the_reload_pause_is_capped_by_the_ttl() {
        let cache = stale_cache(2, 60);
        load_value(&cache, "a", 1).await;
        advance(secs(3)).await;
        let loads = &AtomicUsize::new(0);
        let unavailable = move || async move {
            loads.fetch_add(1, Ordering::SeqCst);
            Err(AppError::unavailable("down"))
        };
        assert_eq!(cache.get_or_try_insert("a", unavailable).await.unwrap(), 1);
        advance(secs(2)).await;
        assert_eq!(cache.get_or_try_insert("a", unavailable).await.unwrap(), 1);
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn no_stale_for_permanent_errors() {
        let cache = stale_cache(10, 60);
        load_value(&cache, "a", 1).await;
        advance(secs(15)).await;
        let error = cache
            .get_or_try_insert("a", || async { Err(AppError::not_found("gone")) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[tokio::test(start_paused = true)]
    async fn no_stale_past_the_window_or_without_one() {
        let stale = stale_cache(10, 60);
        load_value(&stale, "a", 1).await;
        advance(secs(70)).await;
        let error = stale
            .get_or_try_insert("a", || async { Err(AppError::unavailable("down")) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);

        let plain = cache(10);
        load_value(&plain, "a", 1).await;
        advance(secs(15)).await;
        let error = plain
            .get_or_try_insert("a", || async { Err(AppError::unavailable("down")) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);

        // Never loaded: nothing stale to serve.
        let error = stale
            .get_or_try_insert("b", || async { Err(AppError::unavailable("down")) })
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
    }

    #[tokio::test(start_paused = true)]
    async fn waiters_receive_the_stale_value() {
        let cache = stale_cache(10, 60);
        load_value(&cache, "k", 7).await;
        advance(secs(15)).await;
        let release = Arc::new(Notify::new());
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let cache = cache.clone();
            let release = Arc::clone(&release);
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_try_insert("k", || async move {
                        release.notified().await;
                        Err(AppError::unavailable("down"))
                    })
                    .await
            }));
        }
        wait_for_waiters(&cache, "k", 2).await;
        release.notify_one();
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), 7);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_cache_drops_dead_entries_first() {
        let cache: TtlCache<&str, u32> =
            TtlCache::builder(secs(10)).max_entries(3).build().unwrap();
        cache.insert("a", 1);
        cache.insert("b", 2);
        advance(secs(5)).await;
        cache.insert("c", 3);
        advance(secs(6)).await;
        cache.insert("d", 4);
        // Both dead entries went, not just the one expiring soonest.
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&"c"), Some(3));
        assert_eq!(cache.get(&"d"), Some(4));
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_cache_drops_the_entry_expiring_soonest() {
        let cache: TtlCache<&str, u32> =
            TtlCache::builder(secs(10)).max_entries(2).build().unwrap();
        cache.insert("a", 1);
        advance(secs(1)).await;
        cache.insert("b", 2);
        advance(secs(1)).await;
        // Replacing an existing key evicts nothing.
        cache.insert("a", 10);
        assert_eq!(cache.get(&"b"), Some(2));
        cache.insert("c", 3);
        assert_eq!(cache.get(&"a"), Some(10));
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"c"), Some(3));
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn invalidate_and_clear() {
        let cache = cache(10);
        cache.insert("a", 1);
        cache.insert("b", 2);
        assert!(cache.invalidate(&"a"));
        assert!(!cache.invalidate(&"a"));
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(2));
        cache.clear();
        assert!(cache.is_empty());
        load_value(&cache, "a", 3).await;
        assert_eq!(cache.get(&"a"), Some(3));
    }

    #[test]
    fn an_injected_wall_clock() {
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH + secs(1_000));
        let cache: TtlCache<&str, u32> = TtlCache::builder(secs(10))
            .clock(Arc::new(WallClock::new(clock.clone())))
            .build()
            .unwrap();
        let builder =
            TtlCache::<u8, u8>::builder(secs(1)).clock(Arc::new(WallClock::new(clock.clone())));
        assert!(format!("{builder:?}").contains("custom_clock: true"));
        cache.insert("a", 1);
        clock.advance(secs(9));
        assert_eq!(cache.get(&"a"), Some(1));
        clock.advance(secs(1));
        assert_eq!(cache.get(&"a"), None);
    }

    #[test]
    fn a_stale_flight_guard_does_not_remove_a_newer_flight() {
        let cache = cache(10);
        let Join::Lead(old) = cache.inner.join(&"k") else {
            panic!("expected to lead");
        };
        // The old flight ends; a new one starts for the same key.
        let mut stale = FlightGuard {
            inner: &cache.inner,
            key: "k",
            id: old,
            armed: true,
        };
        drop(cache.inner.lock().flights.remove(&"k"));
        let Join::Lead(new) = cache.inner.join(&"k") else {
            panic!("expected to lead");
        };
        assert_ne!(old, new);
        assert_eq!(stale.complete(Ok(1)).unwrap(), 1);
        assert!(cache.inner.lock().flights.contains_key(&"k"));
        assert_eq!(cache.get(&"k"), Some(1));
    }
}
