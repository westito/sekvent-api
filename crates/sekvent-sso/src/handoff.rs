//! One-time handoff codes and the bounded expiring map behind them.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sekvent_config::Secret;
use sekvent_context::Clock;
use sekvent_error::AppError;

use crate::state::{digest, random_token};

/// Default lifetime of a handoff code.
pub const DEFAULT_HANDOFF_TTL: Duration = Duration::from_secs(60);

/// Default number of codes that may wait for redemption at once.
pub const DEFAULT_HANDOFF_CAPACITY: usize = 10_000;

/// Longest accepted handoff lifetime.
pub const MAX_HANDOFF_TTL: Duration = Duration::from_secs(600);

/// Length of a code: 32 random bytes in base64url without padding.
const CODE_LEN: usize = 43;

type Key = [u8; 32];

/// Entries keyed by a digest, each with an expiry (Unix milliseconds),
/// swept lazily in insertion order.
pub(crate) struct Expiring<V> {
    entries: HashMap<Key, (V, u64)>,
    order: VecDeque<(Key, u64)>,
    capacity: usize,
}

impl<V> Expiring<V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    /// Drop every entry whose expiry has passed, oldest first.
    pub(crate) fn sweep(&mut self, now: u64) {
        while let Some(&(key, expires)) = self.order.front() {
            if expires > now {
                break;
            }
            self.order.pop_front();
            self.remove_if(&key, expires);
        }
        // Taken entries leave their key in `order` until it expires; keep
        // the queue from outgrowing the map by much.
        if self.order.len() > self.capacity.saturating_mul(2) {
            let entries = &self.entries;
            self.order
                .retain(|(key, expires)| entries.get(key).is_some_and(|(_, e)| e == expires));
        }
    }

    fn remove_if(&mut self, key: &Key, expires: u64) {
        if self.entries.get(key).is_some_and(|(_, e)| *e == expires) {
            self.entries.remove(key);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Insert unless full; `false` when there is no room.
    pub(crate) fn insert(&mut self, key: Key, value: V, expires: u64) -> bool {
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            return false;
        }
        self.entries.insert(key, (value, expires));
        self.order.push_back((key, expires));
        true
    }

    /// Insert, evicting the oldest entries to make room.
    pub(crate) fn insert_evicting(&mut self, key: Key, value: V, expires: u64) {
        while self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            let Some((oldest, oldest_expires)) = self.order.pop_front() else {
                break;
            };
            self.remove_if(&oldest, oldest_expires);
        }
        self.entries.insert(key, (value, expires));
        self.order.push_back((key, expires));
    }

    /// Remove and return a live entry.
    pub(crate) fn take(&mut self, key: &Key, now: u64) -> Option<V> {
        let (value, expires) = self.entries.remove(key)?;
        (expires > now).then_some(value)
    }

    pub(crate) fn contains(&self, key: &Key, now: u64) -> bool {
        self.entries.get(key).is_some_and(|(_, e)| *e > now)
    }
}

/// Single-use codes that carry a sign-in from the callback to the
/// application.
///
/// The callback stores the application's login outcome under a fresh code
/// and sends the browser back with the code in the URL fragment; the
/// application's frontend posts the code to one of its own endpoints, which
/// calls [`redeem`](Self::redeem) and mints its session. A code is 32 bytes
/// from the operating system's random source, lives for the configured TTL
/// (60 s by default) and works once.
///
/// The store keeps codes in memory, so a code can only be redeemed on the
/// process that issued it. Only SHA-256 digests of codes are kept; a lookup
/// compares digests, so its timing says nothing about a stored code.
/// Expired codes are swept lazily on each call; there is no background
/// task. Clones share the store.
pub struct HandoffStore<O> {
    inner: Arc<Mutex<Expiring<O>>>,
    ttl: Duration,
    clock: Arc<dyn Clock>,
}

impl<O> Clone for HandoffStore<O> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            ttl: self.ttl,
            clock: Arc::clone(&self.clock),
        }
    }
}

impl<O> fmt::Debug for HandoffStore<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandoffStore")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl<O> HandoffStore<O> {
    /// A store whose codes live for `ttl` (more than zero, at most
    /// [`MAX_HANDOFF_TTL`]) with room for `capacity` (at least 1) waiting
    /// codes.
    pub fn new(ttl: Duration, capacity: usize, clock: Arc<dyn Clock>) -> Result<Self, AppError> {
        if ttl.is_zero() || ttl > MAX_HANDOFF_TTL {
            return Err(AppError::invalid_argument(
                "the SSO handoff TTL must be longer than zero and at most 10 minutes",
            ));
        }
        if capacity == 0 {
            return Err(AppError::invalid_argument(
                "the SSO handoff capacity must be at least 1",
            ));
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(Expiring::new(capacity))),
            ttl,
            clock,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Expiring<O>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lifetime of a code.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Store `outcome` under a fresh code and return the code.
    ///
    /// `RESOURCE_EXHAUSTED` when the store is full of live codes;
    /// `INTERNAL` when the random source fails.
    pub fn issue(&self, outcome: O) -> Result<Secret, AppError> {
        let code = random_token()?;
        let now = self.clock.now_unix_millis();
        let ttl_ms = u64::try_from(self.ttl.as_millis()).unwrap_or(u64::MAX);
        let mut inner = self.lock();
        inner.sweep(now);
        if !inner.insert(digest(&code), outcome, now.saturating_add(ttl_ms)) {
            return Err(AppError::resource_exhausted(
                "too many sign-ins are waiting to be completed",
            )
            .with_reason("SSO_HANDOFF_FULL"));
        }
        Ok(Secret::new(code))
    }

    /// Take the outcome stored under `code`. `None` for an unknown,
    /// expired, already redeemed or malformed code.
    pub fn redeem(&self, code: &str) -> Option<O> {
        if code.len() != CODE_LEN
            || !code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        let now = self.clock.now_unix_millis();
        let mut inner = self.lock();
        inner.sweep(now);
        inner.take(&digest(code), now)
    }

    /// The number of live codes.
    pub fn len(&self) -> usize {
        let now = self.clock.now_unix_millis();
        let mut inner = self.lock();
        inner.sweep(now);
        inner.len()
    }

    /// Whether no code is waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use sekvent_context::ManualClock;
    use sekvent_error::ErrorCode;

    use super::*;

    fn store(capacity: usize) -> (HandoffStore<String>, ManualClock) {
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
        let store =
            HandoffStore::new(DEFAULT_HANDOFF_TTL, capacity, Arc::new(clock.clone())).unwrap();
        (store, clock)
    }

    #[test]
    fn a_code_works_once() {
        let (store, _) = store(4);
        let code = store.issue("user-1".to_owned()).unwrap();
        assert_eq!(code.expose().len(), CODE_LEN);
        assert_eq!(store.len(), 1);
        assert_eq!(store.redeem(code.expose()).as_deref(), Some("user-1"));
        assert_eq!(store.redeem(code.expose()), None);
        assert!(store.is_empty());
    }

    #[test]
    fn a_code_expires_after_its_ttl() {
        let (store, clock) = store(4);
        let code = store.issue("user-1".to_owned()).unwrap();
        clock.advance(Duration::from_millis(59_999));
        let other = store.issue("user-2".to_owned()).unwrap();
        clock.advance(Duration::from_millis(1));
        assert_eq!(store.redeem(code.expose()), None);
        assert_eq!(store.len(), 1);
        assert_eq!(store.redeem(other.expose()).as_deref(), Some("user-2"));
    }

    #[test]
    fn an_expired_entry_is_not_returned_even_unswept() {
        let mut map = Expiring::new(4);
        map.insert([1; 32], "v", 10);
        assert!(map.contains(&[1; 32], 9));
        assert!(!map.contains(&[1; 32], 10));
        assert_eq!(map.take(&[1; 32], 10), None);
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn a_full_store_refuses_new_codes_until_one_expires() {
        let (store, clock) = store(2);
        store.issue("a".to_owned()).unwrap();
        store.issue("b".to_owned()).unwrap();
        let error = store.issue("c".to_owned()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(error.reason(), Some("SSO_HANDOFF_FULL"));
        clock.advance(DEFAULT_HANDOFF_TTL);
        store.issue("c".to_owned()).unwrap();
    }

    #[test]
    fn malformed_and_unknown_codes_redeem_nothing() {
        let (store, _) = store(2);
        store.issue("a".to_owned()).unwrap();
        for code in [
            "",
            "short",
            &"a".repeat(CODE_LEN + 1),
            &"!".repeat(CODE_LEN),
        ] {
            assert_eq!(store.redeem(code), None);
        }
        assert_eq!(store.redeem(&"a".repeat(CODE_LEN)), None);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn settings_are_validated() {
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(SystemTime::UNIX_EPOCH));
        for (ttl, capacity) in [
            (Duration::ZERO, 1),
            (MAX_HANDOFF_TTL + Duration::from_secs(1), 1),
            (Duration::from_secs(1), 0),
        ] {
            let error = HandoffStore::<()>::new(ttl, capacity, Arc::clone(&clock)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
        }
        let store = HandoffStore::<()>::new(MAX_HANDOFF_TTL, 1, clock).unwrap();
        assert_eq!(store.ttl(), MAX_HANDOFF_TTL);
        let clone = store.clone();
        assert!(format!("{clone:?}").starts_with("HandoffStore"));
    }

    #[test]
    fn evicting_insert_drops_the_oldest() {
        let mut map = Expiring::new(2);
        map.insert_evicting([1; 32], (), 100);
        map.insert_evicting([2; 32], (), 100);
        map.insert_evicting([3; 32], (), 100);
        assert!(!map.contains(&[1; 32], 0));
        assert!(map.contains(&[2; 32], 0));
        assert!(map.contains(&[3; 32], 0));
        // Re-inserting a present key needs no room.
        map.insert_evicting([3; 32], (), 200);
        assert_eq!(map.len(), 2);
        assert!(map.insert([3; 32], (), 300));
        assert!(!map.insert([4; 32], (), 300));
    }

    #[test]
    fn evicting_with_an_empty_queue_still_inserts() {
        let mut map: Expiring<()> = Expiring::new(0);
        map.insert_evicting([1; 32], (), 100);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn the_order_queue_is_compacted() {
        let mut map = Expiring::new(1);
        for round in 0..5u8 {
            map.insert([round; 32], (), 1_000);
            assert!(map.take(&[round; 32], 0).is_some());
            map.sweep(0);
        }
        assert!(map.order.len() <= 2);
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn a_reinserted_key_survives_the_sweep_of_its_old_entry() {
        let mut map = Expiring::new(2);
        map.insert([1; 32], "old", 10);
        map.insert([1; 32], "new", 20);
        map.sweep(10);
        assert_eq!(map.take(&[1; 32], 10), Some("new"));
    }
}
