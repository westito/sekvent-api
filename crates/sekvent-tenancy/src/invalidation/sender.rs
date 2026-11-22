//! The sender of the invalidation channel: queues notifications and never makes the caller wait.
//!
//! `change` and `hint` put a message into a queue under a mutex held for a few instructions and
//! start a drain task when none is running. The drain takes a control connection first, then the
//! whole queue, and sends it with one statement; then it looks at the queue again and stops when
//! it is empty. A failed acquire or statement drops what that round took, with one warning:
//! nothing is retried, because the listener's next successful resync repairs a lost notification.
//!
//! The flag that says a drain runs is cleared under the queue's mutex when the drain ends
//! normally, and otherwise by a guard the drain's task owns from the moment it is spawned: a
//! drain that panics, or whose task is dropped before or while it runs (its runtime shut down),
//! never keeps a later call from starting another.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres};

use super::message::{ChangeKind, Channel, Message, Topic};
use crate::TenantId;

/// One statement for a whole batch: every payload on the channel, in the order given.
const NOTIFY: &str = "SELECT pg_notify($1, payload) \
     FROM unnest($2::text[]) WITH ORDINALITY AS batch(payload, n) \
     ORDER BY n";

/// Why an acquire or a statement failed: the class of the error, never its text.
type Failure = &'static str;

/// How a drain reaches the database: a connection first, then a batch on it.
trait Outlet: Send + Sync + 'static {
    /// What `acquire` hands out.
    type Conn: Send;

    /// A connection to send on.
    fn acquire(&self) -> impl Future<Output = Result<Self::Conn, Failure>> + Send;

    /// Sends `payloads`, in order, with one statement.
    fn send(
        &self,
        conn: &mut Self::Conn,
        payloads: Vec<String>,
    ) -> impl Future<Output = Result<(), Failure>> + Send;
}

/// The control pool and the channel.
struct PoolOutlet {
    /// The control pool.
    pool: PgPool,
    /// The channel every payload goes to.
    channel: Channel,
}

impl Outlet for PoolOutlet {
    type Conn = PoolConnection<Postgres>;

    async fn acquire(&self) -> Result<Self::Conn, Failure> {
        self.pool.acquire().await.map_err(|error| class_of(&error))
    }

    async fn send(&self, conn: &mut Self::Conn, payloads: Vec<String>) -> Result<(), Failure> {
        sqlx::query(NOTIFY)
            .bind(self.channel.as_str())
            .bind(payloads)
            .execute(&mut **conn)
            .await
            .map(|_| ())
            .map_err(|error| class_of(&error))
    }
}

/// The class of a database error, for the one warning of a dropped batch.
fn class_of(error: &sqlx::Error) -> Failure {
    match error {
        sqlx::Error::PoolTimedOut => "pool_timed_out",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::Io(_) => "io",
        sqlx::Error::Tls(_) => "tls",
        sqlx::Error::Database(_) => "database",
        _ => "other",
    }
}

/// What is waiting to be sent.
#[derive(Default)]
struct Queue {
    /// Changes in the order first queued, each once.
    changes: Vec<(TenantId, ChangeKind)>,
    /// The changes in `changes`.
    queued: HashSet<(TenantId, ChangeKind)>,
    /// Hints in the order first queued, each with the highest `seq` seen.
    hints: Vec<(TenantId, Topic, i64)>,
    /// Where each tenant and topic is in `hints`.
    hint_at: HashMap<(TenantId, Topic), usize>,
    /// Whether a drain task is running.
    running: bool,
}

impl Queue {
    fn push_change(&mut self, tenant: TenantId, kind: &ChangeKind) {
        if self.queued.insert((tenant, kind.clone())) {
            self.changes.push((tenant, kind.clone()));
        }
    }

    fn push_hint(&mut self, tenant: TenantId, topic: &Topic, seq: i64) {
        match self.hint_at.entry((tenant, topic.clone())) {
            Entry::Occupied(at) => {
                let held = &mut self.hints[*at.get()].2;
                *held = (*held).max(seq);
            }
            Entry::Vacant(slot) => {
                slot.insert(self.hints.len());
                self.hints.push((tenant, topic.clone(), seq));
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.hints.is_empty()
    }

    /// Everything queued; the queue is left empty.
    fn take(&mut self) -> Batch {
        self.queued.clear();
        self.hint_at.clear();
        Batch {
            changes: mem::take(&mut self.changes),
            hints: mem::take(&mut self.hints),
        }
    }
}

/// What one round of a drain took from the queue.
struct Batch {
    /// The changes, in order.
    changes: Vec<(TenantId, ChangeKind)>,
    /// The hints, in order.
    hints: Vec<(TenantId, Topic, i64)>,
}

impl Batch {
    fn len(&self) -> usize {
        self.changes.len() + self.hints.len()
    }

    /// The payloads: changes first, then hints.
    fn payloads(self) -> Vec<String> {
        let changes = self
            .changes
            .into_iter()
            .map(|(tenant, kind)| Message::Change { tenant, kind }.encode());
        let hints = self
            .hints
            .into_iter()
            .map(|(tenant, topic, seq)| Message::Hint { tenant, topic, seq }.encode());
        changes.chain(hints).collect()
    }
}

/// The queue and the drain over one outlet.
struct Core<O> {
    /// Where batches go.
    outlet: O,
    /// What waits, and whether a drain runs.
    queue: Mutex<Queue>,
}

impl<O> Core<O> {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<O: Outlet> Core<O> {
    fn new(outlet: O) -> Self {
        Self {
            outlet,
            queue: Mutex::new(Queue::default()),
        }
    }

    fn change(self: &Arc<Self>, tenant: TenantId, kind: &ChangeKind) {
        self.enqueue(|queue| queue.push_change(tenant, kind));
    }

    fn hint(self: &Arc<Self>, tenant: TenantId, topic: &Topic, seq: i64) {
        if seq >= 0 {
            self.enqueue(|queue| queue.push_hint(tenant, topic, seq));
        }
    }

    /// Queues with `push` and starts a drain when none runs and a runtime is there to run it.
    fn enqueue(self: &Arc<Self>, push: impl FnOnce(&mut Queue)) {
        let runtime = tokio::runtime::Handle::try_current().ok();
        let start = {
            let mut queue = self.lock();
            push(&mut queue);
            let start = runtime.is_some() && !queue.running;
            queue.running |= start;
            start
        };
        if let Some(runtime) = runtime.filter(|_| start) {
            // The guard exists before the task does, so a task dropped before its first poll
            // still clears the flag.
            let running = Running {
                core: Arc::clone(self),
                armed: true,
            };
            runtime.spawn(Self::drain(running));
        }
    }

    async fn drain(mut running: Running<O>) {
        let core = Arc::clone(&running.core);
        loop {
            let acquired = core.outlet.acquire().await;
            let batch = core.lock().take();
            let count = batch.len();
            let sent = match acquired {
                Ok(mut conn) => core.outlet.send(&mut conn, batch.payloads()).await,
                Err(failure) => Err(failure),
            };
            if let Err(class) = sent {
                tracing::warn!(
                    class,
                    dropped = count,
                    "sending invalidation notifications failed; the batch is dropped"
                );
            }
            let mut queue = core.lock();
            if queue.is_empty() {
                queue.running = false;
                running.armed = false;
                return;
            }
        }
    }
}

/// Clears the running flag when a drain ends without reaching its normal end (a panic, or its
/// task dropped before or while it ran), so that a later call starts another. Made before the
/// drain's task is spawned and moved into it.
struct Running<O> {
    /// The drain's core.
    core: Arc<Core<O>>,
    /// Whether the flag is still to be cleared here.
    armed: bool,
}

impl<O> Drop for Running<O> {
    fn drop(&mut self) {
        if self.armed {
            self.core.lock().running = false;
        }
    }
}

/// Sends notifications without making the caller wait. Clones share the queue.
#[derive(Clone)]
pub struct Sender {
    /// The queue and the drain on the control pool.
    core: Arc<Core<PoolOutlet>>,
}

// type name only
impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sender")
    }
}

impl Sender {
    /// A sender on `control` for `channel`.
    pub fn new(control: PgPool, channel: Channel) -> Self {
        Self {
            core: Arc::new(Core::new(PoolOutlet {
                pool: control,
                channel,
            })),
        }
    }

    /// Queues a change; kept once per tenant and kind.
    pub fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        self.core.change(tenant, kind);
    }

    /// Queues a hint; the highest `seq` per tenant and topic wins; a negative `seq` is ignored.
    pub fn hint(&self, tenant: TenantId, topic: &Topic, seq: i64) {
        self.core.hint(tenant, topic, seq);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::sync::{mpsc, oneshot};
    use uuid::Uuid;

    use super::*;

    /// What the fake does on one acquire or one send.
    enum Step {
        Pass,
        Fail,
        Gate(oneshot::Receiver<()>),
        Panic,
    }

    /// What the fake saw, in order.
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Acquire,
        Send,
        Batch(Vec<String>),
    }

    /// An outlet that follows a script and reports what it saw.
    struct Fake {
        acquires: Mutex<VecDeque<Step>>,
        sends: Mutex<VecDeque<Step>>,
        seen: mpsc::UnboundedSender<Seen>,
    }

    fn next_step(steps: &Mutex<VecDeque<Step>>) -> Step {
        steps.lock().unwrap().pop_front().unwrap_or(Step::Pass)
    }

    async fn perform(step: Step) -> Result<(), Failure> {
        match step {
            Step::Pass => Ok(()),
            Step::Fail => Err("fake"),
            Step::Gate(gate) => {
                gate.await.expect("the test opens the gate");
                Ok(())
            }
            Step::Panic => panic!("the fake outlet panicked"),
        }
    }

    impl Outlet for Fake {
        type Conn = ();

        async fn acquire(&self) -> Result<(), Failure> {
            self.seen.send(Seen::Acquire).unwrap();
            perform(next_step(&self.acquires)).await
        }

        async fn send(&self, _conn: &mut (), payloads: Vec<String>) -> Result<(), Failure> {
            self.seen.send(Seen::Send).unwrap();
            perform(next_step(&self.sends)).await?;
            self.seen.send(Seen::Batch(payloads)).unwrap();
            Ok(())
        }
    }

    fn core(
        acquires: Vec<Step>,
        sends: Vec<Step>,
    ) -> (Arc<Core<Fake>>, mpsc::UnboundedReceiver<Seen>) {
        let (seen, observed) = mpsc::unbounded_channel();
        let fake = Fake {
            acquires: Mutex::new(acquires.into()),
            sends: Mutex::new(sends.into()),
            seen,
        };
        (Arc::new(Core::new(fake)), observed)
    }

    fn tenant(n: u128) -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(n))
    }

    fn topic(text: &str) -> Topic {
        Topic::new(text).unwrap()
    }

    async fn expect(observed: &mut mpsc::UnboundedReceiver<Seen>, want: Seen) {
        assert_eq!(observed.recv().await, Some(want));
    }

    fn batch(payloads: &[String]) -> Seen {
        Seen::Batch(payloads.to_vec())
    }

    #[tokio::test(start_paused = true)]
    async fn what_is_queued_while_the_connection_is_awaited_goes_out_as_one_batch() {
        let (open, gate) = oneshot::channel();
        let (core, mut seen) = core(vec![Step::Gate(gate)], vec![]);
        let (a, b) = (tenant(1), tenant(2));
        let (orders, stock) = (topic("orders"), topic("stock"));

        core.change(a, &ChangeKind::STATE);
        expect(&mut seen, Seen::Acquire).await;
        core.hint(a, &orders, 3);
        core.change(b, &ChangeKind::HOSTS);
        core.change(a, &ChangeKind::STATE);
        core.hint(a, &orders, 9);
        core.hint(a, &orders, 5);
        core.hint(b, &stock, -1);
        core.hint(b, &orders, 0);
        core.change(a, &ChangeKind::HOSTS);
        open.send(()).unwrap();

        expect(&mut seen, Seen::Send).await;
        let want = [
            format!("t:{a}:state"),
            format!("t:{b}:hosts"),
            format!("t:{a}:hosts"),
            format!("e:{a}:orders:9"),
            format!("e:{b}:orders:0"),
        ];
        expect(&mut seen, batch(&want)).await;

        core.hint(b, &stock, -5);
        core.hint(b, &stock, 4);
        expect(&mut seen, Seen::Acquire).await;
        expect(&mut seen, Seen::Send).await;
        expect(&mut seen, batch(&[format!("e:{b}:stock:4")])).await;
    }

    #[tokio::test(start_paused = true)]
    async fn only_one_batch_is_in_flight_and_what_comes_meanwhile_goes_next() {
        let (open, gate) = oneshot::channel();
        let (core, mut seen) = core(vec![], vec![Step::Gate(gate)]);
        let (a, b) = (tenant(1), tenant(2));

        core.change(a, &ChangeKind::STATE);
        expect(&mut seen, Seen::Acquire).await;
        expect(&mut seen, Seen::Send).await;
        core.change(b, &ChangeKind::STATE);
        core.change(a, &ChangeKind::STATE);
        open.send(()).unwrap();

        expect(&mut seen, batch(&[format!("t:{a}:state")])).await;
        expect(&mut seen, Seen::Acquire).await;
        expect(&mut seen, Seen::Send).await;
        let want = [format!("t:{b}:state"), format!("t:{a}:state")];
        expect(&mut seen, batch(&want)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_acquire_drops_its_round_and_later_messages_are_still_sent() {
        let (core, mut seen) = core(vec![Step::Fail], vec![]);
        let (a, b) = (tenant(1), tenant(2));

        core.change(a, &ChangeKind::STATE);
        expect(&mut seen, Seen::Acquire).await;
        core.change(b, &ChangeKind::STATE);
        expect(&mut seen, Seen::Acquire).await;
        expect(&mut seen, Seen::Send).await;
        expect(&mut seen, batch(&[format!("t:{b}:state")])).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_statement_drops_its_round_and_later_messages_are_still_sent() {
        let (core, mut seen) = core(vec![], vec![Step::Fail]);
        let (a, b) = (tenant(1), tenant(2));
        let orders = topic("orders");

        core.change(a, &ChangeKind::STATE);
        core.hint(a, &orders, 1);
        expect(&mut seen, Seen::Acquire).await;
        expect(&mut seen, Seen::Send).await;
        core.hint(b, &orders, 2);
        expect(&mut seen, Seen::Acquire).await;
        expect(&mut seen, Seen::Send).await;
        expect(&mut seen, batch(&[format!("e:{b}:orders:2")])).await;
    }

    #[test]
    fn a_call_outside_a_runtime_only_queues_and_a_later_call_inside_one_sends_both() {
        let (core, mut seen) = core(vec![], vec![]);
        let a = tenant(1);

        core.change(a, &ChangeKind::STATE);
        assert_eq!(seen.try_recv(), Err(TryRecvError::Empty));
        assert!(!core.lock().running);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            core.hint(a, &topic("orders"), 1);
            expect(&mut seen, Seen::Acquire).await;
            expect(&mut seen, Seen::Send).await;
            let want = [format!("t:{a}:state"), format!("e:{a}:orders:1")];
            expect(&mut seen, batch(&want)).await;
        });
    }

    #[test]
    fn a_drain_dropped_before_it_ran_does_not_wedge_the_sender() {
        let (core, mut seen) = core(vec![], vec![]);
        let (a, b) = (tenant(1), tenant(2));

        // The drain is spawned on a runtime that is never driven, then dropped with it.
        let first = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let _entered = first.enter();
            core.change(a, &ChangeKind::STATE);
            assert!(core.lock().running);
        }
        drop(first);
        assert!(!core.lock().running);
        assert_eq!(seen.try_recv(), Err(TryRecvError::Empty));

        let second = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        second.block_on(async {
            core.change(b, &ChangeKind::STATE);
            expect(&mut seen, Seen::Acquire).await;
            expect(&mut seen, Seen::Send).await;
            let want = [format!("t:{a}:state"), format!("t:{b}:state")];
            expect(&mut seen, batch(&want)).await;
        });
        assert!(!core.lock().running);
    }

    #[tokio::test(start_paused = true)]
    async fn a_drain_that_panics_does_not_wedge_the_sender() {
        let (core, mut seen) = core(vec![Step::Panic], vec![]);
        let (a, b) = (tenant(1), tenant(2));

        core.change(a, &ChangeKind::STATE);
        expect(&mut seen, Seen::Acquire).await;
        core.change(b, &ChangeKind::STATE);
        expect(&mut seen, Seen::Acquire).await;
        expect(&mut seen, Seen::Send).await;
        let want = [format!("t:{a}:state"), format!("t:{b}:state")];
        expect(&mut seen, batch(&want)).await;
    }

    #[tokio::test]
    async fn the_sender_queues_without_awaiting_and_prints_its_type_name() {
        let options = PgConnectOptions::new().host("127.0.0.1").port(1);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy_with(options);
        let sender = Sender::new(pool.clone(), Channel::new("app_changes").unwrap());
        assert_eq!(format!("{sender:?}"), "Sender");
        assert_eq!(format!("{:?}", sender.clone()), "Sender");

        let change: fn(&Sender, TenantId, &ChangeKind) = Sender::change;
        let hint: fn(&Sender, TenantId, &Topic, i64) = Sender::hint;
        change(&sender, tenant(1), &ChangeKind::STATE);
        hint(&sender, tenant(1), &topic("orders"), 1);
        hint(&sender, tenant(1), &topic("orders"), -1);
        pool.close().await;
    }

    #[test]
    fn a_failure_is_logged_by_its_class_only() {
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(class_of(&sqlx::Error::Io(refused)), "io");
        assert_eq!(class_of(&sqlx::Error::Tls("handshake".into())), "tls");
        assert_eq!(class_of(&sqlx::Error::PoolTimedOut), "pool_timed_out");
        assert_eq!(class_of(&sqlx::Error::PoolClosed), "pool_closed");
        assert_eq!(class_of(&sqlx::Error::RowNotFound), "other");
    }
}
