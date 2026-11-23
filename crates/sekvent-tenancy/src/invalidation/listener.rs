//! The listener of the invalidation channel and its periodic resync.
//!
//! The listener holds one `LISTEN` session on the control pool. A change forgets the tenant's
//! lookups and retires the tenant's scope at once, so a cached scope (one a lookup without a
//! row read would still hand out) is gone before anything is awaited; it then reloads the host
//! snapshot (and with it the tenant's entry in the baseline the resync compares with), retires
//! the scope again, also when the reload failed, and tells the sink; a hint goes to the sink
//! alone. Periodically it resyncs: it clears every lookup, reloads the active tenants, retires
//! the scope of each one that changed, joined or left since the baseline, and tells the sink.
//! Because a scope is retired again after the read that becomes the baseline, a scope opened
//! before that read never survives it, so a later change whose notification is lost still
//! differs from the baseline at the next successful resync (unless another change restored what
//! the baseline holds).
//!
//! Retiring a scope takes it out of the tenant cache at once; closing its pool waits for the
//! connections still checked out, and that wait runs on the cache's own task, never on the
//! listener's.
//!
//! The next periodic resync is due one interval after `ready`, and then one interval after the
//! previous resync ended (a resync after a reconnect included), whatever it took: a tick that
//! came due while something else was handled runs once, at once, and is never replayed, and a
//! resync slower than the interval still leaves room for the notifications. An interval longer
//! than 100 years never ticks.
//!
//! After any failed read of the channel that does not end the listener (a lost connection, or
//! any other read error), the listener listens again on a new connection taken from the pool,
//! then resyncs and calls [`Sink::reconnected`], also when that resync fails. A lost connection
//! is reported by the read itself, before anything else is awaited, so a tick that interrupts a
//! read never hides it. A failed read, or a failed attempt to listen again, is logged by its code
//! and followed by a 1 s wait.
//!
//! One notification drops a tenant's scope and lookups on every listening process in the normal
//! case. A lost notification is repaired by the next successful resync; a resync happens only
//! when the control database answers, so there is no bound in seconds. A resync's load has no
//! deadline of its own. Two failed resyncs in a row empty the host snapshot, so origins are
//! refused rather than allowed from stale data.
//!
//! The loop is written over private seams (a source of events, the sessions it listens on, and a
//! target of the effects), so that its rules run in unit tests on a paused clock without a
//! server.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::task::{Context, Waker};
use std::time::Duration;

#[cfg(feature = "runtime")]
use futures::future::BoxFuture;
use sekvent_error::AppError;
#[cfg(feature = "runtime")]
use sekvent_runtime::UnitContext;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::message::{ChangeKind, Channel, Message, Topic};
use crate::{HostSnapshot, Registry, TenantId, TenantInfo, Tenants};

/// How often a listener resyncs unless told otherwise.
const DEFAULT_RESYNC: Duration = Duration::from_secs(60);
/// The longest resync interval that is scheduled; a longer one never ticks.
const LONGEST_RESYNC: Duration = Duration::from_hours(100 * 365 * 24);
/// The wait after reading the channel failed, before reading again.
const RETRY_AFTER: Duration = Duration::from_secs(1);
/// Failed resyncs in a row after which the host snapshot is emptied.
const FAILURES_BEFORE_CLEARING: u32 = 2;

/// What a product does with what the listener learns. Every method has a default that does
/// nothing. Methods are called on the listener's task: they must return quickly and must not
/// panic.
pub trait Sink: Send + Sync + 'static {
    /// A change of `tenant` arrived; the crate already reloaded the host snapshot (or tried to)
    /// and then dropped what it held of the tenant.
    fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        let _ = (tenant, kind);
    }

    /// A hint of `tenant`'s data arrived.
    fn hint(&self, tenant: TenantId, topic: &Topic, seq: i64) {
        let _ = (tenant, topic, seq);
    }

    /// After a successful resync: the tenants that changed, joined or left, whose scopes the
    /// crate already retired.
    fn resync(&self, changed: &[TenantId]) {
        let _ = changed;
    }

    /// Reading the channel failed without ending the listener (the connection was lost, or any
    /// other read error) and the channel is listened on again, on a new connection:
    /// notifications may have been missed, so every waiter is stale. Called after the resync
    /// that follows, also when that resync failed.
    fn reconnected(&self) {}
}

impl Sink for () {}

/// One sink shared by several users: every method delegates to `T`'s.
impl<T: Sink + ?Sized> Sink for Arc<T> {
    fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        (**self).change(tenant, kind);
    }

    fn hint(&self, tenant: TenantId, topic: &Topic, seq: i64) {
        (**self).hint(tenant, topic, seq);
    }

    fn resync(&self, changed: &[TenantId]) {
        (**self).resync(changed);
    }

    fn reconnected(&self) {
        (**self).reconnected();
    }
}

/// Follows the channel for one `Tenants`. Clones share the sink.
pub struct Listener<S> {
    /// The tenants whose scopes, lookups and snapshot the channel keeps current.
    tenants: Tenants<S>,
    /// The channel to listen on.
    channel: Channel,
    /// Who hears what the listener learns.
    sink: Arc<dyn Sink>,
    /// The resync interval.
    every: Duration,
}

impl<S> Clone for Listener<S> {
    fn clone(&self) -> Self {
        Self {
            tenants: self.tenants.clone(),
            channel: self.channel.clone(),
            sink: Arc::clone(&self.sink),
            every: self.every,
        }
    }
}

// type name only
impl<S> fmt::Debug for Listener<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Listener")
    }
}

impl<S: Send + Sync + 'static> Listener<S> {
    /// Without a sink, resyncing every 60 s.
    pub fn new(tenants: Tenants<S>, channel: Channel) -> Self {
        Self {
            tenants,
            channel,
            sink: Arc::new(()),
            every: DEFAULT_RESYNC,
        }
    }

    /// The sink that hears what the listener learns.
    #[must_use]
    pub fn sink(mut self, sink: impl Sink) -> Self {
        self.sink = Arc::new(sink);
        self
    }

    /// How often to resync: the first periodic resync is one interval after `ready`, each next
    /// one an interval after the previous resync ended. `INVALID_ARGUMENT` for zero. Any other
    /// interval is accepted; one longer than 100 years (`Duration::MAX`, say) means never, and
    /// the listener then resyncs only after a failed read of the channel.
    pub fn resync_every(mut self, interval: Duration) -> Result<Self, AppError> {
        if interval.is_zero() {
            return Err(AppError::invalid_argument(
                "the resync interval must be greater than zero",
            ));
        }
        self.every = interval;
        Ok(self)
    }

    /// Listens until `shutdown` fires (then `Ok(())`). `ready` is called once, after the first
    /// `LISTEN` and the first load succeeded; a failure before that is the result. A fixed
    /// `Tenants` is `FAILED_PRECONDITION` without I/O. A closed control pool ends it with
    /// `UNAVAILABLE`.
    pub async fn run(
        self,
        ready: impl FnOnce() + Send,
        shutdown: CancellationToken,
    ) -> Result<(), AppError> {
        let (Some(registry), Some(hosts)) = (self.tenants.registry(), self.tenants.hosts()) else {
            return Err(AppError::failed_precondition(
                "a fixed scope has nothing to listen for",
            ));
        };
        let target = Live {
            registry: registry.clone(),
            hosts: hosts.clone(),
            tenants: self.tenants.clone(),
        };
        let sessions = PgSessions {
            pool: target.registry.pool().clone(),
            channel: self.channel.clone(),
        };
        let started = shutdown
            .run_until_cancelled(Relisten::start(sessions))
            .await;
        let Some(started) = started else {
            return Ok(());
        };
        let mut source = started?;
        let sink: &dyn Sink = self.sink.as_ref();
        drive(&mut source, &target, sink, self.every, ready, &shutdown).await
    }

    /// The listener as a runtime unit: `ready` is `UnitContext::ready`, `shutdown` the unit's
    /// token.
    #[cfg(feature = "runtime")]
    pub fn into_unit(
        self,
    ) -> impl FnMut(UnitContext) -> BoxFuture<'static, Result<(), AppError>> + Send + 'static {
        move |ctx: UnitContext| -> BoxFuture<'static, Result<(), AppError>> {
            let listener = self.clone();
            Box::pin(async move {
                let shutdown = ctx.shutdown();
                listener.run(move || ctx.ready(), shutdown).await
            })
        }
    }
}

/// `Listener::new(tenants, channel).sink(sink).run(ready, shutdown)`.
pub async fn listen<S: Send + Sync + 'static>(
    tenants: Tenants<S>,
    channel: Channel,
    sink: impl Sink,
    ready: impl FnOnce() + Send,
    shutdown: CancellationToken,
) -> Result<(), AppError> {
    Listener::new(tenants, channel)
        .sink(sink)
        .run(ready, shutdown)
        .await
}

/// What the source of the loop reports.
enum Event {
    /// A notification's payload, not parsed yet.
    Payload(String),
    /// A read failed or the connection was lost, and the channel is listened on again.
    Reconnected,
    /// Reading, or listening again, failed. A terminal failure ends the listener; any other is
    /// retried after `RETRY_AFTER`.
    Failed {
        /// Whether the control pool is closed.
        terminal: bool,
        /// What failed.
        error: AppError,
    },
}

/// Where the loop's events come from.
trait Source: Send {
    /// The next event. Cancel-safe: dropping the future loses no event.
    fn next(&mut self) -> impl Future<Output = Event> + Send;
}

/// One listening session: a `LISTEN` on one connection.
trait Session: Send {
    /// The next notification's payload. `Ok(None)` when the connection was lost, returned as
    /// soon as the loss is seen, without awaiting anything after it.
    fn recv(&mut self) -> impl Future<Output = Result<Option<String>, sqlx::Error>> + Send;
}

/// Where sessions come from.
trait Sessions: Send + Sync {
    /// What `open` hands out.
    type Session: Session;
    /// A new session on a connection of its own, already listening on the channel.
    fn open(&self) -> impl Future<Output = Result<Self::Session, sqlx::Error>> + Send;
    /// Whether the pool sessions come from is closed.
    fn is_closed(&self) -> bool;
}

/// What the loop acts on.
trait Target: Sync {
    /// Drops the tenant's cached lookups.
    fn forget(&self, tenant: TenantId);
    /// Retires the tenant's scope: it is out of the cache when this returns. Does not wait for
    /// the disposal.
    fn invalidate(&self, tenant: TenantId);
    /// Reloads the host snapshot; the active tenants read.
    fn refresh(&self) -> impl Future<Output = Result<Vec<TenantInfo>, AppError>> + Send;
    /// Drops every cached lookup.
    fn clear_lookups(&self);
    /// Empties the host snapshot.
    fn clear_snapshot(&self) -> impl Future<Output = ()> + Send;
}

/// The keyed tenant cache with its registry and snapshot.
struct Live<S> {
    /// The tenant cache.
    tenants: Tenants<S>,
    /// Its registry.
    registry: Registry,
    /// Its host snapshot.
    hosts: HostSnapshot,
}

impl<S: Send + Sync + 'static> Target for Live<S> {
    fn forget(&self, tenant: TenantId) {
        self.registry.forget(tenant);
    }

    fn invalidate(&self, tenant: TenantId) {
        // On its first poll `Tenants::invalidate` forgets the lookups and removes the entry
        // from the keyed cache's map under the map's lock, starts the disposal on a task the
        // cache tracks (its `close` and its unit wait for it), and only then awaits that task,
        // which waits for the pool's checked-out connections. One poll takes the scope out of
        // circulation; dropping the future then detaches the wait without cancelling the
        // disposal, and leaves no task of the listener's behind.
        let invalidation = std::pin::pin!(self.tenants.invalidate(tenant));
        let _ = invalidation.poll(&mut Context::from_waker(Waker::noop()));
    }

    async fn refresh(&self) -> Result<Vec<TenantInfo>, AppError> {
        self.tenants.refresh_hosts().await
    }

    fn clear_lookups(&self) {
        self.registry.clear_lookups();
    }

    async fn clear_snapshot(&self) {
        self.hosts.clear().await;
    }
}

/// Sessions on the control pool.
struct PgSessions {
    /// The pool connections are taken from.
    pool: PgPool,
    /// The channel to listen on.
    channel: Channel,
}

impl Sessions for PgSessions {
    type Session = PgListener;

    async fn open(&self) -> Result<PgListener, sqlx::Error> {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        // A lost connection is reported at once; the next session is opened by `Relisten`.
        listener.eager_reconnect(false);
        listener.listen(self.channel.as_str()).await?;
        Ok(listener)
    }

    fn is_closed(&self) -> bool {
        self.pool.is_closed()
    }
}

impl Session for PgListener {
    async fn recv(&mut self) -> Result<Option<String>, sqlx::Error> {
        let notification = self.try_recv().await?;
        Ok(notification.map(|notification| notification.payload().to_owned()))
    }
}

/// The source of the loop: one session at a time, replaced by a new one after any failed read.
struct Relisten<O: Sessions> {
    /// Where sessions come from.
    sessions: O,
    /// The session read from; `None` after a failed read or a lost connection, until a new
    /// session listens.
    session: Option<O::Session>,
}

impl<O: Sessions> Relisten<O> {
    /// The first session.
    async fn start(sessions: O) -> Result<Self, AppError> {
        let session = sessions.open().await.map_err(sekvent_db::to_app_error)?;
        Ok(Self {
            sessions,
            session: Some(session),
        })
    }

    fn failure(&mut self, error: sqlx::Error) -> Event {
        let terminal = matches!(error, sqlx::Error::PoolClosed) || self.sessions.is_closed();
        // After a read error the session may hold a dead connection: it is dropped and never
        // read again.
        self.session = None;
        Event::Failed {
            terminal,
            error: sekvent_db::to_app_error(error),
        }
    }
}

impl<O: Sessions> Source for Relisten<O> {
    async fn next(&mut self) -> Event {
        loop {
            let Some(session) = self.session.as_mut() else {
                // Dropped while opening, there is still no session and the next call opens
                // again.
                return match self.sessions.open().await {
                    Ok(session) => {
                        self.session = Some(session);
                        Event::Reconnected
                    }
                    Err(error) => self.failure(error),
                };
            };
            // A loss or a failure is recorded before anything else is awaited.
            match session.recv().await {
                Ok(Some(payload)) => return Event::Payload(payload),
                Ok(None) => self.session = None,
                Err(error) => return self.failure(error),
            }
        }
    }
}

/// What one turn of the loop does.
enum Step {
    /// The resync is due.
    Resync,
    /// The wait after a failed read is over.
    Retry,
    /// The source reported.
    Event(Event),
}

/// When the next periodic resync is due: `every` from now; `None` (never) for an interval
/// longer than `LONGEST_RESYNC`.
fn schedule(every: Duration) -> Option<Instant> {
    if every > LONGEST_RESYNC {
        return None;
    }
    Instant::now().checked_add(every)
}

/// The listener's loop: the first load, `ready`, then events until `shutdown`.
async fn drive<Src: Source, T: Target>(
    source: &mut Src,
    target: &T,
    sink: &dyn Sink,
    every: Duration,
    ready: impl FnOnce() + Send,
    shutdown: &CancellationToken,
) -> Result<(), AppError> {
    let Some(first) = shutdown.run_until_cancelled(target.refresh()).await else {
        return Ok(());
    };
    let mut state = State {
        target,
        sink,
        baseline: by_id(first?),
        failures: 0,
    };
    ready();

    let mut resync_at = schedule(every);
    let mut retry_at: Option<Instant> = None;
    loop {
        let resync = resync_at.unwrap_or_else(Instant::now);
        let retry = retry_at.unwrap_or_else(Instant::now);
        let step = tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(()),
            () = tokio::time::sleep_until(resync), if resync_at.is_some() => Step::Resync,
            () = tokio::time::sleep_until(retry), if retry_at.is_some() => Step::Retry,
            event = source.next(), if retry_at.is_none() => Step::Event(event),
        };
        let handled = match step {
            Step::Resync => {
                let done = shutdown.run_until_cancelled(state.resync()).await;
                resync_at = schedule(every);
                done
            }
            Step::Retry => {
                retry_at = None;
                Some(())
            }
            Step::Event(Event::Payload(payload)) => {
                shutdown.run_until_cancelled(state.payload(&payload)).await
            }
            Step::Event(Event::Reconnected) => {
                let done = shutdown.run_until_cancelled(state.reconnected()).await;
                resync_at = schedule(every);
                done
            }
            Step::Event(Event::Failed {
                terminal: true,
                error,
            }) => return Err(error),
            Step::Event(Event::Failed {
                terminal: false,
                error,
            }) => {
                let code = error.code().as_str();
                tracing::warn!(
                    code,
                    "reading the invalidation channel failed; reading again in 1 s"
                );
                retry_at = Some(Instant::now() + RETRY_AFTER);
                Some(())
            }
        };
        if handled.is_none() {
            return Ok(());
        }
    }
}

/// What the loop keeps between events.
struct State<'a, T> {
    /// What it acts on.
    target: &'a T,
    /// Who hears about it.
    sink: &'a dyn Sink,
    /// The active tenants as last compared.
    baseline: BTreeMap<TenantId, TenantInfo>,
    /// Failed resyncs in a row.
    failures: u32,
}

impl<T: Target> State<'_, T> {
    async fn payload(&mut self, payload: &str) {
        match Message::parse(payload) {
            Some(Message::Change { tenant, kind }) => self.change(tenant, &kind).await,
            Some(Message::Hint { tenant, topic, seq }) => self.sink.hint(tenant, &topic, seq),
            None => {
                let length = payload.len();
                tracing::debug!(length, "a notification that is not a message was ignored");
            }
        }
    }

    /// Forget, retire, reload, retire again, tell. The first retirement takes the scope out
    /// before the reload is awaited, so the cache stops handing out the scope of a tenant that
    /// changed while the control database is read. The second comes after the read that
    /// becomes the tenant's baseline, so no scope opened before that read outlives it; it runs
    /// also when the reload failed.
    async fn change(&mut self, tenant: TenantId, kind: &ChangeKind) {
        self.target.forget(tenant);
        self.target.invalidate(tenant);
        match self.target.refresh().await {
            Ok(load) => {
                if let Some(info) = load.into_iter().find(|info| info.id == tenant) {
                    self.baseline.insert(tenant, info);
                } else {
                    self.baseline.remove(&tenant);
                }
            }
            Err(error) => {
                let (code, reason) = (error.code().as_str(), error.reason().unwrap_or(""));
                tracing::warn!(
                    code,
                    reason,
                    "reloading the host snapshot after a change failed"
                );
            }
        }
        self.target.invalidate(tenant);
        self.sink.change(tenant, kind);
    }

    /// Clear, read, retire what differs from the baseline, tell.
    async fn resync(&mut self) {
        self.target.clear_lookups();
        match self.target.refresh().await {
            Ok(load) => {
                let fresh = by_id(load);
                let changed = changed(&self.baseline, &fresh);
                for &tenant in &changed {
                    self.target.invalidate(tenant);
                }
                self.baseline = fresh;
                self.failures = 0;
                self.sink.resync(&changed);
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                if self.failures >= FAILURES_BEFORE_CLEARING {
                    self.target.clear_snapshot().await;
                }
                let (code, reason) = (error.code().as_str(), error.reason().unwrap_or(""));
                tracing::warn!(
                    code,
                    reason,
                    failures = self.failures,
                    "a resync of the active tenants failed"
                );
            }
        }
    }

    async fn reconnected(&mut self) {
        self.resync().await;
        self.sink.reconnected();
    }
}

fn by_id(load: Vec<TenantInfo>) -> BTreeMap<TenantId, TenantInfo> {
    load.into_iter().map(|info| (info.id, info)).collect()
}

/// Every tenant whose info differs, that is new, or that is gone; sorted.
fn changed(
    old: &BTreeMap<TenantId, TenantInfo>,
    new: &BTreeMap<TenantId, TenantInfo>,
) -> Vec<TenantId> {
    let differs = new
        .iter()
        .filter(|&(id, info)| old.get(id) != Some(info))
        .map(|(id, _)| *id);
    let gone = old.keys().filter(|id| !new.contains_key(*id)).copied();
    let mut changed: Vec<TenantId> = differs.chain(gone).collect();
    changed.sort_unstable();
    changed
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use sekvent_error::ErrorCode;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::sync::{mpsc, oneshot};
    use uuid::Uuid;

    use super::*;
    use crate::{RoleKey, TenantState};

    const EVERY: Duration = Duration::from_secs(60);
    /// The bound of one scenario on the paused clock.
    const LONG: Duration = Duration::from_hours(24);

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Forget(TenantId),
        Invalidate(TenantId),
        Refresh,
        ClearLookups,
        ClearSnapshot,
        Ready,
        Change(TenantId, ChangeKind),
        Hint(TenantId, Topic, i64),
        Resync(Vec<TenantId>),
        Reconnected,
    }

    /// Every call of the target, the sink and `ready`, with the time it was made.
    #[derive(Clone)]
    struct Log(mpsc::UnboundedSender<(Instant, Call)>);

    impl Log {
        fn record(&self, call: Call) {
            let _ = self.0.send((Instant::now(), call));
        }
    }

    /// What one `refresh` of the fake target does.
    enum Load {
        World,
        Fail,
        /// Reads the world once the gate opens.
        Gated(oneshot::Receiver<()>),
        /// Fails once the gate opens.
        GatedFail(oneshot::Receiver<()>),
        /// Reads the world after this long.
        Slow(Duration),
        /// Waits for `before`, reads the world, tells `read`, waits for `after`, returns.
        Around {
            before: oneshot::Receiver<()>,
            read: oneshot::Sender<()>,
            after: oneshot::Receiver<()>,
        },
    }

    /// The warm scopes, each with the info it was opened from.
    type Scopes = Arc<Mutex<BTreeMap<TenantId, TenantInfo>>>;

    struct FakeTarget {
        log: Log,
        world: Arc<Mutex<Vec<TenantInfo>>>,
        loads: Arc<Mutex<VecDeque<Load>>>,
        scopes: Scopes,
    }

    impl FakeTarget {
        fn read(&self) -> Vec<TenantInfo> {
            self.world.lock().unwrap().clone()
        }
    }

    impl Target for FakeTarget {
        fn forget(&self, tenant: TenantId) {
            self.log.record(Call::Forget(tenant));
        }

        fn invalidate(&self, tenant: TenantId) {
            self.scopes.lock().unwrap().remove(&tenant);
            self.log.record(Call::Invalidate(tenant));
        }

        async fn refresh(&self) -> Result<Vec<TenantInfo>, AppError> {
            self.log.record(Call::Refresh);
            let load = self.loads.lock().unwrap().pop_front();
            match load.unwrap_or(Load::World) {
                Load::World => {}
                Load::Fail => return Err(AppError::unavailable("the control database is away")),
                Load::Gated(gate) => {
                    let _ = gate.await;
                }
                Load::GatedFail(gate) => {
                    let _ = gate.await;
                    return Err(AppError::unavailable("the control database is away"));
                }
                Load::Slow(after) => tokio::time::sleep(after).await,
                Load::Around {
                    before,
                    read,
                    after,
                } => {
                    let _ = before.await;
                    let world = self.read();
                    let _ = read.send(());
                    let _ = after.await;
                    return Ok(world);
                }
            }
            Ok(self.read())
        }

        fn clear_lookups(&self) {
            self.log.record(Call::ClearLookups);
        }

        async fn clear_snapshot(&self) {
            self.log.record(Call::ClearSnapshot);
        }
    }

    struct Recorder(Log);

    impl Sink for Recorder {
        fn change(&self, tenant: TenantId, kind: &ChangeKind) {
            self.0.record(Call::Change(tenant, kind.clone()));
        }

        fn hint(&self, tenant: TenantId, topic: &Topic, seq: i64) {
            self.0.record(Call::Hint(tenant, topic.clone(), seq));
        }

        fn resync(&self, changed: &[TenantId]) {
            self.0.record(Call::Resync(changed.to_vec()));
        }

        fn reconnected(&self) {
            self.0.record(Call::Reconnected);
        }
    }

    struct FakeSource {
        events: mpsc::UnboundedReceiver<Event>,
        polled: mpsc::UnboundedSender<Instant>,
    }

    impl Source for FakeSource {
        async fn next(&mut self) -> Event {
            let _ = self.polled.send(Instant::now());
            match self.events.recv().await {
                Some(event) => event,
                None => std::future::pending().await,
            }
        }
    }

    /// A source that always has a payload ready.
    struct Flood(String);

    impl Source for Flood {
        async fn next(&mut self) -> Event {
            tokio::task::yield_now().await;
            Event::Payload(self.0.clone())
        }
    }

    struct Rig {
        target: FakeTarget,
        sink: Recorder,
        log: Log,
        shutdown: CancellationToken,
    }

    /// The test's side of a scenario.
    struct Script {
        calls: mpsc::UnboundedReceiver<(Instant, Call)>,
        events: mpsc::UnboundedSender<Event>,
        polled: mpsc::UnboundedReceiver<Instant>,
        shutdown: CancellationToken,
        world: Arc<Mutex<Vec<TenantInfo>>>,
        scopes: Scopes,
        begin: Instant,
    }

    impl Script {
        async fn expect(&mut self, want: Call) -> Instant {
            let (at, call) = self.calls.recv().await.expect("a call was recorded");
            assert_eq!(call, want);
            at
        }

        /// The next call that is not a hint, which must be `want`.
        async fn until(&mut self, want: Call) -> Instant {
            loop {
                let (at, call) = self.calls.recv().await.expect("a call was recorded");
                if !matches!(call, Call::Hint(..)) {
                    assert_eq!(call, want);
                    return at;
                }
            }
        }

        async fn polled(&mut self) -> Instant {
            self.polled.recv().await.expect("the source was polled")
        }

        fn send(&self, event: Event) {
            assert!(self.events.send(event).is_ok(), "the source is open");
        }

        fn payload(&self, text: String) {
            self.send(Event::Payload(text));
        }

        fn set_world(&self, world: Vec<TenantInfo>) {
            *self.world.lock().unwrap() = world;
        }

        /// A request opens `tenant`'s scope from what the world holds now.
        fn reopen(&self, tenant: TenantId) {
            let info = self
                .world
                .lock()
                .unwrap()
                .iter()
                .find(|info| info.id == tenant)
                .cloned()
                .expect("the tenant is active");
            self.scopes.lock().unwrap().insert(tenant, info);
        }

        /// The info `tenant`'s warm scope was opened from.
        fn scope(&self, tenant: TenantId) -> Option<TenantInfo> {
            self.scopes.lock().unwrap().get(&tenant).cloned()
        }

        fn stop(&self) {
            self.shutdown.cancel();
        }
    }

    fn rig(world: Vec<TenantInfo>, loads: Vec<Load>) -> (Rig, FakeSource, Script) {
        let (log, calls) = mpsc::unbounded_channel();
        let log = Log(log);
        let (events_tx, events) = mpsc::unbounded_channel();
        let (polled_tx, polled) = mpsc::unbounded_channel();
        let world = Arc::new(Mutex::new(world));
        let scopes = Scopes::default();
        let shutdown = CancellationToken::new();
        let rig = Rig {
            target: FakeTarget {
                log: log.clone(),
                world: Arc::clone(&world),
                loads: Arc::new(Mutex::new(loads.into())),
                scopes: Arc::clone(&scopes),
            },
            sink: Recorder(log.clone()),
            log,
            shutdown: shutdown.clone(),
        };
        let source = FakeSource {
            events,
            polled: polled_tx,
        };
        let script = Script {
            calls,
            events: events_tx,
            polled,
            shutdown,
            world,
            scopes,
            begin: Instant::now(),
        };
        (rig, source, script)
    }

    /// Runs the loop and `script` together; the calls the script left unread.
    async fn play(
        mut source: impl Source,
        rig: Rig,
        every: Duration,
        script: impl Future<Output = Script>,
    ) -> (Result<(), AppError>, Vec<Call>) {
        let ready = {
            let log = rig.log.clone();
            move || log.record(Call::Ready)
        };
        let driven = drive(
            &mut source,
            &rig.target,
            &rig.sink,
            every,
            ready,
            &rig.shutdown,
        );
        let both = async { tokio::join!(driven, script) };
        let (result, mut script) = tokio::time::timeout(LONG, both)
            .await
            .expect("the scenario ended");
        let mut rest = Vec::new();
        while let Ok((_, call)) = script.calls.try_recv() {
            rest.push(call);
        }
        (result, rest)
    }

    fn tenant(n: u128) -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(n))
    }

    fn info(n: u128, primary: &str, hosts: &[&str]) -> TenantInfo {
        let mut hosts: Vec<String> = hosts.iter().map(|host| (*host).to_owned()).collect();
        hosts.sort();
        TenantInfo {
            id: tenant(n),
            subdomain: format!("t{n}"),
            display_name: format!("Tenant {n}"),
            state: TenantState::Active,
            primary_host: primary.to_owned(),
            hosts,
            role_key: RoleKey {
                key_id: "k1".to_owned(),
                generation: 1,
            },
        }
    }

    fn one_host(n: u128) -> TenantInfo {
        let host = format!("t{n}.example.com");
        info(n, &host, &[host.as_str()])
    }

    fn topic(text: &str) -> Topic {
        Topic::new(text).unwrap()
    }

    fn channel() -> Channel {
        Channel::new("app_changes").unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_load_is_the_baseline_and_ready_follows_it() {
        let (rig, source, mut s) = rig(vec![one_host(1), one_host(2)], vec![]);
        let (result, rest) = play(source, rig, EVERY, async move {
            let loaded = s.expect(Call::Refresh).await;
            let ready = s.expect(Call::Ready).await;
            assert!(ready >= loaded);
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_first_load_is_the_result_and_ready_is_never_called() {
        let (rig, source, mut s) = rig(vec![], vec![Load::Fail]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s
        })
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "the control database is away");
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_forgets_retires_reloads_retires_again_and_then_tells_the_sink() {
        let a = tenant(1);
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.payload(format!("t:{a}:state"));
            s.expect(Call::Forget(a)).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Change(a, ChangeKind::STATE)).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_retires_the_scope_before_the_reload_and_again_after_it() {
        let a = tenant(1);
        let (open, gate) = oneshot::channel();
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![Load::World, Load::Gated(gate)]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.reopen(a);
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.payload(format!("t:{a}:state"));
            s.expect(Call::Forget(a)).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Refresh).await;
            // The reload is still waiting and the scope is already out.
            assert_eq!(s.scope(a), None);
            // A request reopens it while the control database is read.
            s.reopen(a);
            assert_eq!(s.scope(a), Some(one_host(1)));
            open.send(()).unwrap();
            s.expect(Call::Invalidate(a)).await;
            assert_eq!(s.scope(a), None);
            s.expect(Call::Change(a, ChangeKind::STATE)).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_whose_reload_fails_retires_the_scope_before_and_after_it() {
        let a = tenant(1);
        let (open, gate) = oneshot::channel();
        let loads = vec![Load::World, Load::GatedFail(gate)];
        let (rig, source, mut s) = rig(vec![one_host(1)], loads);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.reopen(a);
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.payload(format!("t:{a}:state"));
            s.expect(Call::Forget(a)).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Refresh).await;
            assert_eq!(s.scope(a), None);
            s.reopen(a);
            open.send(()).unwrap();
            // The reload fails; the scope reopened during it is retired all the same.
            s.expect(Call::Invalidate(a)).await;
            assert_eq!(s.scope(a), None);
            s.expect(Call::Change(a, ChangeKind::STATE)).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_whose_reload_fails_still_reaches_the_sink_and_is_no_failed_resync() {
        let a = tenant(1);
        let plan = ChangeKind::new("plan").unwrap();
        let loads = vec![Load::World, Load::Fail, Load::Fail];
        let (rig, source, mut s) = rig(vec![one_host(1)], loads);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.payload(format!("t:{a}:plan"));
            s.expect(Call::Forget(a)).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Change(a, plan)).await;
            // The first resync fails: one failure, so the snapshot stays.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_change_the_baseline_moves_for_that_tenant_only() {
        let (a, b) = (tenant(1), tenant(2));
        let (rig, source, mut s) = rig(vec![one_host(1), one_host(2)], vec![]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.set_world(vec![
                info(1, "t1.example.com", &["t1.example.com", "a.example.com"]),
                info(2, "t2.example.com", &["t2.example.com", "b.example.com"]),
            ]);
            s.payload(format!("t:{a}:hosts"));
            s.expect(Call::Forget(a)).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Change(a, ChangeKind::HOSTS)).await;
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Invalidate(b)).await;
            s.expect(Call::Resync(vec![b])).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_of_a_tenant_that_left_drops_it_from_the_baseline() {
        let a = tenant(1);
        let (rig, source, mut s) = rig(vec![one_host(1), one_host(2)], vec![]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.set_world(vec![one_host(2)]);
            s.payload(format!("t:{a}:state"));
            s.expect(Call::Forget(a)).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Change(a, ChangeKind::STATE)).await;
            // `a` already left the baseline, `b` did not change: nothing to report.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_hint_reaches_the_sink_alone_and_a_payload_that_is_no_message_is_ignored() {
        let a = tenant(1);
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.payload(format!("e:{a}:orders:7"));
            s.expect(Call::Hint(a, topic("orders"), 7)).await;
            s.payload("not a message".to_owned());
            s.payload(format!("t:{a}:State"));
            s.payload(format!("e:{a}:orders:-1"));
            s.payload(format!("e:{a}:orders:8"));
            s.expect(Call::Hint(a, topic("orders"), 8)).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_resync_reports_each_tenant_that_changed_joined_or_left_in_order() {
        let before = vec![
            info(2, "t2.example.com", &["t2.example.com"]),
            info(3, "t3.example.com", &["t3.example.com"]),
            info(
                4,
                "t4.example.com",
                &["t4.example.com", "www.t4.example.com"],
            ),
            info(5, "t5.example.com", &["t5.example.com"]),
        ];
        let after = vec![
            // joined
            info(1, "t1.example.com", &["t1.example.com"]),
            // 2 left; 3 gained a host with its state unchanged
            info(
                3,
                "t3.example.com",
                &["t3.example.com", "shop.t3.example.com"],
            ),
            // the primary moved within the same hosts
            info(
                4,
                "www.t4.example.com",
                &["t4.example.com", "www.t4.example.com"],
            ),
            // unchanged
            info(5, "t5.example.com", &["t5.example.com"]),
        ];
        let changed = vec![tenant(1), tenant(2), tenant(3), tenant(4)];
        let (rig, source, mut s) = rig(before, vec![]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.set_world(after);
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            for &id in &changed {
                s.expect(Call::Invalidate(id)).await;
            }
            s.expect(Call::Resync(changed)).await;
            // The load became the baseline.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_tick_is_an_interval_after_ready_and_each_next_one_after_a_resync() {
        let (open, gate) = oneshot::channel();
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![Load::World, Load::Gated(gate)]);
        let (result, rest) = play(source, rig, EVERY, async move {
            let begin = s.begin;
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            let first = s.expect(Call::ClearLookups).await;
            assert_eq!(first - begin, EVERY);
            s.expect(Call::Refresh).await;
            tokio::time::advance(EVERY * 3 + EVERY / 2).await;
            open.send(()).unwrap();
            let done = s.expect(Call::Resync(vec![])).await;
            assert_eq!(done - begin, EVERY * 4 + EVERY / 2);
            // The ticks the load outlasted are not replayed: the next is an interval away.
            let next = s.expect(Call::ClearLookups).await;
            assert_eq!(next - done, EVERY);
            s.expect(Call::Refresh).await;
            let after = s.expect(Call::Resync(vec![])).await;
            let last = s.expect(Call::ClearLookups).await;
            assert_eq!(last - after, EVERY);
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_that_came_due_while_a_change_was_handled_runs_once_at_once() {
        let a = tenant(1);
        let (open, gate) = oneshot::channel();
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![Load::World, Load::Gated(gate)]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.payload(format!("t:{a}:state"));
            s.expect(Call::Forget(a)).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Refresh).await;
            tokio::time::advance(EVERY * 3 + EVERY / 2).await;
            open.send(()).unwrap();
            s.expect(Call::Invalidate(a)).await;
            let told = s.expect(Call::Change(a, ChangeKind::STATE)).await;
            let late = s.expect(Call::ClearLookups).await;
            assert_eq!(late, told);
            s.expect(Call::Refresh).await;
            let done = s.expect(Call::Resync(vec![])).await;
            let next = s.expect(Call::ClearLookups).await;
            assert_eq!(next - done, EVERY);
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_resync_slower_than_the_interval_still_lets_notifications_through() {
        let a = tenant(1);
        let slow = || Load::Slow(EVERY * 2);
        let loads = vec![Load::World, slow(), slow(), slow()];
        let (rig, _source, mut s) = rig(vec![one_host(1)], loads);
        let flood = Flood(format!("e:{a}:orders:1"));
        let hint = Call::Hint(a, topic("orders"), 1);
        let (result, _rest) = play(flood, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.expect(hint.clone()).await;
            for _ in 0..2 {
                // The source never lets the runtime idle, so the clock moves only by hand
                // between resyncs; during one, the slow load lets it idle.
                tokio::time::advance(EVERY).await;
                let tick = s.until(Call::ClearLookups).await;
                s.expect(Call::Refresh).await;
                let done = s.expect(Call::Resync(vec![])).await;
                assert_eq!(done - tick, EVERY * 2);
                // The next resync is a whole interval away: the source is read first.
                s.expect(hint.clone()).await;
            }
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn an_interval_longer_than_a_century_never_ticks_and_never_panics() {
        assert_eq!(schedule(Duration::MAX), None);
        assert_eq!(schedule(LONGEST_RESYNC + Duration::from_nanos(1)), None);
        assert_eq!(
            schedule(LONGEST_RESYNC),
            Some(Instant::now() + LONGEST_RESYNC)
        );
        let listener = Listener::new(Tenants::fixed(()), channel())
            .resync_every(Duration::MAX)
            .unwrap();
        assert_eq!(listener.every, Duration::MAX);

        let a = tenant(1);
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![]);
        let (result, rest) = play(source, rig, Duration::MAX, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            tokio::time::advance(Duration::from_hours(23)).await;
            s.payload(format!("e:{a}:orders:1"));
            s.expect(Call::Hint(a, topic("orders"), 1)).await;
            // A reconnect still resyncs, and schedules no tick either.
            s.send(Event::Reconnected);
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.expect(Call::Reconnected).await;
            s.payload(format!("e:{a}:orders:2"));
            s.expect(Call::Hint(a, topic("orders"), 2)).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    /// Tenant 1 as of its `n`th version.
    fn version(n: u32) -> TenantInfo {
        TenantInfo {
            display_name: format!("Tenant 1, version {n}"),
            ..one_host(1)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_scope_reopened_while_a_change_is_handled_never_hides_a_missed_change() {
        let a = tenant(1);
        let (open_before, before) = oneshot::channel();
        let (read, was_read) = oneshot::channel();
        let (open_after, after) = oneshot::channel();
        let loads = vec![
            Load::World,
            Load::Around {
                before,
                read,
                after,
            },
        ];
        let (rig, source, mut s) = rig(vec![version(1)], loads);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.reopen(a);
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.set_world(vec![version(2)]);
            s.payload(format!("t:{a}:state"));
            s.expect(Call::Forget(a)).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Refresh).await;
            // The scope opened before the change is out before the reload reads.
            assert_eq!(s.scope(a), None);
            // Before the read: a request reopens the scope, then a change commits whose
            // notification is lost.
            s.reopen(a);
            s.set_world(vec![version(3)]);
            open_before.send(()).unwrap();
            was_read.await.unwrap();
            // After the read, before the scope is retired again: the same again.
            s.reopen(a);
            s.set_world(vec![version(4)]);
            open_after.send(()).unwrap();
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Change(a, ChangeKind::STATE)).await;
            // No scope opened before the second retirement survives it.
            assert_eq!(s.scope(a), None);
            // After it: a request opens the scope, and a change whose notification is lost
            // leaves that scope stale.
            s.reopen(a);
            s.set_world(vec![version(5)]);
            assert_eq!(s.scope(a), Some(version(4)));
            // The baseline is the change's read (version 3): the resync sees the difference.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Resync(vec![a])).await;
            assert_eq!(s.scope(a), None);
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn the_timer_wins_over_a_source_that_always_has_a_payload() {
        let a = tenant(1);
        let (rig, _source, mut s) = rig(vec![one_host(1)], vec![]);
        let flood = Flood(format!("e:{a}:orders:1"));
        let (result, _rest) = play(flood, rig, EVERY, async move {
            let begin = s.begin;
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.expect(Call::Hint(a, topic("orders"), 1)).await;
            // The source never lets the runtime idle, so the clock moves only by hand.
            for round in 1..=2_u32 {
                tokio::time::advance(EVERY).await;
                let at = s.until(Call::ClearLookups).await;
                assert!(at - begin >= EVERY * round);
                s.until(Call::Refresh).await;
                s.until(Call::Resync(vec![])).await;
            }
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn one_failed_resync_keeps_the_snapshot_and_the_second_in_a_row_clears_it() {
        let a = tenant(1);
        let loads = vec![Load::World, Load::Fail, Load::World, Load::Fail, Load::Fail];
        let (rig, source, mut s) = rig(vec![one_host(1)], loads);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.set_world(vec![info(
                1,
                "t1.example.com",
                &["t1.example.com", "x.example.com"],
            )]);
            // One failure: the snapshot and the baseline stay.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            // A success: compared with the kept baseline, and the count is reset.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Invalidate(a)).await;
            s.expect(Call::Resync(vec![a])).await;
            // One failure, then a second in a row: the snapshot is emptied.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::ClearSnapshot).await;
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_resync_has_no_deadline_and_its_load_counts_once_it_returns() {
        let (open, gate) = oneshot::channel();
        let loads = vec![Load::World, Load::Gated(gate), Load::Fail];
        let (rig, source, mut s) = rig(vec![one_host(1)], loads);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            tokio::time::advance(EVERY * 10).await;
            assert_eq!(s.calls.try_recv(), Err(TryRecvError::Empty));
            open.send(()).unwrap();
            s.expect(Call::Resync(vec![])).await;
            // The next load fails: the first failure in a row, so nothing is cleared.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_the_listener_while_a_resync_load_is_awaited() {
        let (open, gate) = oneshot::channel();
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![Load::World, Load::Gated(gate)]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            tokio::time::advance(EVERY * 5).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
        // The load was still waiting at its gate when the listener ended.
        drop(open);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_the_first_load_ends_the_listener_without_ready() {
        let (open, gate) = oneshot::channel();
        let (rig, source, mut s) = rig(vec![], vec![Load::Gated(gate)]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
        // The load was still waiting at its gate when the listener ended.
        drop(open);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_resyncs_and_then_tells_the_sink_also_when_that_resync_fails() {
        let loads = vec![Load::World, Load::World, Load::Fail];
        let (rig, source, mut s) = rig(vec![one_host(1)], loads);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.send(Event::Reconnected);
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.expect(Call::Reconnected).await;
            s.send(Event::Reconnected);
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Reconnected).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_read_is_retried_after_a_second_and_a_terminal_one_ends_the_listener() {
        let every = Duration::from_millis(600);
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![]);
        let (result, rest) = play(source, rig, every, async move {
            let begin = s.begin;
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            let first = s.polled().await;
            assert_eq!(first, begin);
            s.send(Event::Failed {
                terminal: false,
                error: AppError::unavailable("the connection is away"),
            });
            let tick = s.expect(Call::ClearLookups).await;
            assert_eq!(tick - begin, every);
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            let again = s.polled().await;
            assert_eq!(again - first, RETRY_AFTER);
            s.send(Event::Failed {
                terminal: true,
                error: AppError::unavailable("the pool is closed"),
            });
            s
        })
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "the pool is closed");
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_the_listener_during_the_wait_after_a_failed_read() {
        let every = Duration::from_millis(600);
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![]);
        let (result, rest) = play(source, rig, every, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.polled().await;
            s.send(Event::Failed {
                terminal: false,
                error: AppError::unavailable("the connection is away"),
            });
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.stop();
            assert_eq!(s.polled.try_recv(), Err(TryRecvError::Empty));
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_a_waiting_listener() {
        let (rig, source, mut s) = rig(vec![one_host(1)], vec![]);
        let (result, rest) = play(source, rig, EVERY, async move {
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            s.polled().await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    /// What one `open` of the fake sessions does.
    enum Open {
        Ok,
        Fail,
        Closed,
        Hang,
    }

    /// What one `recv` of a fake session does.
    enum Recv {
        Payload(&'static str),
        Lost,
        Fail,
        Hang,
    }

    /// What the fake sessions were used for, in order: the number of the session opened, read
    /// or dropped.
    #[derive(Debug, PartialEq, Eq)]
    enum Used {
        Opened(usize),
        Read(usize),
        Dropped(usize),
    }

    #[derive(Clone)]
    struct Scripted {
        opens: Arc<Mutex<VecDeque<Open>>>,
        recvs: Arc<Mutex<VecDeque<Recv>>>,
        used: mpsc::UnboundedSender<Used>,
    }

    struct FakeSessions {
        script: Scripted,
        opened: AtomicUsize,
        closed: AtomicBool,
    }

    struct FakeSession {
        n: usize,
        script: Scripted,
    }

    impl Drop for FakeSession {
        fn drop(&mut self) {
            let _ = self.script.used.send(Used::Dropped(self.n));
        }
    }

    impl Session for FakeSession {
        async fn recv(&mut self) -> Result<Option<String>, sqlx::Error> {
            let _ = self.script.used.send(Used::Read(self.n));
            let turn = self.script.recvs.lock().unwrap().pop_front();
            match turn.unwrap_or(Recv::Hang) {
                Recv::Payload(text) => Ok(Some(text.to_owned())),
                Recv::Lost => Ok(None),
                Recv::Fail => Err(sqlx::Error::Protocol("a fake read error".to_owned())),
                Recv::Hang => std::future::pending().await,
            }
        }
    }

    impl Sessions for FakeSessions {
        type Session = FakeSession;

        async fn open(&self) -> Result<FakeSession, sqlx::Error> {
            let turn = self.script.opens.lock().unwrap().pop_front();
            match turn.unwrap_or(Open::Ok) {
                Open::Ok => {}
                Open::Fail => return Err(sqlx::Error::PoolTimedOut),
                Open::Closed => return Err(sqlx::Error::PoolClosed),
                Open::Hang => std::future::pending::<()>().await,
            }
            let n = self.opened.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = self.script.used.send(Used::Opened(n));
            Ok(FakeSession {
                n,
                script: self.script.clone(),
            })
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    /// Sessions that follow `opens` (the first is the start's) and whose reads, across every
    /// session, follow `recvs`; an exhausted script opens and hangs.
    fn sessions(
        opens: Vec<Open>,
        recvs: Vec<Recv>,
    ) -> (FakeSessions, mpsc::UnboundedReceiver<Used>) {
        let (used, seen) = mpsc::unbounded_channel();
        let fake = FakeSessions {
            script: Scripted {
                opens: Arc::new(Mutex::new(opens.into())),
                recvs: Arc::new(Mutex::new(recvs.into())),
                used,
            },
            opened: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        };
        (fake, seen)
    }

    fn uses(seen: &mut mpsc::UnboundedReceiver<Used>) -> Vec<Used> {
        let mut all = Vec::new();
        while let Ok(one) = seen.try_recv() {
            all.push(one);
        }
        all
    }

    fn payload_of(event: Event) -> String {
        match event {
            Event::Payload(payload) => payload,
            Event::Reconnected | Event::Failed { .. } => panic!("a payload was expected"),
        }
    }

    /// Whether the failure reported is terminal.
    fn terminal(event: &Event) -> bool {
        match *event {
            Event::Failed { terminal, .. } => terminal,
            Event::Payload(_) | Event::Reconnected => panic!("a failure was expected"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_connection_is_reported_and_listened_on_again_on_a_new_session() {
        let recvs = vec![Recv::Payload("one"), Recv::Lost, Recv::Payload("two")];
        let (fake, mut used) = sessions(vec![], recvs);
        let Ok(mut source) = Relisten::start(fake).await else {
            panic!("the first session opens");
        };
        assert_eq!(payload_of(source.next().await), "one");
        assert!(matches!(source.next().await, Event::Reconnected));
        assert_eq!(payload_of(source.next().await), "two");
        assert_eq!(
            uses(&mut used),
            [
                Used::Opened(1),
                Used::Read(1),
                Used::Read(1),
                Used::Dropped(1),
                Used::Opened(2),
                Used::Read(2),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_failed_read_a_new_session_replaces_the_old_one_once_it_listens() {
        let opens = vec![Open::Ok, Open::Fail, Open::Ok];
        let (fake, mut used) = sessions(opens, vec![Recv::Fail, Recv::Payload("three")]);
        let Ok(mut source) = Relisten::start(fake).await else {
            panic!("the first session opens");
        };
        // The read fails, then listening again fails: each is a failure that is retried.
        assert!(!terminal(&source.next().await));
        assert!(!terminal(&source.next().await));
        assert!(matches!(source.next().await, Event::Reconnected));
        assert_eq!(payload_of(source.next().await), "three");
        // The session whose read failed is dropped at once and never read again.
        assert_eq!(
            uses(&mut used),
            [
                Used::Opened(1),
                Used::Read(1),
                Used::Dropped(1),
                Used::Opened(2),
                Used::Read(2),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_pool_ends_the_source_whether_it_reads_or_listens_again() {
        let (fake, _used) = sessions(vec![Open::Closed], vec![]);
        let Err(error) = Relisten::start(fake).await else {
            panic!("a closed pool cannot listen");
        };
        assert_eq!(error.code(), ErrorCode::Unavailable);

        let (fake, _used) = sessions(vec![Open::Ok, Open::Closed], vec![Recv::Lost]);
        let Ok(mut source) = Relisten::start(fake).await else {
            panic!("the first session opens");
        };
        assert!(terminal(&source.next().await));

        let (fake, _used) = sessions(vec![], vec![Recv::Fail]);
        let Ok(mut open) = Relisten::start(fake).await else {
            panic!("the first session opens");
        };
        assert!(!terminal(&open.next().await));
        let (fake, _used) = sessions(vec![], vec![Recv::Fail]);
        let Ok(mut closed) = Relisten::start(fake).await else {
            panic!("the first session opens");
        };
        closed.sessions.closed.store(true, Ordering::SeqCst);
        assert!(terminal(&closed.next().await));
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_dropped_while_listening_again_still_reports_the_loss() {
        let opens = vec![Open::Ok, Open::Hang, Open::Ok];
        let (fake, mut used) = sessions(opens, vec![Recv::Lost, Recv::Payload("four")]);
        let Ok(mut source) = Relisten::start(fake).await else {
            panic!("the first session opens");
        };
        // The loss is seen, listening again hangs, and the read is dropped.
        let dropped = tokio::time::timeout(Duration::from_secs(1), source.next()).await;
        assert!(dropped.is_err(), "listening again hangs");
        assert!(matches!(source.next().await, Event::Reconnected));
        assert_eq!(payload_of(source.next().await), "four");
        assert_eq!(
            uses(&mut used),
            [
                Used::Opened(1),
                Used::Read(1),
                Used::Dropped(1),
                Used::Opened(2),
                Used::Read(2),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_that_interrupts_a_read_never_hides_a_lost_connection() {
        let (fake, _used) = sessions(vec![Open::Ok, Open::Hang, Open::Ok], vec![Recv::Lost]);
        let Ok(source) = Relisten::start(fake).await else {
            panic!("the first session opens");
        };
        let (rig, _unused, mut s) = rig(vec![one_host(1)], vec![]);
        let (result, rest) = play(source, rig, EVERY, async move {
            let begin = s.begin;
            s.expect(Call::Refresh).await;
            s.expect(Call::Ready).await;
            // The loss is seen at once; listening again hangs until the tick drops the read.
            let tick = s.expect(Call::ClearLookups).await;
            assert_eq!(tick - begin, EVERY);
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            // The next read listens again and reports the loss.
            s.expect(Call::ClearLookups).await;
            s.expect(Call::Refresh).await;
            s.expect(Call::Resync(vec![])).await;
            s.expect(Call::Reconnected).await;
            s.stop();
            s
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(rest, Vec::<Call>::new());
    }

    #[tokio::test]
    async fn a_fixed_scope_is_refused_before_any_io_and_ready_is_not_called() {
        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        let error = Listener::new(Tenants::fixed(()), channel())
            .run(
                move || flag.store(true, Ordering::SeqCst),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.message(), "a fixed scope has nothing to listen for");

        let flag = Arc::clone(&called);
        let error = listen(
            Tenants::fixed(()),
            channel(),
            (),
            move || flag.store(true, Ordering::SeqCst),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn the_resync_interval_must_not_be_zero_and_the_listener_prints_its_type_name() {
        let listener = Listener::new(Tenants::fixed(()), channel());
        assert_eq!(listener.every, DEFAULT_RESYNC);
        assert_eq!(format!("{listener:?}"), "Listener");
        let error = listener.clone().resync_every(Duration::ZERO).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        let listener = listener.resync_every(Duration::from_secs(5)).unwrap();
        assert_eq!(listener.every, Duration::from_secs(5));
        assert_eq!(listener.clone().every, Duration::from_secs(5));
    }

    #[test]
    fn the_unit_sink_accepts_every_call() {
        let sink: &dyn Sink = &();
        sink.change(tenant(1), &ChangeKind::STATE);
        sink.hint(tenant(1), &topic("orders"), 1);
        sink.resync(&[tenant(1)]);
        sink.reconnected();
    }

    #[tokio::test]
    async fn a_sink_given_as_an_arc_hears_every_call_through_the_shared_value() {
        let (log, mut calls) = mpsc::unbounded_channel();
        let shared = Arc::new(Recorder(Log(log)));
        let a = tenant(1);

        let listener = Listener::new(Tenants::fixed(()), channel()).sink(Arc::clone(&shared));
        listener.sink.change(a, &ChangeKind::STATE);
        listener.sink.hint(a, &topic("orders"), 3);
        listener.sink.resync(&[a]);
        listener.sink.reconnected();

        let dynamic: Arc<dyn Sink> = shared;
        let listener = Listener::new(Tenants::fixed(()), channel()).sink(Arc::clone(&dynamic));
        listener.sink.change(a, &ChangeKind::HOSTS);
        listener.sink.hint(a, &topic("stock"), 4);
        listener.sink.resync(&[]);
        listener.sink.reconnected();

        let mut heard = Vec::new();
        while let Ok((_, call)) = calls.try_recv() {
            heard.push(call);
        }
        assert_eq!(
            heard,
            [
                Call::Change(a, ChangeKind::STATE),
                Call::Hint(a, topic("orders"), 3),
                Call::Resync(vec![a]),
                Call::Reconnected,
                Call::Change(a, ChangeKind::HOSTS),
                Call::Hint(a, topic("stock"), 4),
                Call::Resync(vec![]),
                Call::Reconnected,
            ]
        );
    }

    #[test]
    fn the_comparison_names_differing_new_and_gone_tenants_sorted() {
        let old = by_id(vec![one_host(3), one_host(1), one_host(2)]);
        let new = by_id(vec![
            one_host(4),
            info(2, "t2.example.com", &["t2.example.com", "b.example.com"]),
            one_host(3),
        ]);
        assert_eq!(changed(&old, &new), [tenant(1), tenant(2), tenant(4)]);
        assert_eq!(changed(&new, &new), Vec::<TenantId>::new());
    }
}
