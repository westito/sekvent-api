//! A keyed, lazily created, evicting resource cache: [`Keyed`].
//!
//! # Building on it inside the crate
//!
//! [`KeyedBuilder::from_parts`] starts a builder from an internal factory
//!
//! ```text
//! Fn(K, Arc<Tracker>) -> BoxFut<'static, Result<Made<V>, AppError>>
//! ```
//!
//! (`BoxFut` and `Tracker` live in `keyed::task`). Its contract:
//!
//! - It is called once per creation, on the creation's own task, under the
//!   cache's `create_timeout`. Its future is dropped at the deadline, when
//!   `close` cancels it, and unwinds on a panic: whatever it built so far is
//!   released by its own `Drop`. A resource that must be released by an
//!   async step (a pool that must be closed) sits in a guard that, when
//!   dropped armed, hands that step to
//!   `tracker.spawn_retiring(tracker.retire(), work)`: a tracked task,
//!   counted in `retiring`, that `close` and the unit wait for.
//! - Returning `Ok(Made { value, finisher })` hands both to the cache, so the
//!   guard is disarmed right before. From then on the cache guarantees that
//!   `finisher` runs exactly once, after `value` was dropped (or after the
//!   `dispose` hook consumed it), on a tracked task: when the value is
//!   evicted, invalidated or closed and no reference remains, or at once when
//!   the creation turned out stale or the cache closed under it. A last
//!   reference dropped on a thread outside any runtime disposes on the
//!   runtime the value was created on; only once that runtime shut down is
//!   the value dropped with its finisher unrun.
//!
//! `KeyedPools` (`pools.rs`) wraps a `Keyed<K, V>` built this way: its
//! finisher closes the key's pool.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use sekvent_error::{AppError, ErrorCode, WireError};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::reasons::{KEYED_CAPACITY, KEYED_CLOSED, KEYED_CREATE_TIMEOUT, KEYED_FACTORY_PANICKED};

#[cfg(feature = "sqlx-postgres")]
mod pools;
mod task;
#[cfg(feature = "runtime")]
mod unit;

#[cfg(feature = "sqlx-postgres")]
pub use pools::{ConnectCandidates, KeyedPools, KeyedPoolsBuilder};

#[cfg(test)]
mod tests;

use task::{
    BoxFut, Panicked, RetireToken, TaskToken, Tracker, guarded, log_dispose_panic, log_failure,
};

/// What a factory made: the value, and an optional step that runs after the
/// value was dropped at its disposal.
pub(crate) struct Made<V> {
    pub(crate) value: V,
    pub(crate) finisher: Option<BoxFut<'static, ()>>,
}

impl<V> Made<V> {
    /// A value without a finisher.
    pub(crate) fn new(value: V) -> Self {
        Self {
            value,
            finisher: None,
        }
    }
}

type Factory<K, V> =
    Box<dyn Fn(K, Arc<Tracker>) -> BoxFut<'static, Result<Made<V>, AppError>> + Send + Sync>;
type Hook<K, V> = Arc<dyn Fn(K, V) -> BoxFut<'static, ()> + Send + Sync>;
type Release<V> = Box<dyn FnOnce(V) -> BoxFut<'static, ()> + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Settings {
    capacity: usize,
    evict_idle_after: Option<Duration>,
    create_timeout: Duration,
    reap_interval: Duration,
}

impl Settings {
    const DEFAULT: Self = Self {
        capacity: 64,
        evict_idle_after: None,
        create_timeout: Duration::from_secs(30),
        reap_interval: Duration::from_secs(30),
    };
}

/// One lazily created value per key.
///
/// [`get`](Self::get) returns the key's value, creating it with the factory
/// when the cache does not hold it; concurrent callers of one key share one
/// creation, which runs on a task of its own under `create_timeout` and is
/// never cached when it fails. A value is in use while a [`KeyedRef`] to it
/// exists, and an entry in use is never evicted. Values leave the cache for
/// capacity (the least recently used entry nothing refers to), for idleness
/// ([`evict_idle`](Self::evict_idle), the unit of `into_unit`), through
/// [`invalidate`](Self::invalidate) and [`close`](Self::close); each is
/// disposed exactly once, when the cache dropped it and no reference remains.
///
/// Every creation carries a generation: one whose entry `invalidate` or
/// `close` removed meanwhile is stale, its value is disposed unseen, its
/// error is logged and dropped, and its waiters retry with the key's current
/// state.
///
/// Idle time is measured on tokio's clock. Clones share the cache.
pub struct Keyed<K, V> {
    inner: Arc<Shared<K, V>>,
}

impl<K, V> Clone for Keyed<K, V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> fmt::Debug for Keyed<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keyed")
            .field("stats", &self.inner.stats())
            .finish_non_exhaustive()
    }
}

/// Builds a [`Keyed`] cache.
///
/// | Setting | Default | Refused |
/// |---|---|---|
/// | `capacity` | 64 | 0 |
/// | `evict_idle_after` | `None` (never) | `Some(0)` |
/// | `create_timeout` | 30 s | 0 |
/// | `reap_interval` | 30 s | 0 |
pub struct KeyedBuilder<K, V> {
    factory: Factory<K, V>,
    dispose: Option<Hook<K, V>>,
    settings: Settings,
}

impl<K, V> fmt::Debug for KeyedBuilder<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedBuilder")
            .field("capacity", &self.settings.capacity)
            .field("evict_idle_after", &self.settings.evict_idle_after)
            .field("create_timeout", &self.settings.create_timeout)
            .field("reap_interval", &self.settings.reap_interval)
            .finish_non_exhaustive()
    }
}

/// A counted reference to a cached value. While one exists the value is in
/// use: the cache never evicts it.
///
/// A reference from [`Keyed::get`] marks its entry used when it is taken and
/// when it (or a clone) drops; one from [`Keyed::peek`] does not.
pub struct KeyedRef<V> {
    slot: Arc<Slot<V>>,
    counts: bool,
}

impl<V> KeyedRef<V> {
    fn used(slot: Arc<Slot<V>>) -> Self {
        slot.touch();
        Self { slot, counts: true }
    }
}

impl<V> Clone for KeyedRef<V> {
    fn clone(&self) -> Self {
        Self {
            slot: Arc::clone(&self.slot),
            counts: self.counts,
        }
    }
}

impl<V> std::ops::Deref for KeyedRef<V> {
    type Target = V;

    fn deref(&self) -> &V {
        self.slot
            .value
            .as_ref()
            .expect("a referenced value is present until its disposal")
    }
}

impl<V> Drop for KeyedRef<V> {
    fn drop(&mut self) {
        if self.counts {
            self.slot.touch();
        }
    }
}

impl<V> fmt::Debug for KeyedRef<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedRef").finish_non_exhaustive()
    }
}

/// A snapshot of a [`Keyed`] cache.
///
/// `creations`, `create_failures`, `evictions` and `capacity_refusals` only
/// grow; the other fields are gauges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyedStats {
    /// The most entries (warm and being created) the cache holds.
    pub capacity: usize,
    /// Entries holding a value.
    pub warm: usize,
    /// Warm entries with at least one `KeyedRef`, or a waiter about to
    /// receive one.
    pub in_use: usize,
    /// Entries whose value is being created.
    pub creating: usize,
    /// Values out of the cache that are still referenced or being
    /// disposed. Not bounded by `capacity`.
    pub retiring: usize,
    /// Creations started.
    pub creations: u64,
    /// Creations that ended without a value: an error, the deadline, a
    /// panic, or the cache closing under them.
    pub create_failures: u64,
    /// Entries the cache removed on its own, for capacity or idleness.
    pub evictions: u64,
    /// `get`s refused because the cache was full.
    pub capacity_refusals: u64,
}

struct Shared<K, V> {
    state: Mutex<State<K, V>>,
    tracker: Arc<Tracker>,
    factory: Factory<K, V>,
    dispose: Option<Hook<K, V>>,
    settings: Settings,
    /// The origin of every slot's last-use offset.
    epoch: Instant,
    /// Raised once by `close`; every creation task selects on it.
    closing: watch::Sender<bool>,
}

struct State<K, V> {
    map: HashMap<K, Entry<V>>,
    closed: bool,
    next_generation: u64,
    creations: u64,
    create_failures: u64,
    evictions: u64,
    capacity_refusals: u64,
}

enum Entry<V> {
    Creating {
        generation: u64,
        outcome: watch::Receiver<Outcome<V>>,
    },
    Warm(Arc<Slot<V>>),
}

/// What a creation tells its waiters.
enum Outcome<V> {
    Pending,
    Ready(Arc<Slot<V>>),
    Failed(WireError),
    /// The creation was stale: look at the key again.
    Retry,
}

impl<V> Outcome<V> {
    fn is_settled(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One value and what releases it. Only the map, live references and
/// waiters that have not woken yet hold one.
struct Slot<V> {
    /// Taken out only by the disposal, once nothing refers to the slot.
    value: Option<V>,
    release: Mutex<Option<Release<V>>>,
    /// Set when the value leaves the map; lowered when its disposal ends.
    retire: OnceLock<RetireToken>,
    tracker: Arc<Tracker>,
    /// The runtime the value was created on: the disposal runs there when
    /// the last reference drops on a thread outside any runtime.
    runtime: Option<Handle>,
    epoch: Instant,
    /// Nanoseconds from `epoch` to the last use.
    last_use: AtomicU64,
}

impl<V> Slot<V> {
    fn touch(&self) {
        self.touch_at(offset(self.epoch));
    }

    /// Concurrent drops may store out of order; the latest use wins.
    fn touch_at(&self, at: u64) {
        self.last_use.fetch_max(at, Ordering::AcqRel);
    }

    fn last_use(&self) -> u64 {
        self.last_use.load(Ordering::Acquire)
    }

    /// Starts the disposal on a tracked task of its own; `None` when it ran
    /// already or no runtime is left to run it (the value is then just
    /// dropped).
    fn start_disposal(&mut self) -> Option<JoinHandle<()>> {
        let value = self.value.take()?;
        let release = self
            .release
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        let retire = self.retire.take().unwrap_or_else(|| self.tracker.retire());
        self.tracker
            .spawn_retiring_on(self.runtime.as_ref(), retire, release(value))
    }
}

/// A slot taken out of the map, counted as retiring and as a running task
/// from under the map's lock on, so a concurrent `close` that finds the map
/// empty still waits for its disposal to start and end.
struct Leaving<V> {
    slot: Arc<Slot<V>>,
    task: TaskToken,
}

impl<V> Leaving<V> {
    /// Gives up the cache's handle: disposes the value when that was the
    /// last handle, otherwise the last holder's drop does.
    fn release(self) -> Option<JoinHandle<()>> {
        let Self { slot, task } = self;
        let disposal = Arc::into_inner(slot).and_then(|mut slot| slot.start_disposal());
        drop(task);
        disposal
    }
}

impl<V> Drop for Slot<V> {
    fn drop(&mut self) {
        drop(self.start_disposal());
    }
}

/// Nanoseconds from `epoch` to now on tokio's clock.
fn offset(epoch: Instant) -> u64 {
    nanos(Instant::now().saturating_duration_since(epoch))
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn closed_error() -> AppError {
    AppError::unavailable("the resource cache is closed").with_reason(KEYED_CLOSED)
}

fn capacity_error() -> AppError {
    AppError::resource_exhausted("the resource cache is full").with_reason(KEYED_CAPACITY)
}

fn timeout_error() -> AppError {
    AppError::unavailable("creating the resource took too long").with_reason(KEYED_CREATE_TIMEOUT)
}

fn panicked_error() -> AppError {
    AppError::new(ErrorCode::Internal, "internal error").with_reason(KEYED_FACTORY_PANICKED)
}

impl<K, V> Shared<K, V> {
    fn lock(&self) -> MutexGuard<'_, State<K, V>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stats(&self) -> KeyedStats {
        let state = self.lock();
        let (mut warm, mut in_use, mut creating) = (0, 0, 0);
        for entry in state.map.values() {
            match entry {
                Entry::Warm(slot) => {
                    warm += 1;
                    if Arc::strong_count(slot) > 1 {
                        in_use += 1;
                    }
                }
                Entry::Creating { .. } => creating += 1,
            }
        }
        KeyedStats {
            capacity: self.settings.capacity,
            warm,
            in_use,
            creating,
            retiring: self.tracker.retiring(),
            creations: state.creations,
            create_failures: state.create_failures,
            evictions: state.evictions,
            capacity_refusals: state.capacity_refusals,
        }
    }

    /// Counts a slot that just left the map in. Called under the map's lock.
    fn leave(&self, slot: Arc<Slot<V>>) -> Leaving<V> {
        let _ = slot.retire.set(self.tracker.retire());
        Leaving {
            slot,
            task: self.tracker.task(false),
        }
    }
}

impl<K, V> Shared<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// What disposes a value of `key`: the hook (or a drop), then the
    /// finisher.
    fn release(&self, key: K, finisher: Option<BoxFut<'static, ()>>) -> Release<V> {
        let hook = self.dispose.clone();
        Box::new(move |value| -> BoxFut<'static, ()> {
            Box::pin(run_disposal(hook, key, value, finisher))
        })
    }

    fn slot(&self, key: K, made: Made<V>) -> Slot<V> {
        Slot {
            value: Some(made.value),
            release: Mutex::new(Some(self.release(key, made.finisher))),
            retire: OnceLock::new(),
            tracker: Arc::clone(&self.tracker),
            runtime: Handle::try_current().ok(),
            epoch: self.epoch,
            last_use: AtomicU64::new(offset(self.epoch)),
        }
    }

    /// Disposes a value nobody ever received.
    fn dispose_unpublished(&self, key: K, made: Made<V>) {
        let work = self.release(key, made.finisher)(made.value);
        drop(self.tracker.spawn_retiring(self.tracker.retire(), work));
    }

    /// Releases a value nobody received whose key could not be cloned: the
    /// value is dropped and the finisher runs, without the hook.
    fn finish_without_hook(&self, made: Made<V>) {
        let Made { value, finisher } = made;
        let work = async move {
            let dropped = guarded(move || {
                drop(value);
                std::future::ready(())
            })
            .await;
            if dropped.is_err() {
                log_dispose_panic();
            }
            if let Some(finisher) = finisher
                && guarded(move || finisher).await.is_err()
            {
                log_dispose_panic();
            }
        };
        drop(self.tracker.spawn_retiring(self.tracker.retire(), work));
    }
}

/// Runs the hook on the value (or drops it), then the finisher; a panic of
/// either is logged and the other still runs.
async fn run_disposal<K, V>(
    hook: Option<Hook<K, V>>,
    key: K,
    value: V,
    finisher: Option<BoxFut<'static, ()>>,
) {
    let disposed = match hook {
        Some(hook) => guarded(move || hook(key, value)).await,
        None => {
            guarded(move || {
                drop((key, value));
                std::future::ready(())
            })
            .await
        }
    };
    if disposed.is_err() {
        log_dispose_panic();
    }
    if let Some(finisher) = finisher
        && guarded(move || finisher).await.is_err()
    {
        log_dispose_panic();
    }
}

/// What a caller of `get` found under the lock.
enum Found<V> {
    Warm(KeyedRef<V>),
    Wait(watch::Receiver<Outcome<V>>),
}

impl<K, V> Keyed<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// Start a cache whose values `factory` creates.
    pub fn builder<F, Fut>(factory: F) -> KeyedBuilder<K, V>
    where
        F: Fn(K) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<V, AppError>> + Send + 'static,
    {
        KeyedBuilder::from_parts(
            move |key, _tracker| -> BoxFut<'static, Result<Made<V>, AppError>> {
                let creating = factory(key);
                Box::pin(async move { creating.await.map(Made::new) })
            },
        )
    }

    /// The value of `key`, created if the cache does not hold it.
    ///
    /// Concurrent callers of one key share one creation; dropping this
    /// future never cancels it. Fails with `UNAVAILABLE` / `KEYED_CLOSED`
    /// after [`close`](Self::close), `RESOURCE_EXHAUSTED` / `KEYED_CAPACITY`
    /// when every entry is in use or being created, `UNAVAILABLE` /
    /// `KEYED_CREATE_TIMEOUT` when the factory outlived `create_timeout`,
    /// `INTERNAL` / `KEYED_FACTORY_PANICKED` when it panicked, and with the
    /// caller-visible part of the factory's own error (no source).
    pub async fn get(&self, key: &K) -> Result<KeyedRef<V>, AppError> {
        loop {
            let outcome = match self.find_or_start(key)? {
                Found::Warm(reference) => return Ok(reference),
                Found::Wait(outcome) => outcome,
            };
            match settled(outcome).await {
                Outcome::Ready(slot) => return Ok(KeyedRef::used(slot)),
                Outcome::Failed(wire) => return Err(AppError::from_wire(wire)),
                Outcome::Pending | Outcome::Retry => {}
            }
        }
    }

    fn find_or_start(&self, key: &K) -> Result<Found<V>, AppError> {
        let shared = &self.inner;
        let mut state = shared.lock();
        if state.closed {
            return Err(closed_error());
        }
        match state.map.get(key) {
            Some(Entry::Warm(slot)) => return Ok(Found::Warm(KeyedRef::used(Arc::clone(slot)))),
            Some(Entry::Creating { outcome, .. }) if outcome.has_changed().is_ok() => {
                return Ok(Found::Wait(outcome.clone()));
            }
            Some(Entry::Creating { .. }) => {
                // Its creation ended without settling it: nobody ever will.
                drop(state.map.remove(key));
            }
            None => {}
        }
        let evicted = if state.map.len() >= shared.settings.capacity {
            let Some(victim) = least_recently_used(&state.map) else {
                state.capacity_refusals += 1;
                return Err(capacity_error());
            };
            state.evictions += 1;
            match state.map.remove(&victim) {
                Some(Entry::Warm(slot)) => Some(shared.leave(slot)),
                _ => None,
            }
        } else {
            None
        };
        // The key's own code (`Clone`, `Hash`, `Eq`) may panic: it all runs
        // before the entry exists, so a panic leaves no entry behind that
        // no creation would ever settle.
        let entry_key = key.clone();
        let creation_key = key.clone();
        let generation = state.next_generation;
        let (sender, outcome) = watch::channel(Outcome::Pending);
        state.map.insert(
            entry_key,
            Entry::Creating {
                generation,
                outcome: outcome.clone(),
            },
        );
        state.next_generation += 1;
        state.creations += 1;
        let creation = Creation {
            shared: Arc::clone(shared),
            key: creation_key,
            generation,
            sender: Some(sender),
            _task: shared.tracker.task(true),
        };
        drop(state);
        if let Some(evicted) = evicted {
            drop(evicted.release());
        }
        tokio::spawn(async move {
            let mut creation = creation;
            let end = creation.run().await;
            creation.settle(end);
        });
        Ok(Found::Wait(outcome))
    }

    /// The value of `key` if the cache holds one. Never creates, never
    /// waits, and does not count as a use.
    ///
    /// `None` when the key is absent or being created, or the cache is
    /// closed. While the returned reference exists the value is in use.
    pub fn peek(&self, key: &K) -> Option<KeyedRef<V>> {
        let state = self.inner.lock();
        if state.closed {
            return None;
        }
        match state.map.get(key) {
            Some(Entry::Warm(slot)) => Some(KeyedRef {
                slot: Arc::clone(slot),
                counts: false,
            }),
            _ => None,
        }
    }

    /// Drop the cache's value of `key`; whether there was one.
    ///
    /// A warm value nothing refers to is disposed before this returns; a
    /// referenced one retires and is disposed when its last reference drops.
    /// A creation in flight is not cancelled: it runs to its end and what it
    /// produces is discarded. Either way the next `get` creates afresh.
    /// Meant for an operator's action (a credential change, a restore), not
    /// for every request.
    pub async fn invalidate(&self, key: &K) -> bool {
        let leaving = {
            let mut state = self.inner.lock();
            match state.map.remove(key) {
                None => return false,
                Some(Entry::Creating { .. }) => return true,
                Some(Entry::Warm(slot)) => self.inner.leave(slot),
            }
        };
        if let Some(disposal) = leaving.release() {
            let _ = disposal.await;
        }
        true
    }

    /// Evict every entry that was idle for `evict_idle_after`; how many.
    /// Evicts nothing when that setting is `None`.
    pub async fn evict_idle(&self) -> usize {
        match self.inner.settings.evict_idle_after {
            Some(idle) => self.evict_idle_older_than(idle).await,
            None => 0,
        }
    }

    /// Evict every entry that nothing refers to and that was last used at
    /// least `idle` ago; how many, once their disposal finished.
    ///
    /// `Duration::ZERO` evicts every entry not in use, whatever the clock
    /// says.
    pub async fn evict_idle_older_than(&self, idle: Duration) -> usize {
        let (evictions, disposals) = self.sweep(idle);
        for disposal in disposals {
            let _ = disposal.await;
        }
        evictions
    }

    /// The periodic sweep of the unit: [`evict_idle`](Self::evict_idle)
    /// without waiting for the disposals, which run on their own tracked
    /// tasks, so one slow disposal never stalls the next sweep.
    #[cfg_attr(not(feature = "runtime"), allow(dead_code))]
    pub(crate) fn evict_idle_in_background(&self) {
        if let Some(idle) = self.inner.settings.evict_idle_after {
            drop(self.sweep(idle));
        }
    }

    /// Evicts the idle entries and starts their disposals: how many, and
    /// the disposals to wait for.
    fn sweep(&self, idle: Duration) -> (usize, Vec<JoinHandle<()>>) {
        let idle = nanos(idle);
        let leaving: Vec<Leaving<V>> = {
            let mut state = self.inner.lock();
            let now = offset(self.inner.epoch);
            let evicted: Vec<_> = state
                .map
                .extract_if(|_, entry| match entry {
                    Entry::Warm(slot) => {
                        Arc::strong_count(slot) == 1 && now.saturating_sub(slot.last_use()) >= idle
                    }
                    Entry::Creating { .. } => false,
                })
                .filter_map(|(_, entry)| match entry {
                    Entry::Warm(slot) => Some(self.inner.leave(slot)),
                    Entry::Creating { .. } => None,
                })
                .collect();
            state.evictions += count(evicted.len());
            evicted
        };
        let evictions = leaving.len();
        let disposals = leaving.into_iter().filter_map(Leaving::release).collect();
        (evictions, disposals)
    }

    /// Drop every value, cancel every creation and refuse further `get`s.
    ///
    /// Unreferenced values are disposed, referenced ones retire and are
    /// disposed when their last reference drops; creations in flight are
    /// cancelled and their waiters get `KEYED_CLOSED`. Returns once no
    /// creation task is running and no disposal is in progress; it does not
    /// wait for values that are still referenced. It has no bound of its
    /// own: wrap it in `tokio::time::timeout` for one (giving up abandons the
    /// wait and nothing else). Idempotent.
    pub async fn close(&self) {
        let leaving: Vec<_> = {
            let mut state = self.inner.lock();
            state.closed = true;
            state
                .map
                .drain()
                .filter_map(|(_, entry)| match entry {
                    Entry::Warm(slot) => Some(self.inner.leave(slot)),
                    Entry::Creating { .. } => None,
                })
                .collect()
        };
        self.inner.closing.send_replace(true);
        for leaving in leaving {
            drop(leaving.release());
        }
        self.inner.tracker.wait_idle().await;
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.inner.lock().closed
    }

    /// A snapshot of the cache's gauges and counters.
    pub fn stats(&self) -> KeyedStats {
        self.inner.stats()
    }
}

/// The warm entry nothing refers to with the oldest last use.
fn least_recently_used<K: Clone, V>(map: &HashMap<K, Entry<V>>) -> Option<K> {
    map.iter()
        .filter_map(|(key, entry)| match entry {
            Entry::Warm(slot) if Arc::strong_count(slot) == 1 => Some((key, slot.last_use())),
            _ => None,
        })
        .min_by_key(|(_, last_use)| *last_use)
        .map(|(key, _)| key.clone())
}

/// Waits for a creation's outcome. A creation that vanished without one
/// failed in the cache's own code: that is terminal, never a retry, so a
/// waiter cannot spin on an entry nobody settles.
async fn settled<V>(mut outcome: watch::Receiver<Outcome<V>>) -> Outcome<V> {
    let Ok(settled) = outcome.wait_for(Outcome::is_settled).await else {
        return Outcome::Failed(panicked_error().to_wire());
    };
    match &*settled {
        Outcome::Ready(slot) => Outcome::Ready(Arc::clone(slot)),
        Outcome::Failed(wire) => Outcome::Failed(wire.clone()),
        Outcome::Pending | Outcome::Retry => Outcome::Retry,
    }
}

/// Resolves once `close` was called.
async fn closing(mut closing: watch::Receiver<bool>) {
    let _ = closing.wait_for(|closed| *closed).await;
}

/// How a creation ended.
enum End<V> {
    Made(Made<V>),
    Failed(AppError),
    /// Cancelled by `close`, or its task was dropped: the waiters look at
    /// the key again, which after `close` fails with `KEYED_CLOSED`.
    Abandoned,
}

/// One creation in flight. Settles its entry by the generation rule and
/// sends its outcome exactly once, from [`settle`](Self::settle) or, when
/// its task is dropped or unwinds before that, from `Drop`. The factory's
/// own panics never unwind here: [`guarded`] turns them into an error.
struct Creation<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    shared: Arc<Shared<K, V>>,
    key: K,
    generation: u64,
    sender: Option<watch::Sender<Outcome<V>>>,
    _task: TaskToken,
}

impl<K, V> Creation<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    async fn run(&self) -> End<V> {
        let shared = &self.shared;
        let closed = closing(shared.closing.subscribe());
        let made = guarded(|| (shared.factory)(self.key.clone(), Arc::clone(&shared.tracker)));
        let timed = tokio::time::timeout(shared.settings.create_timeout, made);
        tokio::select! {
            biased;
            () = closed => End::Abandoned,
            result = timed => match result {
                Ok(Ok(Ok(made))) => End::Made(made),
                Ok(Ok(Err(error))) => End::Failed(error),
                Ok(Err(Panicked)) => End::Failed(panicked_error()),
                Err(_elapsed) => End::Failed(timeout_error()),
            },
        }
    }

    /// Settles the entry and sends the outcome. A panic of the key's own
    /// code (`Clone`, `Hash`, `Eq`) while settling still removes this
    /// creation's entry and sends a terminal failure, so the key never
    /// stays wedged behind an entry nobody settles.
    fn settle(&mut self, end: End<V>) {
        let Some(sender) = self.sender.take() else {
            return;
        };
        let mut end = Some(end);
        let outcome = catch_unwind(AssertUnwindSafe(|| self.settle_entry(&mut end)))
            .unwrap_or_else(|_| self.settle_after_panic(end.take()));
        sender.send_replace(outcome);
    }

    /// A value is published or disposed with no call into the key after
    /// `end` is taken, so a panic of the key never loses a value: `end` is
    /// left for [`settle_after_panic`](Self::settle_after_panic).
    fn settle_entry(&self, end: &mut Option<End<V>>) -> Outcome<V> {
        let shared = &self.shared;
        let key = self.key.clone();
        let mut guard = shared.lock();
        let state = &mut *guard;
        let closed = state.closed;
        let entry = if closed {
            None
        } else {
            state.map.get_mut(&self.key).filter(|entry| {
                matches!(entry, Entry::Creating { generation, .. } if *generation == self.generation)
            })
        };
        let current = entry.is_some();
        let Some(end) = end.take() else {
            return Outcome::Retry;
        };
        match end {
            End::Made(made) => {
                if let Some(entry) = entry {
                    let slot = Arc::new(shared.slot(key, made));
                    let replaced = std::mem::replace(entry, Entry::Warm(Arc::clone(&slot)));
                    drop(guard);
                    drop(replaced);
                    return Outcome::Ready(slot);
                }
                if closed {
                    state.create_failures += 1;
                }
                drop(guard);
                shared.dispose_unpublished(key, made);
                Outcome::Retry
            }
            End::Failed(error) => {
                let removed = current.then(|| state.map.remove(&self.key));
                state.create_failures += 1;
                drop(guard);
                drop(removed);
                log_failure(&error);
                if current {
                    Outcome::Failed(error.to_wire())
                } else {
                    Outcome::Retry
                }
            }
            End::Abandoned => {
                let removed = current.then(|| state.map.remove(&self.key));
                state.create_failures += 1;
                drop(guard);
                drop(removed);
                Outcome::Retry
            }
        }
    }

    /// The key panicked while the creation settled: remove this creation's
    /// entry by its generation alone (no call into the key), release what
    /// it made without the hook (which needs the key), and fail its waiters
    /// with `KEYED_FACTORY_PANICKED`.
    fn settle_after_panic(&self, end: Option<End<V>>) -> Outcome<V> {
        let shared = &self.shared;
        let mut state = shared.lock();
        state.create_failures += 1;
        let generation = self.generation;
        let removed: Vec<_> = state
            .map
            .extract_if(|_, entry| {
                matches!(entry, Entry::Creating { generation: own, .. } if *own == generation)
            })
            .collect();
        drop(state);
        let _ = catch_unwind(AssertUnwindSafe(move || drop(removed)));
        if let Some(End::Made(made)) = end {
            shared.finish_without_hook(made);
        }
        let error = panicked_error();
        log_failure(&error);
        Outcome::Failed(error.to_wire())
    }
}

impl<K, V> Drop for Creation<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.settle(End::Abandoned);
    }
}

impl<K, V> KeyedBuilder<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// A builder over the crate's internal factory: see the module docs.
    pub(crate) fn from_parts<F>(factory: F) -> Self
    where
        F: Fn(K, Arc<Tracker>) -> BoxFut<'static, Result<Made<V>, AppError>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            factory: Box::new(factory),
            dispose: None,
            settings: Settings::DEFAULT,
        }
    }

    /// Every setting at once, for a cache built on this one.
    #[cfg_attr(not(feature = "sqlx-postgres"), allow(dead_code))]
    fn settings(mut self, settings: Settings) -> Self {
        self.settings = settings;
        self
    }

    /// The most entries, warm and being created, the cache holds (default
    /// 64). A new key over it evicts the least recently used entry nothing
    /// refers to, or fails with `KEYED_CAPACITY`.
    #[must_use]
    pub fn capacity(mut self, entries: usize) -> Self {
        self.settings.capacity = entries;
        self
    }

    /// How long an entry nothing refers to stays before
    /// [`Keyed::evict_idle`] evicts it (default `None`: never).
    #[must_use]
    pub fn evict_idle_after(mut self, idle: Option<Duration>) -> Self {
        self.settings.evict_idle_after = idle;
        self
    }

    /// How long one creation may take (default 30 s); past it the factory's
    /// future is dropped and the waiters get `KEYED_CREATE_TIMEOUT`.
    #[must_use]
    pub fn create_timeout(mut self, timeout: Duration) -> Self {
        self.settings.create_timeout = timeout;
        self
    }

    /// How often the unit of `into_unit` evicts idle entries, as
    /// [`Keyed::evict_idle`] does (default 30 s).
    #[must_use]
    pub fn reap_interval(mut self, interval: Duration) -> Self {
        self.settings.reap_interval = interval;
        self
    }

    /// How a value is released once the cache dropped it and nothing
    /// refers to it any more. Runs exactly once per value the factory
    /// returned, on a task of its own; a panic in it is logged and ignored.
    /// Without it the value is dropped.
    #[must_use]
    pub fn dispose<F, Fut>(mut self, dispose: F) -> Self
    where
        F: Fn(K, V) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.dispose = Some(Arc::new(move |key, value| -> BoxFut<'static, ()> {
            Box::pin(dispose(key, value))
        }));
        self
    }

    /// The cache. `INVALID_ARGUMENT`, naming the setting, for a zero
    /// `capacity`, `create_timeout` or `reap_interval` and for
    /// `evict_idle_after(Some(Duration::ZERO))`.
    pub fn build(self) -> Result<Keyed<K, V>, AppError> {
        let settings = self.settings;
        if settings.capacity == 0 {
            return Err(AppError::invalid_argument("capacity must be at least 1"));
        }
        if settings.evict_idle_after == Some(Duration::ZERO) {
            return Err(AppError::invalid_argument(
                "evict_idle_after must be longer than zero",
            ));
        }
        if settings.create_timeout.is_zero() {
            return Err(AppError::invalid_argument(
                "create_timeout must be longer than zero",
            ));
        }
        if settings.reap_interval.is_zero() {
            return Err(AppError::invalid_argument(
                "reap_interval must be longer than zero",
            ));
        }
        let (closing, _) = watch::channel(false);
        Ok(Keyed {
            inner: Arc::new(Shared {
                state: Mutex::new(State {
                    map: HashMap::new(),
                    closed: false,
                    next_generation: 0,
                    creations: 0,
                    create_failures: 0,
                    evictions: 0,
                    capacity_refusals: 0,
                }),
                tracker: Arc::default(),
                factory: self.factory,
                dispose: self.dispose,
                settings,
                epoch: Instant::now(),
                closing,
            }),
        })
    }
}
