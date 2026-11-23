//! The lookup cache of the registry, the lookup admission and the reads in flight.
//!
//! A request's lookup is answered by a fresh cache entry, by joining a read of the same key that is
//! already running, or by a read of its own under a lookup permit taken without waiting. A read runs
//! on a task of its own, so a caller that goes away cancels nothing and frees nothing early.
//!
//! An entry found not fresh is removed at once, so it is never served later; an answer that is no
//! longer fresh when its read ends is not stored; and an entry is replaced only by the answer of a
//! read that started later than the read that made it.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use sekvent_context::Clock;
use sekvent_error::AppError;
use sqlx::PgPool;
use tokio::sync::watch;

use super::admission::{LookupAdmission, LookupPermit};
use super::rows::{self, TenantRow};
use crate::error::serving;
use crate::host::{canonical_host, is_loopback_host};
use crate::label::{LabelRules, path_label_host};
use crate::{Selector, TenantId};

/// Found entries per key space.
const FOUND_ENTRIES: usize = 4_096;

/// Not-found entries per key space.
const MISSING_ENTRIES: usize = 1_024;

/// How long an entry is served, counted from the start of the read that produced it.
const LIFETIME: Duration = Duration::from_secs(30);

/// How many reads of others a caller joins before it gives up.
const MAX_JOINS: u32 = 2;

/// One statement a lookup runs on the control database.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Read {
    /// The row of a tenant id.
    Id(TenantId),
    /// The row of the tenant that holds exactly this canonical host.
    Host(String),
    /// The one live tenant of this lowered label with a loopback host.
    LoopbackLabel(String),
}

/// What a read returns.
type ReadFuture = Pin<Box<dyn Future<Output = Result<Option<TenantRow>, AppError>> + Send>>;

/// Makes the read of one statement; the control pool in production, a fake in the unit tests.
type Reader<'a> = &'a (dyn Fn(Read) -> ReadFuture + Send + Sync);

/// The key of a cached answer and of a read in flight.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    /// The id space.
    Id(TenantId),
    /// The host space: an exact canonical host.
    Host(String),
}

impl Key {
    /// The statement that answers this key.
    fn read(&self) -> Read {
        match self {
            Self::Id(id) => Read::Id(*id),
            Self::Host(host) => Read::Host(host.clone()),
        }
    }
}

/// How a selector is answered.
enum Plan {
    /// No tenant, without a read.
    Nothing,
    /// Through the cache.
    Cached(Key),
    /// By a read that is never cached and never joined.
    Uncached(Read),
}

/// The plan of `selector`; pure, no I/O. A path segment `labels` refuses selects nothing on every
/// listener host; a loopback listener reads by the label `labels` returns.
fn plan(selector: &Selector, labels: &LabelRules) -> Plan {
    match selector {
        Selector::Id(id) => Plan::Cached(Key::Id(*id)),
        Selector::Host(host) => {
            if is_canonical(host) {
                Plan::Cached(Key::Host(host.clone()))
            } else {
                Plan::Nothing
            }
        }
        Selector::PathLabel {
            segment,
            listener_host,
        } => {
            let Ok(label) = labels.canonical(segment) else {
                return Plan::Nothing;
            };
            if let Some(host) = path_label_host(&label, listener_host) {
                Plan::Cached(Key::Host(host))
            } else if is_canonical(listener_host) && is_loopback_host(listener_host) {
                Plan::Uncached(Read::LoopbackLabel(label))
            } else {
                Plan::Nothing
            }
        }
    }
}

/// Whether `host` is exactly its canonical spelling.
fn is_canonical(host: &str) -> bool {
    canonical_host(host).is_ok_and(|canonical| canonical == host)
}

/// Whether an entry made by a read that started at `start` may be served at `now`:
/// `start <= now < start + LIFETIME`. A clock set back never extends a lifetime.
fn is_fresh(start: SystemTime, now: SystemTime) -> bool {
    now.duration_since(start).is_ok_and(|age| age < LIFETIME)
}

/// One cached answer.
struct Entry<V> {
    /// The start of the read that produced it.
    start: SystemTime,
    /// The answer.
    value: V,
}

/// The found or the not-found entries of one key space.
struct Partition<K, V> {
    /// The most entries it holds.
    capacity: usize,
    /// The entries, fresh or not.
    entries: HashMap<K, Entry<V>>,
}

impl<K: Eq + Hash, V> Partition<K, V> {
    /// An empty partition of `capacity` entries.
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
        }
    }

    /// The value of `key` when its entry is fresh at `now`. An entry that is not fresh at `now`
    /// (expired, or dated after `now` because the clock was set back) is removed, so it is never
    /// served later, whatever the clock does.
    fn fresh(&mut self, key: &K, now: SystemTime) -> Option<&V> {
        let fresh = is_fresh(self.entries.get(key)?.start, now);
        if fresh {
            self.entries.get(key).map(|entry| &entry.value)
        } else {
            self.entries.remove(key);
            None
        }
    }

    /// The start of the read that made the entry of `key`, fresh or not.
    fn start_of(&self, key: &K) -> Option<SystemTime> {
        self.entries.get(key).map(|entry| entry.start)
    }

    /// Stores `value` under `key`. A new key in a full partition first drops the entries that are
    /// not fresh at `now`, then, when the partition is still full, every entry of this partition.
    fn insert(&mut self, key: K, value: V, start: SystemTime, now: SystemTime) {
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            self.entries.retain(|_, entry| is_fresh(entry.start, now));
            if self.entries.len() >= self.capacity {
                self.entries.clear();
            }
        }
        self.entries.insert(key, Entry { start, value });
    }

    /// Drops the entry of `key`.
    fn remove(&mut self, key: &K) {
        self.entries.remove(key);
    }
}

/// A fresh cache entry.
enum Hit {
    /// The row.
    Found(Box<TenantRow>),
    /// No row.
    Missing,
}

impl Hit {
    /// The answer of the entry.
    fn into_answer(self) -> Option<TenantRow> {
        match self {
            Self::Found(row) => Some(*row),
            Self::Missing => None,
        }
    }
}

/// One key space: its found and its not-found partitions. A key is in at most one of them.
struct Space<K> {
    /// The rows found.
    found: Partition<K, TenantRow>,
    /// The keys without a row.
    missing: Partition<K, ()>,
}

impl<K: Eq + Hash> Space<K> {
    /// An empty space of the fixed sizes.
    fn new() -> Self {
        Self {
            found: Partition::new(FOUND_ENTRIES),
            missing: Partition::new(MISSING_ENTRIES),
        }
    }

    /// The fresh entry of `key` at `now`; an entry of the key that is not fresh is removed.
    fn answer(&mut self, key: &K, now: SystemTime) -> Option<Hit> {
        if let Some(row) = self.found.fresh(key, now) {
            return Some(Hit::Found(Box::new(row.clone())));
        }
        self.missing.fresh(key, now).map(|()| Hit::Missing)
    }

    /// The start of the read that made the entry of `key`, in either partition.
    fn start_of(&self, key: &K) -> Option<SystemTime> {
        self.found
            .start_of(key)
            .or_else(|| self.missing.start_of(key))
    }

    /// Stores the answer of a read that started at `start`, unless the key holds an entry made by
    /// a read that started at or after `start`: an older observation never replaces a newer one,
    /// and on a tie the stored entry stays. A found entry replaces a not-found entry of the key and
    /// the reverse.
    fn store(&mut self, key: K, answer: Option<TenantRow>, start: SystemTime, now: SystemTime) {
        if self.start_of(&key).is_some_and(|stored| stored >= start) {
            return;
        }
        if let Some(row) = answer {
            self.missing.remove(&key);
            self.found.insert(key, row, start, now);
        } else {
            self.found.remove(&key);
            self.missing.insert(key, (), start, now);
        }
    }

    /// Drops every entry.
    fn clear(&mut self) {
        self.found.entries.clear();
        self.missing.entries.clear();
    }
}

/// A read of a cached key that is running.
struct Flight {
    /// Tells this read from a later read of the same key.
    id: u64,
    /// Ends, for every joiner, when the read ends, however it ends.
    done: watch::Receiver<()>,
}

/// The cache and the reads in flight, behind one mutex locked only for short sections that never
/// await.
struct State {
    /// Raised by `forget` and `clear`; a read stores its answer only under the epoch it started in.
    epoch: u64,
    /// The id space.
    ids: Space<TenantId>,
    /// The host space.
    hosts: Space<String>,
    /// The reads of cached keys that are running.
    flights: HashMap<Key, Flight>,
    /// The id of the next flight.
    next_flight: u64,
}

impl State {
    /// An empty cache.
    fn new() -> Self {
        Self {
            epoch: 0,
            ids: Space::new(),
            hosts: Space::new(),
            flights: HashMap::new(),
            next_flight: 0,
        }
    }

    /// The fresh entry of `key` at `now`; an entry of the key that is not fresh is removed.
    fn answer(&mut self, key: &Key, now: SystemTime) -> Option<Hit> {
        match key {
            Key::Id(id) => self.ids.answer(id, now),
            Key::Host(host) => self.hosts.answer(host, now),
        }
    }

    /// Stores the answer of a read of `key` that started at `start`. A host read that found a row
    /// also stores the row in the id space; an id read stores nothing in the host space. Each key
    /// follows `Space::store` on its own: the id entry is kept when a read that started at or after
    /// this one made it, and the host entry is still stored under its key by the same rule.
    fn store(&mut self, key: &Key, answer: Option<TenantRow>, start: SystemTime, now: SystemTime) {
        match key {
            Key::Id(id) => self.ids.store(*id, answer, start, now),
            Key::Host(host) => {
                if let Some(row) = &answer {
                    self.ids.store(row.info.id, Some(row.clone()), start, now);
                }
                self.hosts.store(host.clone(), answer, start, now);
            }
        }
    }

    /// Drops the tenant's id entry, every found host entry of the tenant and every not-found host
    /// entry; raises the epoch.
    fn forget(&mut self, tenant: TenantId) {
        self.ids.found.remove(&tenant);
        self.ids.missing.remove(&tenant);
        self.hosts
            .found
            .entries
            .retain(|_, entry| entry.value.info.id != tenant);
        self.hosts.missing.entries.clear();
        self.epoch += 1;
    }

    /// Drops every entry; raises the epoch.
    fn clear(&mut self) {
        self.ids.clear();
        self.hosts.clear();
        self.epoch += 1;
    }

    /// Registers a read of `key`: its flight id and the sender whose drop ends the joiners' wait.
    fn start_flight(&mut self, key: Key) -> (u64, watch::Sender<()>) {
        let id = self.next_flight;
        self.next_flight += 1;
        let (done, waiters) = watch::channel(());
        self.flights.insert(key, Flight { id, done: waiters });
        (id, done)
    }
}

/// What every clone of a `Lookups` shares.
struct Shared {
    /// The time entries are made and checked by.
    clock: Arc<dyn Clock>,
    /// The lookup permits.
    admission: LookupAdmission,
    /// The cache and the reads in flight.
    state: Mutex<State>,
}

impl Shared {
    /// The locked state; a poisoned lock is still used, the state is consistent between sections.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores `found` as the answer of `store.key` when the epoch is still `store.epoch` and the
    /// answer is still fresh now. An answer already past its lifetime (or dated after now) is not
    /// stored: it could never be served, and storing it could clear a full partition for nothing.
    fn store(&self, store: &Store, found: Option<&TenantRow>) {
        let mut state = self.state();
        let now = self.clock.now();
        if state.epoch == store.epoch && is_fresh(store.start, now) {
            state.store(&store.key, found.cloned(), store.start, now);
        }
    }
}

/// Where and under which epoch a read's answer is stored.
struct Store {
    /// The key the read answers.
    key: Key,
    /// When the read started.
    start: SystemTime,
    /// The epoch the read started in.
    epoch: u64,
}

/// A running read's hold on its permit and, for a cached key, its place among the reads in flight.
/// Dropping it, also when the read panicked, gives the permit back, then removes that place (its own
/// only, never a later read's), and then ends the joiners' wait.
struct Running {
    /// The state the place is in.
    shared: Arc<Shared>,
    /// The key and the flight id of the place; `None` for a read that is never joined.
    flight: Option<(Key, u64)>,
    /// The lookup permit, given back first.
    permit: Option<LookupPermit>,
    /// Dropped last: the joiners' wait ends.
    done: Option<watch::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        // The permit goes back while the place is still listed, so a caller of the same key in
        // between joins this read (woken below, as its sender is still held) rather than finding
        // neither a read to join nor a free permit. The joiners are woken only once the place is
        // gone, so none of them can join this read again.
        drop(self.permit.take());
        if let Some((key, id)) = self.flight.take() {
            let mut state = self.shared.state();
            if state
                .flights
                .get(&key)
                .is_some_and(|flight| flight.id == id)
            {
                state.flights.remove(&key);
            }
        }
        drop(self.done.take());
    }
}

/// What a caller does after looking at the cache.
enum Next {
    /// Wait for the running read of the key, then look again.
    Join(watch::Receiver<()>),
    /// Lead a read of its own.
    Lead {
        /// The lookup permit.
        permit: LookupPermit,
        /// The flight id.
        id: u64,
        /// The sender whose drop ends the joiners' wait.
        done: watch::Sender<()>,
        /// When the read starts.
        start: SystemTime,
        /// The epoch it starts in.
        epoch: u64,
    },
}

/// Runs `read` on a task of its own and returns its answer. The task stores the answer (when
/// `store` names where, by the rules of `Shared::store`) and then drops `running`; a caller that
/// goes away cancels neither. Joiners therefore look again only after the store.
async fn lead(
    read: ReadFuture,
    store: Option<Store>,
    running: Running,
) -> Result<Option<TenantRow>, AppError> {
    let shared = Arc::clone(&running.shared);
    let task = tokio::spawn(async move {
        let answer = read.await;
        if let (Some(store), Ok(found)) = (&store, &answer) {
            shared.store(store, found.as_ref());
        }
        drop(running);
        answer
    });
    task.await.unwrap_or_else(|_| {
        Err(AppError::internal(
            "the lookup read ended without an answer",
        ))
    })
}

/// The production reader: one connection of `pool` for one statement.
fn control_reader(pool: PgPool) -> impl Fn(Read) -> ReadFuture + Send + Sync {
    move |read: Read| -> ReadFuture {
        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool.acquire().await.map_err(sekvent_db::to_app_error)?;
            let answer = match read {
                Read::Id(id) => rows::get(&mut conn, id).await,
                Read::Host(host) => rows::by_host(&mut conn, &host).await,
                Read::LoopbackLabel(label) => rows::by_loopback_label(&mut conn, &label).await,
            };
            drop(conn);
            answer
        })
    }
}

/// The lookup cache, the lookup admission and the reads in flight. Clones share everything but
/// the label rules.
#[derive(Clone)]
pub(super) struct Lookups {
    /// The cache, the permits and the reads in flight of every clone.
    shared: Arc<Shared>,
    /// The product's label rules, applied to every path segment before any read.
    labels: LabelRules,
}

impl Lookups {
    /// `control_max` is the control pool's `max_connections`.
    pub(super) fn new(control_max: u32, clock: Arc<dyn Clock>) -> Self {
        Self {
            shared: Arc::new(Shared {
                clock,
                admission: LookupAdmission::new(control_max),
                state: Mutex::new(State::new()),
            }),
            labels: LabelRules::new(),
        }
    }

    /// `Registry::labels`.
    pub(super) fn set_labels(&mut self, rules: LabelRules) {
        self.labels = rules;
    }

    /// `Registry::lookup`.
    pub(super) async fn find(
        &self,
        pool: &PgPool,
        selector: &Selector,
    ) -> Result<Option<TenantRow>, AppError> {
        let reader = control_reader(pool.clone());
        self.find_with(selector, &reader).await
    }

    /// `Registry::tenant`.
    pub(super) async fn find_internal(
        &self,
        pool: &PgPool,
        id: TenantId,
    ) -> Result<Option<TenantRow>, AppError> {
        let reader = control_reader(pool.clone());
        self.find_internal_with(id, &reader).await
    }

    /// `Registry::forget`.
    pub(super) fn forget(&self, tenant: TenantId) {
        self.shared.state().forget(tenant);
    }

    /// `Registry::clear_lookups`.
    pub(super) fn clear(&self) {
        self.shared.state().clear();
    }

    /// `find` with the statement made by `reader`.
    async fn find_with(
        &self,
        selector: &Selector,
        reader: Reader<'_>,
    ) -> Result<Option<TenantRow>, AppError> {
        match plan(selector, &self.labels) {
            Plan::Nothing => Ok(None),
            Plan::Cached(key) => self.find_cached(key, reader).await,
            Plan::Uncached(read) => {
                let permit = self
                    .shared
                    .admission
                    .try_admit()
                    .ok_or_else(serving::busy)?;
                let running = Running {
                    shared: Arc::clone(&self.shared),
                    flight: None,
                    permit: Some(permit),
                    done: None,
                };
                lead(reader(read), None, running).await
            }
        }
    }

    /// A cached key: the cache, then a join, then a read of its own under a permit.
    async fn find_cached(
        &self,
        key: Key,
        reader: Reader<'_>,
    ) -> Result<Option<TenantRow>, AppError> {
        let mut joins = 0_u32;
        loop {
            let next = {
                let mut state = self.shared.state();
                let now = self.shared.clock.now();
                if let Some(hit) = state.answer(&key, now) {
                    return Ok(hit.into_answer());
                }
                if joins >= MAX_JOINS {
                    return Err(serving::busy());
                }
                if let Some(flight) = state.flights.get(&key) {
                    Next::Join(flight.done.clone())
                } else {
                    let Some(permit) = self.shared.admission.try_admit() else {
                        return Err(serving::busy());
                    };
                    let (id, done) = state.start_flight(key.clone());
                    Next::Lead {
                        permit,
                        id,
                        done,
                        start: now,
                        epoch: state.epoch,
                    }
                }
            };
            match next {
                Next::Join(mut done) => {
                    joins += 1;
                    // Never sent: the wait ends when the read's sender is dropped.
                    let _ = done.changed().await;
                }
                Next::Lead {
                    permit,
                    id,
                    done,
                    start,
                    epoch,
                } => {
                    let running = Running {
                        shared: Arc::clone(&self.shared),
                        flight: Some((key.clone(), id)),
                        permit: Some(permit),
                        done: Some(done),
                    };
                    let read = reader(key.read());
                    let store = Store { key, start, epoch };
                    return lead(read, Some(store), running).await;
                }
            }
        }
    }

    /// `find_internal` with the statement made by `reader`: the cache, otherwise a read on the
    /// caller's own task with no permit, never joined and never joining.
    async fn find_internal_with(
        &self,
        id: TenantId,
        reader: Reader<'_>,
    ) -> Result<Option<TenantRow>, AppError> {
        let key = Key::Id(id);
        let store = {
            let mut state = self.shared.state();
            let now = self.shared.clock.now();
            if let Some(hit) = state.answer(&key, now) {
                return Ok(hit.into_answer());
            }
            Store {
                key,
                start: now,
                epoch: state.epoch,
            }
        };
        let answer = reader(Read::Id(id)).await;
        if let Ok(found) = &answer {
            self.shared.store(&store, found.as_ref());
        }
        answer
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use sekvent_context::ManualClock;
    use sekvent_error::ErrorCode;
    use tokio::sync::Semaphore;
    use uuid::Uuid;

    use super::super::rows::ProvisionStep;
    use super::*;
    use crate::{RoleKey, TenantInfo, TenantState, reasons};

    /// The time every test starts at.
    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// `t0()` plus `millis` milliseconds.
    fn at(millis: u64) -> SystemTime {
        t0() + Duration::from_millis(millis)
    }

    /// A row of tenant `n` with the host `<label>.example.com`.
    fn row(n: u128, label: &str) -> TenantRow {
        let host = format!("{label}.example.com");
        TenantRow {
            info: TenantInfo {
                id: TenantId::from_uuid(Uuid::from_u128(n)),
                subdomain: label.to_owned(),
                display_name: label.to_owned(),
                state: TenantState::Active,
                primary_host: host.clone(),
                hosts: vec![host],
                role_key: RoleKey {
                    key_id: "k1".to_owned(),
                    generation: 1,
                },
            },
            provision_step: ProvisionStep::Active,
            db_cluster: "main".to_owned(),
            db_name: format!("app_t_{n}"),
            db_role: format!("app_t_{n}"),
            schema_version: Some(1),
            created_at: t0(),
            updated_at: t0(),
        }
    }

    /// The id of tenant `n`.
    fn id(n: u128) -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(n))
    }

    /// What the fake read does.
    #[derive(Clone)]
    enum Outcome {
        Answer(Box<Option<TenantRow>>),
        Fail,
        Panic,
    }

    impl Outcome {
        fn answer(answer: Option<TenantRow>) -> Self {
            Self::Answer(Box::new(answer))
        }
    }

    /// A control read without a server: it records each call, counts arrivals, waits at a gate and
    /// then does what its outcome says. Clones share everything.
    #[derive(Clone)]
    struct Fake {
        inner: Arc<FakeInner>,
    }

    struct FakeInner {
        outcome: std::sync::Mutex<Outcome>,
        calls: std::sync::Mutex<Vec<Read>>,
        gate: Semaphore,
        arrived: watch::Sender<usize>,
    }

    impl Fake {
        /// Reads pass the gate at once.
        fn open(outcome: Outcome) -> Self {
            Self::with_gate(outcome, Semaphore::MAX_PERMITS)
        }

        /// Reads wait at the gate until `release`.
        fn gated(outcome: Outcome) -> Self {
            Self::with_gate(outcome, 0)
        }

        fn with_gate(outcome: Outcome, permits: usize) -> Self {
            Self {
                inner: Arc::new(FakeInner {
                    outcome: std::sync::Mutex::new(outcome),
                    calls: std::sync::Mutex::new(Vec::new()),
                    gate: Semaphore::new(permits),
                    arrived: watch::channel(0).0,
                }),
            }
        }

        fn set(&self, outcome: Outcome) {
            *self.inner.outcome.lock().unwrap() = outcome;
        }

        /// Lets `reads` waiting or later reads through.
        fn release(&self, reads: usize) {
            self.inner.gate.add_permits(reads);
        }

        fn calls(&self) -> Vec<Read> {
            self.inner.calls.lock().unwrap().clone()
        }

        /// Resolves once `count` reads have reached the gate.
        async fn arrived(&self, count: usize) {
            let mut arrivals = self.inner.arrived.subscribe();
            arrivals
                .wait_for(|arrived| *arrived >= count)
                .await
                .expect("the fake holds its sender");
        }

        fn reader(self) -> impl Fn(Read) -> ReadFuture + Send + Sync {
            move |read: Read| -> ReadFuture {
                let fake = self.clone();
                Box::pin(async move {
                    fake.inner.calls.lock().unwrap().push(read);
                    fake.inner.arrived.send_modify(|arrived| *arrived += 1);
                    fake.inner
                        .gate
                        .acquire()
                        .await
                        .expect("the gate is never closed")
                        .forget();
                    let outcome = fake.inner.outcome.lock().unwrap().clone();
                    match outcome {
                        Outcome::Answer(answer) => Ok(*answer),
                        Outcome::Fail => Err(AppError::unavailable("the fake read failed")),
                        Outcome::Panic => panic!("the fake read panicked"),
                    }
                })
            }
        }
    }

    /// Lookups of a control pool of `control_max` on `clock`.
    fn new_lookups(control_max: u32, clock: &ManualClock) -> Lookups {
        Lookups::new(control_max, Arc::new(clock.clone()))
    }

    fn available(lookups: &Lookups) -> usize {
        lookups.shared.admission.available()
    }

    fn assert_busy(error: &AppError) {
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::TENANT_BUSY));
        assert_eq!(error.domain(), None);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(1)));
    }

    fn host_of(row: &TenantRow) -> Selector {
        Selector::Host(row.info.primary_host.clone())
    }

    #[test]
    fn freshness_is_half_open_and_never_extended_by_a_clock_set_back() {
        assert!(is_fresh(t0(), t0()));
        assert!(is_fresh(t0(), at(29_999)));
        assert!(!is_fresh(t0(), at(30_000)));
        assert!(!is_fresh(at(1_000), t0()));
    }

    #[tokio::test]
    async fn found_and_missing_entries_expire_at_exactly_thirty_seconds() {
        for answer in [Some(row(1, "billing")), None] {
            let clock = ManualClock::new(t0());
            let lookups = new_lookups(4, &clock);
            let fake = Fake::open(Outcome::answer(answer.clone()));
            let reader = fake.clone().reader();
            let selector = Selector::Id(id(1));

            assert_eq!(lookups.find_with(&selector, &reader).await.unwrap(), answer);
            clock.set(at(29_999));
            assert_eq!(lookups.find_with(&selector, &reader).await.unwrap(), answer);
            assert_eq!(fake.calls().len(), 1, "served from the cache");
            clock.set(at(30_000));
            assert_eq!(lookups.find_with(&selector, &reader).await.unwrap(), answer);
            assert_eq!(fake.calls().len(), 2, "read again at 30 s");

            // The entry of the read at 30 s is not served one second before it.
            clock.set(at(29_000));
            assert_eq!(lookups.find_with(&selector, &reader).await.unwrap(), answer);
            assert_eq!(fake.calls().len(), 3, "a clock set back does not serve it");
        }
    }

    #[test]
    fn a_found_entry_replaces_a_missing_one_and_the_reverse() {
        let mut state = State::new();
        let billing = row(1, "billing");
        let key = Key::Id(billing.info.id);

        state.store(&key, None, at(1), at(1));
        assert!(matches!(state.answer(&key, at(1)), Some(Hit::Missing)));
        state.store(&key, Some(billing.clone()), at(2), at(2));
        assert!(matches!(state.answer(&key, at(2)), Some(Hit::Found(found)) if *found == billing));
        assert_eq!(state.ids.missing.entries.len(), 0);
        state.store(&key, None, at(3), at(3));
        assert!(matches!(state.answer(&key, at(3)), Some(Hit::Missing)));
        assert_eq!(state.ids.found.entries.len(), 0);

        let host = Key::Host(billing.info.primary_host.clone());
        state.store(&host, None, at(1), at(1));
        state.store(&host, Some(billing.clone()), at(2), at(2));
        assert!(matches!(state.answer(&host, at(2)), Some(Hit::Found(_))));
        assert_eq!(state.hosts.missing.entries.len(), 0);
        assert_eq!(
            state.answer(&Key::Id(id(2)), at(2)).map(Hit::into_answer),
            None
        );
    }

    #[test]
    fn a_full_missing_partition_drops_expired_entries_then_clears_itself_only() {
        let mut state = State::new();
        for n in 0..10 {
            state.store(
                &Key::Id(id(100_000 + n)),
                Some(row(100_000 + n, "found")),
                t0(),
                t0(),
            );
        }
        for n in 0..5 {
            state.store(&Key::Host(format!("h{n}.example.com")), None, t0(), t0());
        }
        for n in 0..1_000 {
            state.store(&Key::Id(id(n)), None, t0(), t0());
        }
        for n in 1_000..MISSING_ENTRIES as u128 {
            state.store(&Key::Id(id(n)), None, at(20_000), at(20_000));
        }
        assert_eq!(state.ids.missing.entries.len(), MISSING_ENTRIES);

        // Storing an existing key in a full partition drops nothing.
        state.store(&Key::Id(id(5)), None, at(20_000), at(20_000));
        assert_eq!(state.ids.missing.entries.len(), MISSING_ENTRIES);

        // At 31 s the entries read at 0 s (but the one stored again) have expired.
        let now = at(31_000);
        state.store(&Key::Id(id(50_000)), None, now, now);
        assert_eq!(state.ids.missing.entries.len(), 24 + 1 + 1);

        for n in 60_000..60_000 + (MISSING_ENTRIES - 26) as u128 {
            state.store(&Key::Id(id(n)), None, now, now);
        }
        assert_eq!(state.ids.missing.entries.len(), MISSING_ENTRIES);
        state.store(&Key::Id(id(70_000)), None, now, now);
        assert_eq!(state.ids.missing.entries.len(), 1, "still full: cleared");

        assert_eq!(state.ids.found.entries.len(), 10, "found entries stay");
        assert_eq!(
            state.hosts.missing.entries.len(),
            5,
            "the host space is untouched"
        );
    }

    #[test]
    fn a_full_found_partition_drops_expired_entries_then_clears_itself_only() {
        let mut state = State::new();
        for n in 0..7 {
            state.store(&Key::Id(id(900_000 + n)), None, t0(), t0());
        }
        state.store(
            &Key::Host("kept.example.com".to_owned()),
            Some(row(800_000, "kept")),
            t0(),
            t0(),
        );
        assert_eq!(
            state.ids.found.entries.len(),
            1,
            "the host read warmed the id space"
        );
        for n in 0..4_000 {
            state.store(&Key::Id(id(n)), Some(row(n, "a")), t0(), t0());
        }
        for n in 4_000..FOUND_ENTRIES as u128 - 1 {
            state.store(&Key::Id(id(n)), Some(row(n, "a")), at(20_000), at(20_000));
        }
        assert_eq!(state.ids.found.entries.len(), FOUND_ENTRIES);

        let now = at(31_000);
        state.store(&Key::Id(id(10_000)), Some(row(10_000, "b")), now, now);
        assert_eq!(state.ids.found.entries.len(), 95 + 1);

        for n in 20_000..20_000 + (FOUND_ENTRIES - 96) as u128 {
            state.store(&Key::Id(id(n)), Some(row(n, "c")), now, now);
        }
        assert_eq!(state.ids.found.entries.len(), FOUND_ENTRIES);
        state.store(&Key::Id(id(30_000)), Some(row(30_000, "d")), now, now);
        assert_eq!(state.ids.found.entries.len(), 1, "still full: cleared");

        assert_eq!(state.ids.missing.entries.len(), 7, "not-found entries stay");
        assert_eq!(
            state.hosts.found.entries.len(),
            1,
            "the host space is untouched"
        );
    }

    #[tokio::test]
    async fn a_host_read_warms_the_id_space_and_an_id_read_does_not_warm_the_host_space() {
        let clock = ManualClock::new(t0());
        let billing = row(1, "billing");

        let warmed = new_lookups(4, &clock);
        let fake = Fake::open(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let found = warmed.find_with(&host_of(&billing), &reader).await.unwrap();
        assert_eq!(found.as_ref(), Some(&billing));
        let by_id = warmed
            .find_with(&Selector::Id(billing.info.id), &reader)
            .await;
        assert_eq!(by_id.unwrap().as_ref(), Some(&billing));
        let by_label = Selector::PathLabel {
            segment: "Billing".to_owned(),
            listener_host: "app.example.com".to_owned(),
        };
        let by_label = warmed.find_with(&by_label, &reader).await.unwrap();
        assert_eq!(by_label.as_ref(), Some(&billing));
        assert_eq!(fake.calls(), [Read::Host("billing.example.com".to_owned())]);

        let cold = new_lookups(4, &clock);
        let fake = Fake::open(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        cold.find_with(&Selector::Id(billing.info.id), &reader)
            .await
            .unwrap();
        cold.find_with(&host_of(&billing), &reader).await.unwrap();
        assert_eq!(
            fake.calls(),
            [
                Read::Id(billing.info.id),
                Read::Host("billing.example.com".to_owned())
            ]
        );
    }

    #[test]
    fn forget_drops_the_tenant_and_every_missing_host_and_clear_drops_everything() {
        let mut state = State::new();
        let billing = row(1, "billing");
        let orders = row(2, "orders");
        for found in [&billing, &orders] {
            state.store(&Key::Id(found.info.id), Some(found.clone()), t0(), t0());
            let host = Key::Host(found.info.primary_host.clone());
            state.store(&host, Some(found.clone()), t0(), t0());
        }
        state.store(&Key::Host("gone.example.com".to_owned()), None, t0(), t0());
        state.store(&Key::Id(id(3)), None, t0(), t0());
        // A later read of the tenant found no row: its id entry is now a not-found one.
        state.store(&Key::Id(billing.info.id), None, at(1), at(1));
        assert_eq!(state.ids.found.entries.len(), 1, "only the orders id entry");

        state.forget(billing.info.id);
        assert_eq!(state.epoch, 1);
        assert!(state.answer(&Key::Id(billing.info.id), at(1)).is_none());
        assert!(
            state
                .answer(&Key::Host("billing.example.com".to_owned()), at(1))
                .is_none()
        );
        assert!(
            state
                .answer(&Key::Host("gone.example.com".to_owned()), at(1))
                .is_none()
        );
        assert!(matches!(
            state.answer(&Key::Id(id(3)), at(1)),
            Some(Hit::Missing)
        ));
        assert!(matches!(
            state.answer(&Key::Id(orders.info.id), at(1)),
            Some(Hit::Found(_))
        ));
        let orders_host = Key::Host("orders.example.com".to_owned());
        assert!(matches!(
            state.answer(&orders_host, at(1)),
            Some(Hit::Found(_))
        ));

        state.forget(id(3));
        assert!(state.answer(&Key::Id(id(3)), at(1)).is_none());
        assert_eq!(state.epoch, 2);

        state.clear();
        assert_eq!(state.epoch, 3);
        assert_eq!(state.ids.found.entries.len(), 0);
        assert_eq!(state.hosts.found.entries.len(), 0);
        assert_eq!(state.ids.missing.entries.len(), 0);
        assert_eq!(state.hosts.missing.entries.len(), 0);
    }

    #[tokio::test]
    async fn a_read_in_flight_during_forget_or_clear_returns_its_answer_and_stores_nothing() {
        let billing = row(1, "billing");
        for clear in [false, true] {
            let clock = ManualClock::new(t0());
            let lookups = new_lookups(4, &clock);
            let fake = Fake::gated(Outcome::answer(Some(billing.clone())));
            let reader = fake.clone().reader();
            let selector = Selector::Id(billing.info.id);

            let mut first = pin!(lookups.find_with(&selector, &reader));
            assert!(futures::poll!(first.as_mut()).is_pending());
            fake.arrived(1).await;
            if clear {
                lookups.clear();
            } else {
                lookups.forget(billing.info.id);
            }
            fake.release(usize::MAX >> 4);
            assert_eq!(first.await.unwrap().as_ref(), Some(&billing));

            assert_eq!(
                lookups.find_with(&selector, &reader).await.unwrap(),
                Some(billing.clone())
            );
            assert_eq!(fake.calls().len(), 2, "the first answer was not stored");
            assert_eq!(
                lookups.find_with(&selector, &reader).await.unwrap(),
                Some(billing.clone())
            );
            assert_eq!(fake.calls().len(), 2, "the second answer was stored");
        }
    }

    #[tokio::test]
    async fn a_failed_read_caches_nothing_also_over_an_expired_entry() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let billing = row(1, "billing");
        let fake = Fake::open(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let selector = host_of(&billing);

        lookups.find_with(&selector, &reader).await.unwrap();
        clock.set(at(30_000));
        fake.set(Outcome::Fail);
        for calls in [2, 3] {
            let error = lookups.find_with(&selector, &reader).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unavailable);
            assert_eq!(error.message(), "the fake read failed");
            assert_eq!(fake.calls().len(), calls, "the expired entry is not served");
        }
        let by_id = lookups
            .find_with(&Selector::Id(billing.info.id), &reader)
            .await;
        assert!(
            by_id.is_err(),
            "the id entry the host read made expired too"
        );

        fake.set(Outcome::answer(None));
        assert_eq!(lookups.find_with(&selector, &reader).await.unwrap(), None);
        assert_eq!(lookups.find_with(&selector, &reader).await.unwrap(), None);
        assert_eq!(fake.calls().len(), 5);
        assert_eq!(available(&lookups), 2);
    }

    #[tokio::test]
    async fn an_invalid_selector_answers_none_without_a_read() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let fake = Fake::open(Outcome::answer(Some(row(1, "billing"))));
        let reader = fake.clone().reader();
        let label = |segment: &str, listener: &str| Selector::PathLabel {
            segment: segment.to_owned(),
            listener_host: listener.to_owned(),
        };
        for selector in [
            Selector::Host("Billing.example.com".to_owned()),
            Selector::Host("billing.example.com.".to_owned()),
            Selector::Host("billing.example.com:443".to_owned()),
            Selector::Host("not a host".to_owned()),
            Selector::Host(String::new()),
            label("bill_ing", "app.example.com"),
            label("bill_ing", "localhost:8080"),
            label("-billing", "localhost:8080"),
            label("billing", "LocalHost:8080"),
            label("billing", "example"),
            label("billing", "10.0.0.1"),
            label("billing", "not a host"),
        ] {
            assert_eq!(
                lookups.find_with(&selector, &reader).await.unwrap(),
                None,
                "{selector:?}"
            );
        }
        assert_eq!(fake.calls(), [] as [Read; 0]);
        assert_eq!(available(&lookups), 2);
    }

    #[tokio::test]
    async fn a_segment_the_products_label_rules_refuse_answers_none_without_a_read() {
        let clock = ManualClock::new(t0());
        let mut lookups = new_lookups(4, &clock);
        lookups.set_labels(
            LabelRules::new()
                .with_length(3, 20)
                .unwrap()
                .with_reserved(|label| label == "admin"),
        );
        let billing = row(1, "billing");
        let fake = Fake::open(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let label = |segment: &str, listener: &str| Selector::PathLabel {
            segment: segment.to_owned(),
            listener_host: listener.to_owned(),
        };
        for listener in ["localhost:8080", "app.example.com"] {
            for segment in ["admin", "Admin", "ab"] {
                let selector = label(segment, listener);
                assert_eq!(
                    lookups.find_with(&selector, &reader).await.unwrap(),
                    None,
                    "{selector:?}"
                );
            }
        }
        assert_eq!(fake.calls(), [] as [Read; 0]);
        assert_eq!(available(&lookups), 2);

        let found = lookups
            .find_with(&label("Billing", "localhost:8080"), &reader)
            .await;
        assert_eq!(found.unwrap().as_ref(), Some(&billing));
        let found = lookups
            .find_with(&label("billing", "app.example.com"), &reader)
            .await;
        assert_eq!(found.unwrap().as_ref(), Some(&billing));
        assert_eq!(
            fake.calls(),
            [
                Read::LoopbackLabel("billing".to_owned()),
                Read::Host("billing.example.com".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn the_loopback_label_read_is_never_cached_never_joined_and_spends_a_permit() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let billing = row(1, "billing");
        let fake = Fake::gated(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let on_port = |listener: &str| Selector::PathLabel {
            segment: "Billing".to_owned(),
            listener_host: listener.to_owned(),
        };
        let first_selector = on_port("localhost:8080");
        let second_selector = on_port("127.0.0.1:3000");

        let mut first = pin!(lookups.find_with(&first_selector, &reader));
        let mut second = pin!(lookups.find_with(&second_selector, &reader));
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert_eq!(
            available(&lookups),
            0,
            "each read took a permit; none joined"
        );
        let third = lookups.find_with(&on_port("localhost:9000"), &reader).await;
        assert_busy(&third.unwrap_err());

        fake.arrived(2).await;
        fake.release(usize::MAX >> 4);
        assert_eq!(first.await.unwrap().as_ref(), Some(&billing));
        assert_eq!(second.await.unwrap().as_ref(), Some(&billing));
        assert_eq!(available(&lookups), 2);

        lookups.find_with(&first_selector, &reader).await.unwrap();
        let label = Read::LoopbackLabel("billing".to_owned());
        assert_eq!(fake.calls(), [label.clone(), label.clone(), label]);
        let by_id = lookups
            .find_with(&Selector::Id(billing.info.id), &reader)
            .await;
        assert_eq!(by_id.unwrap().as_ref(), Some(&billing));
        assert_eq!(
            fake.calls().len(),
            4,
            "the label read stored nothing in the id space"
        );
    }

    #[tokio::test]
    async fn two_callers_of_one_key_cause_one_read_and_take_one_permit() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let billing = row(1, "billing");
        let fake = Fake::gated(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let selector = Selector::Id(billing.info.id);

        let mut leader = pin!(lookups.find_with(&selector, &reader));
        let mut joiner = pin!(lookups.find_with(&selector, &reader));
        assert!(futures::poll!(leader.as_mut()).is_pending());
        assert!(futures::poll!(joiner.as_mut()).is_pending());
        assert_eq!(available(&lookups), 1);

        fake.arrived(1).await;
        fake.release(1);
        assert_eq!(leader.await.unwrap().as_ref(), Some(&billing));
        assert_eq!(joiner.await.unwrap().as_ref(), Some(&billing));
        assert_eq!(fake.calls(), [Read::Id(billing.info.id)]);
        assert_eq!(available(&lookups), 2);
    }

    #[tokio::test]
    async fn a_caller_that_joined_twice_without_an_answer_is_busy() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let fake = Fake::gated(Outcome::Fail);
        let reader = fake.clone().reader();
        let selector = Selector::Id(id(1));

        let mut first = pin!(lookups.find_with(&selector, &reader));
        let mut joiner = pin!(lookups.find_with(&selector, &reader));
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(joiner.as_mut()).is_pending());
        fake.arrived(1).await;
        fake.release(1);
        assert!(first.await.is_err(), "the leader gets its read's error");

        // A second read of the key starts before the joiner looks again.
        let mut second = pin!(lookups.find_with(&selector, &reader));
        assert!(futures::poll!(second.as_mut()).is_pending());
        assert!(
            futures::poll!(joiner.as_mut()).is_pending(),
            "it joins again"
        );
        fake.arrived(2).await;
        fake.release(1);
        assert!(second.await.is_err());

        assert_busy(&joiner.await.unwrap_err());
        assert_eq!(fake.calls().len(), 2, "the joiner never read itself");
        assert_eq!(available(&lookups), 2);
    }

    #[tokio::test]
    async fn a_joiner_of_a_failed_read_looks_again_and_does_not_get_its_error() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let billing = row(1, "billing");
        let fake = Fake::gated(Outcome::Fail);
        let reader = fake.clone().reader();
        let selector = Selector::Id(billing.info.id);

        let mut leader = pin!(lookups.find_with(&selector, &reader));
        let mut joiner = pin!(lookups.find_with(&selector, &reader));
        assert!(futures::poll!(leader.as_mut()).is_pending());
        assert!(futures::poll!(joiner.as_mut()).is_pending());
        fake.arrived(1).await;
        fake.release(1);
        let error = leader.await.unwrap_err();
        assert_eq!(error.message(), "the fake read failed");

        fake.set(Outcome::answer(Some(billing.clone())));
        fake.release(1);
        assert_eq!(joiner.await.unwrap().as_ref(), Some(&billing));
        assert_eq!(fake.calls().len(), 2, "the joiner led a read of its own");
    }

    #[tokio::test]
    async fn no_free_permit_is_busy_before_any_read() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(3, &clock);
        let billing = row(1, "billing");
        let fake = Fake::gated(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let selector = Selector::Id(billing.info.id);

        let mut held = pin!(lookups.find_with(&selector, &reader));
        assert!(futures::poll!(held.as_mut()).is_pending());
        fake.arrived(1).await;
        assert_eq!(available(&lookups), 0);

        for other in [
            Selector::Id(id(2)),
            Selector::Host("orders.example.com".to_owned()),
            Selector::PathLabel {
                segment: "orders".to_owned(),
                listener_host: "localhost:8080".to_owned(),
            },
        ] {
            assert_busy(&lookups.find_with(&other, &reader).await.unwrap_err());
        }
        assert_eq!(fake.calls(), [Read::Id(billing.info.id)]);

        fake.release(1);
        assert_eq!(held.await.unwrap().as_ref(), Some(&billing));
        assert_eq!(available(&lookups), 1);
    }

    #[tokio::test]
    async fn clones_share_the_permits_the_cache_and_the_reads_in_flight() {
        let clock = ManualClock::new(t0());
        let original = new_lookups(4, &clock);
        let clone = original.clone();
        let billing = row(1, "billing");
        let fake = Fake::gated(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let first_key = Selector::Id(billing.info.id);
        let second_key = Selector::Id(id(2));

        let mut first = pin!(original.find_with(&first_key, &reader));
        let mut second = pin!(original.find_with(&second_key, &reader));
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(futures::poll!(second.as_mut()).is_pending());
        let third = clone.find_with(&Selector::Id(id(3)), &reader).await;
        assert_busy(&third.unwrap_err());

        fake.arrived(2).await;
        fake.release(2);
        first.await.unwrap();
        second.await.unwrap();
        let cached = clone.find_with(&first_key, &reader).await.unwrap();
        assert_eq!(cached.as_ref(), Some(&billing));
        assert_eq!(
            fake.calls().len(),
            2,
            "the clone served the original's answer"
        );

        let orders = row(4, "orders");
        fake.set(Outcome::answer(Some(orders.clone())));
        let shared_key = Selector::Id(orders.info.id);
        let mut led = pin!(original.find_with(&shared_key, &reader));
        let mut joined = pin!(clone.find_with(&shared_key, &reader));
        assert!(futures::poll!(led.as_mut()).is_pending());
        assert!(futures::poll!(joined.as_mut()).is_pending());
        fake.arrived(3).await;
        fake.release(1);
        assert_eq!(led.await.unwrap().as_ref(), Some(&orders));
        assert_eq!(joined.await.unwrap().as_ref(), Some(&orders));
        assert_eq!(
            fake.calls().len(),
            3,
            "the clone joined the original's read"
        );
        assert_eq!(available(&clone), 2);
    }

    #[tokio::test]
    async fn a_dropped_caller_leaves_its_read_running_to_fill_the_cache() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(3, &clock);
        let billing = row(1, "billing");
        let fake = Fake::gated(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let selector = host_of(&billing);

        let mut caller = Box::pin(lookups.find_with(&selector, &reader));
        assert!(futures::poll!(caller.as_mut()).is_pending());
        fake.arrived(1).await;
        drop(caller);
        assert_eq!(available(&lookups), 0, "the read keeps its permit");

        fake.release(1);
        let later = lookups.find_with(&selector, &reader).await.unwrap();
        assert_eq!(later.as_ref(), Some(&billing));
        assert_eq!(
            fake.calls().len(),
            1,
            "the dropped caller's read filled the cache"
        );
        assert_eq!(available(&lookups), 1, "the permit came back");
    }

    #[tokio::test]
    async fn a_panicking_read_is_internal_and_gives_its_permit_and_its_key_back() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(3, &clock);
        let billing = row(1, "billing");
        let fake = Fake::open(Outcome::Panic);
        let reader = fake.clone().reader();
        let selector = Selector::Id(billing.info.id);

        let error = lookups.find_with(&selector, &reader).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        assert_eq!(available(&lookups), 1);
        assert!(lookups.shared.state().flights.is_empty());

        fake.set(Outcome::answer(Some(billing.clone())));
        let found = lookups.find_with(&selector, &reader).await.unwrap();
        assert_eq!(found.as_ref(), Some(&billing));
    }

    #[test]
    fn cleaning_up_an_older_read_never_removes_a_newer_read_of_the_same_key() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let key = Key::Id(id(1));
        let (older, older_done) = lookups.shared.state().start_flight(key.clone());
        let older_joiner = older_done.subscribe();
        let (newer, newer_done) = lookups.shared.state().start_flight(key.clone());
        let running = |flight: u64, done: watch::Sender<()>| Running {
            shared: Arc::clone(&lookups.shared),
            flight: Some((key.clone(), flight)),
            permit: Some(lookups.shared.admission.try_admit().expect("a free permit")),
            done: Some(done),
        };

        drop(running(older, older_done));
        assert!(
            older_joiner.has_changed().is_err(),
            "the older read's joiners are woken"
        );
        assert_eq!(available(&lookups), 2, "the older read's permit is back");
        let current = lookups
            .shared
            .state()
            .flights
            .get(&key)
            .map(|flight| flight.id);
        assert_eq!(current, Some(newer), "the newer read is still in flight");

        drop(running(newer, newer_done));
        assert!(lookups.shared.state().flights.is_empty());
        assert_eq!(available(&lookups), 2);
    }

    #[tokio::test]
    async fn an_internal_read_takes_no_permit_and_never_joins() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(3, &clock);
        let billing = row(1, "billing");
        let orders = row(2, "orders");
        let gated = Fake::gated(Outcome::answer(Some(billing.clone())));
        let gated_reader = gated.clone().reader();
        let open = Fake::open(Outcome::answer(Some(orders.clone())));
        let open_reader = open.clone().reader();

        let selector = Selector::Id(billing.info.id);
        let mut held = pin!(lookups.find_with(&selector, &gated_reader));
        assert!(futures::poll!(held.as_mut()).is_pending());
        gated.arrived(1).await;
        assert_eq!(available(&lookups), 0);

        let internal = lookups
            .find_internal_with(orders.info.id, &open_reader)
            .await;
        assert_eq!(internal.unwrap().as_ref(), Some(&orders));
        let again = lookups
            .find_internal_with(orders.info.id, &open_reader)
            .await;
        assert_eq!(again.unwrap().as_ref(), Some(&orders));
        let cached = lookups
            .find_with(&Selector::Id(orders.info.id), &open_reader)
            .await;
        assert_eq!(cached.unwrap().as_ref(), Some(&orders));
        assert_eq!(
            open.calls(),
            [Read::Id(orders.info.id)],
            "read once, then cached"
        );

        // The key of the gated read: an internal read does not wait for it.
        let beside = lookups
            .find_internal_with(billing.info.id, &open_reader)
            .await;
        assert_eq!(beside.unwrap().as_ref(), Some(&orders));
        assert_eq!(open.calls().len(), 2);

        open.set(Outcome::Fail);
        let failed = lookups.find_internal_with(id(3), &open_reader).await;
        assert!(failed.is_err());
        open.set(Outcome::answer(None));
        assert_eq!(
            lookups
                .find_internal_with(id(3), &open_reader)
                .await
                .unwrap(),
            None
        );
        assert_eq!(open.calls().len(), 4, "the failure stored nothing");

        gated.release(1);
        held.await.unwrap();
        assert_eq!(available(&lookups), 1);
    }

    /// `row` with its state set to `state`.
    fn in_state(mut row: TenantRow, state: TenantState) -> TenantRow {
        row.info.state = state;
        row
    }

    #[test]
    fn an_entry_is_replaced_only_by_an_observation_that_started_later() {
        let mut state = State::new();
        let active = row(1, "billing");
        let suspended = in_state(active.clone(), TenantState::Suspended);
        let by_id = Key::Id(active.info.id);
        let by_host = Key::Host(active.info.primary_host.clone());
        let now = at(5_000);
        let cached = |state: &mut State, key: &Key| state.answer(key, now).map(Hit::into_answer);

        state.store(&by_id, Some(suspended.clone()), at(2_000), at(2_000));
        state.store(&by_host, Some(active.clone()), at(1_000), at(3_000));
        assert_eq!(
            cached(&mut state, &by_host),
            Some(Some(active.clone())),
            "the host entry is stored under its own key"
        );
        assert_eq!(
            cached(&mut state, &by_id),
            Some(Some(suspended.clone())),
            "the older cross-fill left the newer id entry"
        );

        // On a tie the stored entry stays; an older not-found answer does not replace it either.
        state.store(&by_id, Some(active.clone()), at(2_000), now);
        state.store(&by_id, None, at(1_500), now);
        assert_eq!(cached(&mut state, &by_id), Some(Some(suspended.clone())));

        // A later answer replaces it, across the partitions too.
        state.store(&by_id, None, at(3_000), now);
        assert_eq!(cached(&mut state, &by_id), Some(None));
        state.store(&by_host, Some(suspended.clone()), at(4_000), now);
        assert_eq!(
            cached(&mut state, &by_id),
            Some(Some(suspended.clone())),
            "a later cross-fill replaces the id entry"
        );
        assert_eq!(cached(&mut state, &by_host), Some(Some(suspended)));
    }

    #[tokio::test]
    async fn a_slower_host_read_does_not_replace_the_id_entry_of_a_later_id_read() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let active = row(1, "billing");
        let suspended = in_state(active.clone(), TenantState::Suspended);
        let host_fake = Fake::gated(Outcome::answer(Some(active.clone())));
        let host_reader = host_fake.clone().reader();
        let id_fake = Fake::gated(Outcome::answer(Some(suspended.clone())));
        let id_reader = id_fake.clone().reader();
        let by_host = host_of(&active);
        let by_id = Selector::Id(active.info.id);

        let mut host_read = pin!(lookups.find_with(&by_host, &host_reader));
        assert!(futures::poll!(host_read.as_mut()).is_pending());
        host_fake.arrived(1).await;
        clock.set(at(1_000));
        let mut id_read = pin!(lookups.find_with(&by_id, &id_reader));
        assert!(futures::poll!(id_read.as_mut()).is_pending());
        id_fake.arrived(1).await;

        // The id read started later and ends first; the host read ends after it.
        id_fake.release(1);
        assert_eq!(id_read.await.unwrap().as_ref(), Some(&suspended));
        host_fake.release(1);
        assert_eq!(host_read.await.unwrap().as_ref(), Some(&active));

        let cache_only = Fake::open(Outcome::Fail);
        let cache_reader = cache_only.clone().reader();
        let cached = lookups.find_with(&by_id, &cache_reader).await.unwrap();
        assert_eq!(
            cached.as_ref(),
            Some(&suspended),
            "the newer observation stays"
        );
        let cached = lookups.find_with(&by_host, &cache_reader).await.unwrap();
        assert_eq!(cached.as_ref(), Some(&active), "the host entry is stored");
        assert_eq!(cache_only.calls(), [] as [Read; 0]);
        assert_eq!(available(&lookups), 2);
    }

    #[tokio::test]
    async fn a_later_host_read_replaces_an_older_id_entry() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let active = row(1, "billing");
        let suspended = in_state(active.clone(), TenantState::Suspended);
        let fake = Fake::open(Outcome::answer(Some(active.clone())));
        let reader = fake.clone().reader();
        let by_id = Selector::Id(active.info.id);

        lookups.find_with(&by_id, &reader).await.unwrap();
        clock.set(at(1_000));
        fake.set(Outcome::answer(Some(suspended.clone())));
        let found = lookups.find_with(&host_of(&active), &reader).await;
        assert_eq!(found.unwrap().as_ref(), Some(&suspended));
        let cached = lookups.find_with(&by_id, &reader).await.unwrap();
        assert_eq!(
            cached.as_ref(),
            Some(&suspended),
            "the later observation replaced the id entry"
        );
        assert_eq!(fake.calls().len(), 2);
    }

    #[tokio::test]
    async fn an_entry_found_expired_is_removed_and_not_served_after_the_clock_is_set_back() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let billing = row(1, "billing");
        let fake = Fake::open(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let selector = Selector::Id(billing.info.id);

        lookups.find_with(&selector, &reader).await.unwrap();
        clock.set(at(30_000));
        fake.set(Outcome::Fail);
        assert!(lookups.find_with(&selector, &reader).await.is_err());
        assert_eq!(
            lookups.shared.state().ids.found.entries.len(),
            0,
            "the expired entry was removed when it was found"
        );

        // Inside the old entry's lifetime again: it is gone, so the lookup reads.
        clock.set(at(10_000));
        fake.set(Outcome::answer(None));
        assert_eq!(lookups.find_with(&selector, &reader).await.unwrap(), None);
        assert_eq!(fake.calls().len(), 3);
    }

    #[tokio::test]
    async fn an_entry_dated_after_now_is_removed_on_first_sight_and_never_served() {
        let clock = ManualClock::new(at(30_000));
        let lookups = new_lookups(4, &clock);
        let billing = row(1, "billing");
        let fake = Fake::open(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let selector = host_of(&billing);

        lookups.find_with(&selector, &reader).await.unwrap();
        clock.set(t0());
        fake.set(Outcome::Fail);
        assert!(lookups.find_with(&selector, &reader).await.is_err());
        {
            let state = lookups.shared.state();
            assert_eq!(state.hosts.found.entries.len(), 0, "removed on first sight");
            assert_eq!(
                state.ids.found.entries.len(),
                1,
                "the id entry was not looked at"
            );
        }

        // The clock reaches the removed entry's start: it is not served.
        clock.set(at(30_000));
        fake.set(Outcome::answer(None));
        assert_eq!(lookups.find_with(&selector, &reader).await.unwrap(), None);
        assert_eq!(fake.calls().len(), 3);

        // The id entry dated in the future is removed by an internal read too.
        clock.set(t0());
        fake.set(Outcome::Fail);
        let internal = lookups.find_internal_with(billing.info.id, &reader).await;
        assert!(internal.is_err());
        assert_eq!(lookups.shared.state().ids.found.entries.len(), 0);
        clock.set(at(30_000));
        fake.set(Outcome::answer(None));
        let internal = lookups.find_internal_with(billing.info.id, &reader).await;
        assert_eq!(internal.unwrap(), None);
        assert_eq!(fake.calls().len(), 5);
    }

    #[tokio::test]
    async fn an_answer_already_past_its_lifetime_is_returned_and_not_stored() {
        let clock = ManualClock::new(t0());
        let lookups = new_lookups(4, &clock);
        let billing = row(1, "billing");
        let fake = Fake::gated(Outcome::answer(Some(billing.clone())));
        let reader = fake.clone().reader();
        let selector = host_of(&billing);

        let mut leader = pin!(lookups.find_with(&selector, &reader));
        let mut joiner = pin!(lookups.find_with(&selector, &reader));
        assert!(futures::poll!(leader.as_mut()).is_pending());
        assert!(futures::poll!(joiner.as_mut()).is_pending());
        fake.arrived(1).await;
        clock.set(at(30_000));
        fake.release(1);
        assert_eq!(
            leader.await.unwrap().as_ref(),
            Some(&billing),
            "its caller gets the answer"
        );
        {
            let state = lookups.shared.state();
            assert_eq!(state.hosts.found.entries.len(), 0, "nothing stored");
            assert_eq!(state.ids.found.entries.len(), 0, "no cross-fill either");
        }

        // The joiner looks again, finds nothing and leads a read of its own, which is stored.
        assert!(futures::poll!(joiner.as_mut()).is_pending());
        fake.arrived(2).await;
        fake.release(1);
        assert_eq!(joiner.await.unwrap().as_ref(), Some(&billing));
        let cached = lookups.find_with(&selector, &reader).await.unwrap();
        assert_eq!(cached.as_ref(), Some(&billing));
        assert_eq!(fake.calls().len(), 2, "the third lookup was served");
        assert_eq!(available(&lookups), 2);
    }
}
