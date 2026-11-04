use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sekvent_context::CallContext;
use sekvent_error::{AppError, ErrorCode};
use tokio::time::Instant;

use crate::PolicyError;

/// Reason of the error a [`ClientLimiter`] refuses a call with
/// (`RESOURCE_EXHAUSTED`, `retry_after` set).
pub const CLIENT_RATE_LIMITED: &str = "CLIENT_RATE_LIMITED";

/// Longest accepted refill period: a year and a day, well inside what an
/// instant can represent.
const MAX_PER: Duration = Duration::from_hours(366 * 24);

/// Longest time an empty bucket may take to fill.
const MAX_CAPACITY: Duration = Duration::from_hours(3660 * 24);

/// Default [`KeyedLimit::max_keys`].
pub const DEFAULT_MAX_KEYS: usize = 100_000;

/// The shape of a [`KeyedRateLimiter`]: `rate` permits per `per`, up to
/// `burst` at once, for at most `max_keys` keys.
///
/// [`new`](Self::new) sets `burst` to `rate`, `max_keys` to
/// [`DEFAULT_MAX_KEYS`] and `idle_ttl` to `per`; change them with the
/// `with_*` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyedLimit {
    /// Permits earned per `per`.
    pub rate: u32,
    /// The period `rate` permits are earned over.
    pub per: Duration,
    /// The bucket's capacity: the most permits taken at once.
    pub burst: u32,
    /// The most keys tracked at once.
    pub max_keys: usize,
    /// How long a full bucket stays tracked after its last use.
    pub idle_ttl: Duration,
}

impl KeyedLimit {
    /// `rate` permits per `per`, burst `rate`.
    pub fn new(rate: u32, per: Duration) -> Self {
        Self {
            rate,
            per,
            burst: rate,
            max_keys: DEFAULT_MAX_KEYS,
            idle_ttl: per,
        }
    }

    /// The most permits taken at once.
    #[must_use]
    pub fn with_burst(mut self, burst: u32) -> Self {
        self.burst = burst;
        self
    }

    /// The most keys tracked at once.
    #[must_use]
    pub fn with_max_keys(mut self, max_keys: usize) -> Self {
        self.max_keys = max_keys;
        self
    }

    /// How long a full bucket stays tracked after its last use.
    #[must_use]
    pub fn with_idle_ttl(mut self, idle_ttl: Duration) -> Self {
        self.idle_ttl = idle_ttl;
        self
    }

    /// Parse a rate written `<permits>/<period>`, such as `5/1m`, `100/1s`
    /// or `1000/1h` (the period as a duration such as `500ms`, `30s` or
    /// `2m`, or whole seconds), into [`new`](Self::new)'s shape. `None` for
    /// anything else; the values are checked by [`KeyedRateLimiter::new`].
    pub fn parse_rate(raw: &str) -> Option<Self> {
        let (permits, per) = raw.trim().split_once('/')?;
        let permits = permits.trim();
        if permits.is_empty() || !permits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let rate = permits.parse().ok()?;
        let per = per.trim();
        let per = match per.parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(_) => humantime::parse_duration(per).ok()?,
        };
        Some(Self::new(rate, per))
    }

    fn validate(&self) -> Result<(), PolicyError> {
        if self.rate == 0 {
            return Err(PolicyError::new("keyed_limit.rate", "must be at least 1"));
        }
        if self.per.is_zero() {
            return Err(PolicyError::new(
                "keyed_limit.per",
                "must be longer than zero",
            ));
        }
        if self.per > MAX_PER {
            return Err(PolicyError::new(
                "keyed_limit.per",
                "must be at most 366 days",
            ));
        }
        if self.burst == 0 {
            return Err(PolicyError::new("keyed_limit.burst", "must be at least 1"));
        }
        if self.max_keys == 0 {
            return Err(PolicyError::new(
                "keyed_limit.max_keys",
                "must be at least 1",
            ));
        }
        if self.idle_ttl > MAX_PER {
            return Err(PolicyError::new(
                "keyed_limit.idle_ttl",
                "must be at most 366 days",
            ));
        }
        Ok(())
    }
}

/// What a [`KeyedRateLimiter`] counts against.
///
/// An IPv4 address counts as itself (an IPv4-mapped IPv6 address as its
/// IPv4 form); an IPv6 address counts as its /64, the smallest block one
/// subscriber usually holds. [`unknown`](Self::unknown) is the one shared
/// key of calls whose client cannot be told apart.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RateKey(Repr);

#[derive(Clone, PartialEq, Eq, Hash)]
enum Repr {
    V4(u32),
    V6Prefix(u64),
    Subject(Arc<str>),
    Unknown,
}

impl RateKey {
    /// The key of a client address.
    pub fn ip(addr: IpAddr) -> Self {
        match addr.to_canonical() {
            IpAddr::V4(v4) => Self(Repr::V4(u32::from(v4))),
            IpAddr::V6(v6) => Self(Repr::V6Prefix(prefix_64(v6))),
        }
    }

    /// The key of an end-user subject.
    pub fn subject(subject: &str) -> Self {
        Self(Repr::Subject(Arc::from(subject)))
    }

    /// The shared key of calls without a known client.
    pub fn unknown() -> Self {
        Self(Repr::Unknown)
    }
}

/// Never shows the address or subject.
impl fmt::Debug for RateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Repr::V4(_) => "RateKey::Ipv4",
            Repr::V6Prefix(_) => "RateKey::Ipv6Prefix",
            Repr::Subject(_) => "RateKey::Subject",
            Repr::Unknown => "RateKey::Unknown",
        })
    }
}

fn prefix_64(addr: Ipv6Addr) -> u64 {
    let bits = u128::from(addr) >> 64;
    u64::try_from(bits).unwrap_or(u64::MAX)
}

/// A [`KeyedRateLimiter`] refused: try again after `retry_after`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    /// How long until the permits asked for are available. For a weight
    /// above the burst, which no wait satisfies, the time the bucket takes
    /// to fill from empty.
    pub retry_after: Duration,
}

impl fmt::Display for RateLimited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "too many requests; retry after {} ms",
            self.retry_after.as_millis()
        )
    }
}

impl std::error::Error for RateLimited {}

impl From<RateLimited> for AppError {
    /// `RESOURCE_EXHAUSTED` / [`CLIENT_RATE_LIMITED`] with `retry_after`.
    fn from(limited: RateLimited) -> Self {
        AppError::new(ErrorCode::ResourceExhausted, "too many requests")
            .with_reason(CLIENT_RATE_LIMITED)
            .with_retry_after(limited.retry_after)
    }
}

/// A token bucket per key: `rate` permits per `per`, up to `burst` at once.
///
/// Memory is bounded: a bucket is dropped once it is full again and has not
/// been used for `idle_ttl` (a full bucket is the same as no bucket), and
/// when `max_keys` keys are tracked a new key takes the place of the one
/// unused for longest. The limiter never fails for being full. Time is
/// tokio's clock, so tests drive it with `tokio::time::pause`.
///
/// The bucket is kept as the instant it will be full again (the generic
/// cell rate algorithm), so each key costs one instant and no timer.
pub struct KeyedRateLimiter {
    limit: KeyedLimit,
    /// The time one permit takes to come back.
    interval: Duration,
    /// The time an empty bucket takes to fill: `burst` intervals.
    capacity: Duration,
    /// The longer of `idle_ttl` and `capacity`: an entry unused for that
    /// long is full again.
    evict_after: Duration,
    state: Mutex<State>,
}

impl fmt::Debug for KeyedRateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedRateLimiter")
            .field("limit", &self.limit)
            .field("keys", &self.len())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    buckets: HashMap<RateKey, Bucket>,
    /// Keys by last use, oldest first; `(last_use, sequence)` is unique.
    by_use: BTreeMap<(Instant, u64), RateKey>,
    sequence: u64,
}

struct Bucket {
    /// When the bucket is full again; at or before now it is full.
    full_at: Instant,
    /// The entry in `by_use`.
    used: (Instant, u64),
}

impl KeyedRateLimiter {
    /// A limiter of this shape; `rate`, `burst` and `max_keys` must be at
    /// least 1, `per` longer than zero and, like `idle_ttl`, at most 366
    /// days, and an empty bucket must fill within 3660 days.
    pub fn new(limit: KeyedLimit) -> Result<Self, PolicyError> {
        limit.validate()?;
        let interval = (limit.per / limit.rate).max(Duration::from_nanos(1));
        let capacity = interval.saturating_mul(limit.burst);
        if capacity > MAX_CAPACITY {
            return Err(PolicyError::new(
                "keyed_limit.burst",
                "an empty bucket must fill within 3660 days",
            ));
        }
        Ok(Self {
            limit,
            interval,
            capacity,
            evict_after: limit.idle_ttl.max(capacity),
            state: Mutex::new(State::default()),
        })
    }

    /// The shape it was built with.
    pub fn limit(&self) -> KeyedLimit {
        self.limit
    }

    /// How many keys are tracked now.
    pub fn len(&self) -> usize {
        self.lock().buckets.len()
    }

    /// Whether no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take one permit for `key`.
    pub fn try_acquire(&self, key: &RateKey) -> Result<(), RateLimited> {
        self.try_acquire_n(key, 1)
    }

    /// Take `permits` permits for `key` at once, or none. Zero always
    /// succeeds; more than `burst` never does.
    pub fn try_acquire_n(&self, key: &RateKey, permits: u32) -> Result<(), RateLimited> {
        if permits == 0 {
            return Ok(());
        }
        if permits > self.limit.burst {
            return Err(RateLimited {
                retry_after: self.capacity,
            });
        }
        let now = Instant::now();
        let mut state = self.lock();
        state.evict_idle(now, self.evict_after);
        let cost = self.interval.saturating_mul(permits);
        let full_at = state
            .buckets
            .get(key)
            .map_or(now, |bucket| bucket.full_at.max(now));
        let after = full_at + cost;
        let outcome = match (after - now).checked_sub(self.capacity) {
            Some(wait) if !wait.is_zero() => Err(RateLimited { retry_after: wait }),
            _ => Ok(()),
        };
        let full_at = if outcome.is_ok() { after } else { full_at };
        state.touch(key, full_at, now, self.limit.max_keys);
        outcome
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    /// Drop the entries unused for `after`, oldest first.
    fn evict_idle(&mut self, now: Instant, after: Duration) {
        while let Some(entry) = self.by_use.first_entry() {
            let (used, _) = *entry.key();
            if used + after > now {
                break;
            }
            let key = entry.remove();
            self.buckets.remove(&key);
        }
    }

    /// Record a use of `key`, making room when a new key would exceed
    /// `max_keys`.
    fn touch(&mut self, key: &RateKey, full_at: Instant, now: Instant, max_keys: usize) {
        self.sequence = self.sequence.wrapping_add(1);
        let used = (now, self.sequence);
        if let Some(bucket) = self.buckets.get_mut(key) {
            self.by_use.remove(&bucket.used);
            bucket.full_at = full_at;
            bucket.used = used;
        } else {
            while self.buckets.len() >= max_keys {
                let Some((_, oldest)) = self.by_use.pop_first() else {
                    break;
                };
                self.buckets.remove(&oldest);
            }
            self.buckets.insert(key.clone(), Bucket { full_at, used });
        }
        self.by_use.insert(used, key.clone());
    }
}

/// What a [`ClientLimiter`] keys its buckets by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClientKey {
    /// The client address ([`CallContext::client_addr`]).
    ClientAddr,
    /// The end-user subject ([`CallContext::subject`]: an authenticated end
    /// user's, or the one a trusted link asserts).
    EndUser,
    /// The end-user subject when there is one, else the client address.
    EndUserOrClientAddr,
}

impl ClientKey {
    /// The configuration spelling: `client_addr`, `end_user`,
    /// `end_user_or_client_addr`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientAddr => "client_addr",
            Self::EndUser => "end_user",
            Self::EndUserOrClientAddr => "end_user_or_client_addr",
        }
    }

    /// Parse [`as_str`](Self::as_str)'s spellings exactly.
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::ClientAddr, Self::EndUser, Self::EndUserOrClientAddr]
            .into_iter()
            .find(|key| key.as_str() == raw)
    }

    /// Whether the key needs the end user, so the limit runs after
    /// end-user authentication.
    pub fn needs_end_user(self) -> bool {
        !matches!(self, Self::ClientAddr)
    }

    /// The bucket a call counts against; [`RateKey::unknown`] when the
    /// context carries nothing to key by.
    pub fn key_of(self, cx: &CallContext) -> RateKey {
        let subject = || cx.subject().map(RateKey::subject);
        let addr = || cx.client_addr().map(RateKey::ip);
        let key = match self {
            Self::ClientAddr => addr(),
            Self::EndUser => subject(),
            Self::EndUserOrClientAddr => subject().or_else(addr),
        };
        key.unwrap_or_else(RateKey::unknown)
    }
}

/// A [`KeyedRateLimiter`] keyed from the call context: the limit a served
/// component method or a REST route applies per client. Cheap to clone;
/// clones share their buckets, and two limiters are equal when they do.
#[derive(Debug, Clone)]
pub struct ClientLimiter {
    limiter: Arc<KeyedRateLimiter>,
    key: ClientKey,
}

impl PartialEq for ClientLimiter {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.limiter, &other.limiter) && self.key == other.key
    }
}

impl ClientLimiter {
    /// Buckets of `limit` keyed by `key`.
    pub fn new(limit: KeyedLimit, key: ClientKey) -> Result<Self, PolicyError> {
        Ok(Self {
            limiter: Arc::new(KeyedRateLimiter::new(limit)?),
            key,
        })
    }

    /// What it keys by.
    pub fn key(&self) -> ClientKey {
        self.key
    }

    /// The buckets.
    pub fn limiter(&self) -> &KeyedRateLimiter {
        &self.limiter
    }

    /// Take one permit for the call `cx` describes; `RESOURCE_EXHAUSTED` /
    /// [`CLIENT_RATE_LIMITED`] with `retry_after` when its bucket is empty.
    pub fn check(&self, cx: &CallContext) -> Result<(), AppError> {
        self.limiter
            .try_acquire(&self.key.key_of(cx))
            .map_err(AppError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn limiter(limit: KeyedLimit) -> KeyedRateLimiter {
        KeyedRateLimiter::new(limit).unwrap()
    }

    fn v4(last: u8) -> RateKey {
        RateKey::ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)))
    }

    #[test]
    fn invalid_shapes_are_refused() {
        let base = KeyedLimit::new(5, Duration::from_mins(1));
        for (limit, parameter) in [
            (KeyedLimit::new(0, SECOND), "keyed_limit.rate"),
            (KeyedLimit::new(1, Duration::ZERO), "keyed_limit.per"),
            (
                KeyedLimit::new(1, Duration::from_hours(367 * 24)),
                "keyed_limit.per",
            ),
            (base.with_burst(0), "keyed_limit.burst"),
            (base.with_max_keys(0), "keyed_limit.max_keys"),
            (
                base.with_idle_ttl(Duration::from_hours(367 * 24)),
                "keyed_limit.idle_ttl",
            ),
            (
                KeyedLimit::new(1, Duration::from_hours(366 * 24)).with_burst(11),
                "keyed_limit.burst",
            ),
        ] {
            assert_eq!(
                KeyedRateLimiter::new(limit).unwrap_err().parameter(),
                parameter
            );
        }
        let built = limiter(base);
        assert_eq!(built.limit(), base);
        assert_eq!(base.burst, 5);
        assert_eq!(base.max_keys, DEFAULT_MAX_KEYS);
        assert_eq!(base.idle_ttl, Duration::from_mins(1));
        assert!(built.is_empty());
        assert!(format!("{built:?}").contains("keys: 0"));
    }

    #[test]
    fn rates_parse_as_permits_per_period() {
        assert_eq!(
            KeyedLimit::parse_rate("5/1m"),
            Some(KeyedLimit::new(5, Duration::from_mins(1)))
        );
        assert_eq!(
            KeyedLimit::parse_rate(" 100 / 500ms "),
            Some(KeyedLimit::new(100, Duration::from_millis(500)))
        );
        assert_eq!(
            KeyedLimit::parse_rate("10/30"),
            Some(KeyedLimit::new(10, Duration::from_secs(30)))
        );
        assert_eq!(
            KeyedLimit::parse_rate("0/1s"),
            Some(KeyedLimit::new(0, SECOND)),
            "checked when built"
        );
        for bad in [
            "",
            "5",
            "5/",
            "/1m",
            "-5/1m",
            "+5/1m",
            "5/soon",
            "5.5/1m",
            "99999999999/1s",
        ] {
            assert_eq!(KeyedLimit::parse_rate(bad), None, "{bad:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_bucket_drains_and_refills_at_the_rate() {
        let limiter = limiter(KeyedLimit::new(5, Duration::from_mins(1)));
        let key = v4(1);
        for _ in 0..5 {
            limiter.try_acquire(&key).unwrap();
        }
        let refused = limiter.try_acquire(&key).unwrap_err();
        assert_eq!(refused.retry_after, Duration::from_secs(12));
        assert!(refused.to_string().contains("12000 ms"));

        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(
            limiter.try_acquire(&key).unwrap_err().retry_after,
            SECOND,
            "a refusal takes nothing"
        );
        tokio::time::advance(SECOND).await;
        limiter.try_acquire(&key).unwrap();
        assert!(limiter.try_acquire(&key).is_err());

        tokio::time::advance(Duration::from_mins(1)).await;
        for _ in 0..5 {
            limiter.try_acquire(&key).unwrap();
        }
        assert!(limiter.try_acquire(&key).is_err(), "never above the burst");
    }

    #[tokio::test(start_paused = true)]
    async fn keys_have_their_own_buckets() {
        let limiter = limiter(KeyedLimit::new(1, Duration::from_mins(1)));
        limiter.try_acquire(&v4(1)).unwrap();
        assert!(limiter.try_acquire(&v4(1)).is_err());
        limiter.try_acquire(&v4(2)).unwrap();
        limiter.try_acquire(&RateKey::subject("user-1")).unwrap();
        limiter.try_acquire(&RateKey::unknown()).unwrap();
        assert!(limiter.try_acquire(&RateKey::unknown()).is_err());
        assert_eq!(limiter.len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn weighted_acquisitions_take_all_or_nothing() {
        let limiter = limiter(KeyedLimit::new(10, Duration::from_secs(10)).with_burst(4));
        let key = v4(1);
        limiter.try_acquire_n(&key, 0).unwrap();
        limiter.try_acquire_n(&key, 3).unwrap();
        let refused = limiter.try_acquire_n(&key, 2).unwrap_err();
        assert_eq!(refused.retry_after, SECOND);
        limiter.try_acquire_n(&key, 1).unwrap();
        assert_eq!(
            limiter.try_acquire_n(&key, 5).unwrap_err().retry_after,
            Duration::from_secs(4),
            "above the burst: the time a full bucket takes"
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        limiter.try_acquire_n(&key, 4).unwrap();
    }

    #[test]
    fn ipv6_counts_by_its_64_and_mapped_ipv4_as_ipv4() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(RateKey::ip(a), RateKey::ip(b));
        assert_ne!(RateKey::ip(a), RateKey::ip(other));
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        assert_eq!(RateKey::ip(mapped), v4(1));
        assert_ne!(RateKey::ip("::c000:201".parse().unwrap()), v4(1));
        for (key, shown) in [
            (v4(1), "RateKey::Ipv4"),
            (RateKey::ip(a), "RateKey::Ipv6Prefix"),
            (RateKey::subject("secret-user"), "RateKey::Subject"),
            (RateKey::unknown(), "RateKey::Unknown"),
        ] {
            assert_eq!(format!("{key:?}"), shown);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_full_buckets_are_evicted() {
        let limiter = limiter(
            KeyedLimit::new(2, Duration::from_secs(10)).with_idle_ttl(Duration::from_secs(30)),
        );
        limiter.try_acquire_n(&v4(1), 2).unwrap();
        tokio::time::advance(Duration::from_secs(20)).await;
        limiter.try_acquire(&v4(2)).unwrap();
        assert_eq!(limiter.len(), 2);
        tokio::time::advance(Duration::from_secs(10)).await;
        limiter.try_acquire(&v4(3)).unwrap();
        assert_eq!(limiter.len(), 2, "the first key was idle for 30 s");

        let short = self::limiter(KeyedLimit::new(1, Duration::from_mins(1)).with_idle_ttl(SECOND));
        short.try_acquire(&v4(1)).unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        short.try_acquire(&v4(2)).unwrap();
        assert_eq!(short.len(), 2, "an empty bucket stays until it is full");
        assert!(short.try_acquire(&v4(1)).is_err());
        tokio::time::advance(Duration::from_mins(1)).await;
        short.try_acquire(&v4(3)).unwrap();
        assert_eq!(short.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn at_max_keys_the_longest_unused_key_goes() {
        let limiter = limiter(KeyedLimit::new(1, Duration::from_mins(1)).with_max_keys(2));
        limiter.try_acquire(&v4(1)).unwrap();
        tokio::time::advance(SECOND).await;
        limiter.try_acquire(&v4(2)).unwrap();
        tokio::time::advance(SECOND).await;
        assert!(limiter.try_acquire(&v4(1)).is_err(), "a refusal is a use");
        limiter.try_acquire(&v4(3)).unwrap();
        assert_eq!(limiter.len(), 2);
        assert!(limiter.try_acquire(&v4(1)).is_err(), "still tracked");
        limiter.try_acquire(&v4(2)).unwrap();
    }

    #[test]
    fn a_refusal_is_a_rate_limited_resource_exhausted() {
        let error = AppError::from(RateLimited {
            retry_after: Duration::from_secs(3),
        });
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(error.reason(), Some(CLIENT_RATE_LIMITED));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
    }

    #[test]
    fn client_keys_parse_and_pick_their_bucket() {
        for key in [
            ClientKey::ClientAddr,
            ClientKey::EndUser,
            ClientKey::EndUserOrClientAddr,
        ] {
            assert_eq!(ClientKey::parse(key.as_str()), Some(key));
        }
        assert_eq!(ClientKey::parse("ip"), None);
        assert!(!ClientKey::ClientAddr.needs_end_user());
        assert!(ClientKey::EndUser.needs_end_user());
        assert!(ClientKey::EndUserOrClientAddr.needs_end_user());

        let addr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let both = CallContext::new()
            .with_subject("user-1")
            .with_client_addr(addr);
        let only_addr = CallContext::new().with_client_addr(addr);
        let nothing = CallContext::new();
        let user = RateKey::subject("user-1");
        let ip = RateKey::ip(addr);
        assert_eq!(ClientKey::ClientAddr.key_of(&both), ip);
        assert_eq!(ClientKey::ClientAddr.key_of(&nothing), RateKey::unknown());
        assert_eq!(ClientKey::EndUser.key_of(&both), user);
        assert_eq!(ClientKey::EndUser.key_of(&only_addr), RateKey::unknown());
        assert_eq!(ClientKey::EndUserOrClientAddr.key_of(&both), user);
        assert_eq!(ClientKey::EndUserOrClientAddr.key_of(&only_addr), ip);
        assert_eq!(
            ClientKey::EndUserOrClientAddr.key_of(&nothing),
            RateKey::unknown()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_limiter_checks_the_context_and_clones_share_buckets() {
        let limiter = ClientLimiter::new(
            KeyedLimit::new(1, Duration::from_mins(1)),
            ClientKey::ClientAddr,
        )
        .unwrap();
        let clone = limiter.clone();
        assert_eq!(limiter, clone);
        assert_eq!(limiter.key(), ClientKey::ClientAddr);
        let other = ClientLimiter::new(
            KeyedLimit::new(1, Duration::from_mins(1)),
            ClientKey::ClientAddr,
        )
        .unwrap();
        assert_ne!(limiter, other);

        let cx = CallContext::new().with_client_addr(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
        limiter.check(&cx).unwrap();
        let error = clone.check(&cx).unwrap_err();
        assert_eq!(error.reason(), Some(CLIENT_RATE_LIMITED));
        assert_eq!(error.retry_after(), Some(Duration::from_mins(1)));
        assert_eq!(limiter.limiter().len(), 1);
        assert!(ClientLimiter::new(KeyedLimit::new(0, SECOND), ClientKey::EndUser).is_err());
    }
}
