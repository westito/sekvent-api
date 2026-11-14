//! Behaviour of `Keyed` on tokio's paused clock, with factories the test
//! answers by hand and a disposal hook that reports through a channel.

use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use sekvent_error::{AppError, ErrorCode};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, advance};

use super::{Entry, Keyed, KeyedBuilder, KeyedRef, Made};
use crate::reasons::{KEYED_CAPACITY, KEYED_CLOSED, KEYED_CREATE_TIMEOUT, KEYED_FACTORY_PANICKED};

#[derive(Debug, PartialEq, Eq)]
struct Value {
    key: u32,
    serial: u32,
}

/// One factory call, waiting for the test's answer.
struct Call {
    key: u32,
    reply: oneshot::Sender<Result<Value, AppError>>,
}

impl Call {
    fn ok(self, serial: u32) {
        let key = self.key;
        let _ = self.reply.send(Ok(Value { key, serial }));
    }

    fn fail(self, error: AppError) {
        let _ = self.reply.send(Err(error));
    }
}

/// One disposal; `release` is set when the hook was held and waits for it.
struct Disposed {
    key: u32,
    serial: u32,
    release: Option<oneshot::Sender<()>>,
}

struct Harness {
    cache: Keyed<u32, Value>,
    calls: mpsc::UnboundedReceiver<Call>,
    disposed: mpsc::UnboundedReceiver<Disposed>,
    hold: Arc<AtomicBool>,
}

impl Harness {
    fn new(configure: impl FnOnce(KeyedBuilder<u32, Value>) -> KeyedBuilder<u32, Value>) -> Self {
        let (call_tx, calls) = mpsc::unbounded_channel();
        let (dispose_tx, disposed) = mpsc::unbounded_channel();
        let hold = Arc::new(AtomicBool::new(false));
        let gated = Arc::clone(&hold);
        let builder = Keyed::builder(move |key: u32| {
            let (reply, answer) = oneshot::channel();
            let sent = call_tx.send(Call { key, reply });
            async move {
                sent.map_err(|_| AppError::cancelled("the test ended"))?;
                answer
                    .await
                    .map_err(|_| AppError::cancelled("the call was dropped"))?
            }
        })
        .dispose(move |key, value: Value| {
            let (release, gate) = if gated.load(Ordering::SeqCst) {
                let (release, gate) = oneshot::channel();
                (Some(release), Some(gate))
            } else {
                (None, None)
            };
            let _ = dispose_tx.send(Disposed {
                key,
                serial: value.serial,
                release,
            });
            async move {
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
            }
        });
        Self {
            cache: configure(builder).build().unwrap(),
            calls,
            disposed,
            hold,
        }
    }

    fn spawn_get(&self, key: u32) -> JoinHandle<Result<KeyedRef<Value>, AppError>> {
        let cache = self.cache.clone();
        tokio::spawn(async move { cache.get(&key).await })
    }

    async fn call(&mut self) -> Call {
        self.calls.recv().await.expect("a factory call")
    }

    /// A reference to `key`'s value, created with `serial`.
    async fn warm(&mut self, key: u32, serial: u32) -> KeyedRef<Value> {
        let get = self.spawn_get(key);
        let call = self.call().await;
        assert_eq!(call.key, key);
        call.ok(serial);
        get.await.unwrap().unwrap()
    }

    async fn next_disposed(&mut self) -> (u32, u32) {
        let disposed = self.disposed.recv().await.expect("a disposal");
        (disposed.key, disposed.serial)
    }

    fn no_disposal(&mut self) -> bool {
        self.disposed.try_recv().is_err()
    }

    fn no_call(&mut self) -> bool {
        self.calls.try_recv().is_err()
    }

    async fn idle(&self) {
        self.cache.inner.tracker.wait_idle().await;
    }
}

fn plain() -> KeyedBuilder<u32, Value> {
    Keyed::builder(|key: u32| async move { Ok(Value { key, serial: 0 }) })
}

fn assert_reason(error: &AppError, code: ErrorCode, reason: &str) {
    assert_eq!(error.code(), code);
    assert_eq!(error.reason(), Some(reason));
}

#[tokio::test(start_paused = true)]
async fn concurrent_gets_of_one_key_share_one_creation() {
    let mut h = Harness::new(|b| b);
    let waiters: Vec<_> = (0..5).map(|_| h.spawn_get(1)).collect();
    let other = h.spawn_get(2);

    let first = h.call().await;
    let second = h.call().await;
    assert_eq!((first.key, second.key), (1, 2));
    let stats = h.cache.stats();
    assert_eq!((stats.creating, stats.warm, stats.creations), (2, 0, 2));
    second.ok(20);
    first.ok(10);

    let mut values = Vec::new();
    for waiter in waiters {
        values.push(waiter.await.unwrap().unwrap());
    }
    for value in &values {
        assert!(std::ptr::eq(&raw const **value, &raw const *values[0]));
        assert_eq!(**value, Value { key: 1, serial: 10 });
    }
    assert_eq!(other.await.unwrap().unwrap().serial, 20);
    assert!(h.no_call());

    let stats = h.cache.stats();
    assert_eq!((stats.warm, stats.in_use, stats.creating), (2, 1, 0));
    assert_eq!((stats.creations, stats.create_failures), (2, 0));
}

#[tokio::test(start_paused = true)]
async fn a_failure_reaches_every_waiter_without_its_source_and_is_not_cached() {
    let mut h = Harness::new(|b| b);
    let waiters = [h.spawn_get(1), h.spawn_get(1)];
    h.call().await.fail(
        AppError::failed_precondition("tenant suspended")
            .with_reason("TENANT_SUSPENDED")
            .with_metadata("tenant", "t1")
            .with_source(std::io::Error::other("hidden cause")),
    );
    for waiter in waiters {
        let error = waiter.await.unwrap().unwrap_err();
        assert_reason(&error, ErrorCode::FailedPrecondition, "TENANT_SUSPENDED");
        assert_eq!(error.message(), "tenant suspended");
        assert_eq!(
            error.metadata().get("tenant").map(String::as_str),
            Some("t1")
        );
        assert!(std::error::Error::source(&error).is_none());
    }
    let stats = h.cache.stats();
    assert_eq!((stats.warm, stats.creating), (0, 0));
    assert_eq!((stats.creations, stats.create_failures), (1, 1));
    assert!(h.cache.peek(&1).is_none());

    assert_eq!(h.warm(1, 7).await.serial, 7);
    assert_eq!(h.cache.stats().creations, 2);
}

#[tokio::test(start_paused = true)]
async fn a_creation_past_its_deadline_is_dropped_and_retried() {
    let mut h = Harness::new(|b| b.create_timeout(Duration::from_secs(5)));
    let start = Instant::now();
    let waiters = [h.spawn_get(1), h.spawn_get(1)];
    let mut call = h.call().await;
    call.reply.closed().await;
    let waited = start.elapsed();
    assert!(waited >= Duration::from_secs(5) && waited < Duration::from_secs(6));
    for waiter in waiters {
        let error = waiter.await.unwrap().unwrap_err();
        assert_reason(&error, ErrorCode::Unavailable, KEYED_CREATE_TIMEOUT);
        assert_eq!(error.message(), "creating the resource took too long");
    }
    assert_eq!(h.cache.stats().create_failures, 1);
    assert_eq!(h.warm(1, 2).await.serial, 2);
}

struct DropFlag(Arc<AtomicUsize>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn a_panicking_factory_fails_its_waiters_without_the_payload_and_is_retried() {
    let attempts = Arc::new(AtomicU32::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let flags = Arc::clone(&dropped);
    let cache = Keyed::builder(move |key: u32| {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        assert!(attempt != 0, "secret payload before the future");
        let flag = DropFlag(Arc::clone(&flags));
        async move {
            let _flag = flag;
            tokio::task::yield_now().await;
            assert!(attempt != 1, "secret payload in the body");
            Ok(Value {
                key,
                serial: attempt,
            })
        }
    })
    .build()
    .unwrap();

    for _ in 0..2 {
        let error = cache.get(&1).await.unwrap_err();
        assert_reason(&error, ErrorCode::Internal, KEYED_FACTORY_PANICKED);
        assert_eq!(error.message(), "internal error");
        assert!(!format!("{error:?}").contains("secret"));
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(cache.get(&1).await.unwrap().serial, 2);
    let stats = cache.stats();
    assert_eq!((stats.creations, stats.create_failures), (3, 2));
}

#[tokio::test(start_paused = true)]
async fn a_dropped_get_does_not_cancel_its_creation() {
    let mut h = Harness::new(|b| b);
    let first = h.spawn_get(1);
    let call = h.call().await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(!call.reply.is_closed());
    call.ok(3);

    assert_eq!(h.cache.get(&1).await.unwrap().serial, 3);
    assert!(h.no_call());
    assert_eq!(h.cache.stats().creations, 1);
}

#[tokio::test(start_paused = true)]
#[allow(clippy::too_many_lines)]
async fn capacity_evicts_the_least_recently_used_entry_nothing_refers_to() {
    let mut h = Harness::new(|b| b.capacity(2));
    let one = h.warm(1, 1).await;
    advance(Duration::from_secs(1)).await;
    let two = h.warm(2, 2).await;
    advance(Duration::from_secs(1)).await;
    drop(one);
    advance(Duration::from_secs(1)).await;
    drop(two);

    let three = h.spawn_get(3);
    let call = h.call().await;
    assert_eq!(h.next_disposed().await, (1, 1));
    call.ok(3);
    let three = three.await.unwrap().unwrap();
    let stats = h.cache.stats();
    assert_eq!((stats.warm, stats.evictions), (2, 1));

    let two = h.cache.get(&2).await.unwrap();
    let refused = h.cache.get(&4).await.unwrap_err();
    assert_reason(&refused, ErrorCode::ResourceExhausted, KEYED_CAPACITY);
    assert_eq!(refused.message(), "the resource cache is full");
    assert_eq!(h.cache.stats().capacity_refusals, 1);

    let again = h.cache.get(&2).await.unwrap();
    assert!(std::ptr::eq(&raw const *again, &raw const *two));
    drop((two, again));

    let four = h.spawn_get(4);
    let call = h.call().await;
    assert_eq!(h.next_disposed().await, (2, 2));
    call.ok(4);
    assert_eq!(four.await.unwrap().unwrap().serial, 4);
    assert_eq!(three.serial, 3);
    let stats = h.cache.stats();
    assert_eq!((stats.evictions, stats.capacity_refusals), (2, 1));
}

#[tokio::test(start_paused = true)]
async fn an_entry_being_created_counts_against_the_capacity() {
    let mut h = Harness::new(|b| b.capacity(1));
    let first = h.spawn_get(1);
    let call = h.call().await;
    let refused = h.cache.get(&2).await.unwrap_err();
    assert_reason(&refused, ErrorCode::ResourceExhausted, KEYED_CAPACITY);
    call.ok(1);
    drop(first.await.unwrap().unwrap());
    assert_eq!(h.warm(2, 2).await.serial, 2);
    assert_eq!(h.next_disposed().await, (1, 1));
}

#[tokio::test(start_paused = true)]
async fn idle_entries_are_evicted_only_after_the_idle_time_and_never_in_use() {
    let idle = Duration::from_secs(10);
    let mut h = Harness::new(|b| b.evict_idle_after(Some(idle)));
    let value = h.warm(1, 1).await;
    advance(Duration::from_secs(60)).await;
    assert_eq!(h.cache.evict_idle().await, 0);

    let clone = value.clone();
    drop(value);
    advance(Duration::from_secs(60)).await;
    assert_eq!(h.cache.evict_idle().await, 0);
    assert_eq!(h.cache.stats().in_use, 1);
    drop(clone);

    advance(idle.saturating_sub(Duration::from_millis(1))).await;
    assert_eq!(h.cache.evict_idle().await, 0);
    advance(Duration::from_millis(1)).await;
    assert_eq!(h.cache.evict_idle().await, 1);
    assert_eq!(h.next_disposed().await, (1, 1));
    let stats = h.cache.stats();
    assert_eq!((stats.warm, stats.evictions), (0, 1));
}

#[tokio::test(start_paused = true)]
async fn without_an_idle_time_only_an_explicit_age_evicts() {
    let mut h = Harness::new(|b| b);
    drop(h.warm(1, 1).await);
    let held = h.warm(2, 2).await;
    advance(Duration::from_secs(3600)).await;
    assert_eq!(h.cache.evict_idle().await, 0);
    assert_eq!(
        h.cache
            .evict_idle_older_than(Duration::from_secs(7200))
            .await,
        0
    );

    assert_eq!(h.cache.evict_idle_older_than(Duration::ZERO).await, 1);
    assert_eq!(h.next_disposed().await, (1, 1));
    assert!(h.no_disposal());
    assert_eq!(h.cache.peek(&2).unwrap().serial, 2);
    drop(held);
    assert_eq!(h.cache.evict_idle_older_than(Duration::ZERO).await, 1);
    assert_eq!(h.next_disposed().await, (2, 2));
}

#[tokio::test(start_paused = true)]
async fn peek_never_creates_and_never_counts_as_a_use() {
    let idle = Duration::from_secs(10);
    let mut h = Harness::new(|b| b.evict_idle_after(Some(idle)));
    assert!(h.cache.peek(&1).is_none());

    let get = h.spawn_get(1);
    let call = h.call().await;
    assert!(h.cache.peek(&1).is_none());
    assert!(h.no_call());
    call.ok(1);
    let value = get.await.unwrap().unwrap();
    let peeked = h.cache.peek(&1).unwrap();
    assert!(std::ptr::eq(&raw const *peeked, &raw const *value));
    drop((peeked, value));

    advance(idle.saturating_sub(Duration::from_millis(1))).await;
    let peeked = h.cache.peek(&1).unwrap();
    drop(peeked.clone());
    drop(peeked);
    advance(Duration::from_millis(1)).await;
    assert_eq!(h.cache.evict_idle().await, 1);

    drop(h.warm(2, 2).await);
    let held = h.cache.peek(&2).unwrap();
    assert_eq!(h.cache.stats().in_use, 1);
    advance(Duration::from_secs(3600)).await;
    assert_eq!(h.cache.evict_idle().await, 0);
    assert_eq!(h.cache.evict_idle_older_than(Duration::ZERO).await, 0);
    drop(held);
    assert_eq!(h.cache.evict_idle().await, 1);

    drop(h.warm(3, 3).await);
    h.cache.close().await;
    assert!(h.cache.peek(&3).is_none());
}

#[tokio::test(start_paused = true)]
async fn invalidate_retires_a_referenced_value_and_disposes_a_free_one_at_once() {
    let mut h = Harness::new(|b| b);
    let old = h.warm(1, 1).await;
    assert!(h.cache.invalidate(&1).await);
    assert_eq!(old.serial, 1);
    let stats = h.cache.stats();
    assert_eq!((stats.warm, stats.retiring), (0, 1));

    let fresh = h.warm(1, 2).await;
    assert_eq!(fresh.serial, 2);
    assert!(h.no_disposal());
    drop(old);
    assert_eq!(h.next_disposed().await, (1, 1));
    h.idle().await;
    assert_eq!(h.cache.stats().retiring, 0);

    drop(fresh);
    assert!(h.cache.invalidate(&1).await);
    let disposed = h.disposed.try_recv().unwrap();
    assert_eq!((disposed.key, disposed.serial), (1, 2));
    assert!(h.no_disposal());

    assert!(!h.cache.invalidate(&9).await);
    let stats = h.cache.stats();
    assert_eq!((stats.evictions, stats.creations), (0, 2));
}

#[tokio::test(start_paused = true)]
async fn a_stale_value_reaches_nobody_and_its_waiters_get_the_current_one() {
    let mut h = Harness::new(|b| b);
    let first = h.spawn_get(1);
    let stale = h.call().await;
    assert!(h.cache.invalidate(&1).await);
    let second = h.spawn_get(1);
    let current = h.call().await;

    stale.ok(1);
    assert_eq!(h.next_disposed().await, (1, 1));
    current.ok(2);
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();
    assert!(std::ptr::eq(&raw const *first, &raw const *second));
    assert_eq!(first.serial, 2);
    assert!(h.no_call());
    assert_eq!(h.cache.stats().creations, 2);
}

#[tokio::test(start_paused = true)]
async fn a_stale_failure_reaches_nobody() {
    let mut h = Harness::new(|b| b);
    let first = h.spawn_get(1);
    let stale = h.call().await;
    assert!(h.cache.invalidate(&1).await);
    let second = h.spawn_get(1);
    let current = h.call().await;

    stale.fail(AppError::unavailable("stale failure"));
    current.ok(2);
    assert_eq!(first.await.unwrap().unwrap().serial, 2);
    assert_eq!(second.await.unwrap().unwrap().serial, 2);
    let stats = h.cache.stats();
    assert_eq!((stats.creations, stats.create_failures), (2, 1));
}

#[tokio::test(start_paused = true)]
async fn a_stale_creation_ending_late_leaves_the_warm_value_alone() {
    let mut h = Harness::new(|b| b);
    let first = h.spawn_get(1);
    let stale = h.call().await;
    assert!(h.cache.invalidate(&1).await);
    let second = h.spawn_get(1);
    h.call().await.ok(2);
    let warm = second.await.unwrap().unwrap();
    assert!(std::ptr::eq(
        &raw const *h.cache.peek(&1).unwrap(),
        &raw const *warm
    ));

    stale.ok(1);
    assert_eq!(first.await.unwrap().unwrap().serial, 2);
    assert_eq!(h.next_disposed().await, (1, 1));
    h.idle().await;
    assert!(h.no_disposal());
    assert!(std::ptr::eq(
        &raw const *h.cache.peek(&1).unwrap(),
        &raw const *warm
    ));
    assert_eq!(h.cache.stats().warm, 1);
}

#[tokio::test(start_paused = true)]
async fn close_cancels_creations_disposes_free_values_and_refuses_gets() {
    let mut h = Harness::new(|b| b);
    let held = h.warm(1, 1).await;
    drop(h.warm(2, 2).await);
    let pending = h.spawn_get(3);
    let call = h.call().await;

    h.cache.close().await;
    assert!(call.reply.is_closed());
    let disposed = h.disposed.try_recv().unwrap();
    assert_eq!((disposed.key, disposed.serial), (2, 2));
    assert!(h.no_disposal());
    let error = pending.await.unwrap().unwrap_err();
    assert_reason(&error, ErrorCode::Unavailable, KEYED_CLOSED);
    assert_eq!(error.message(), "the resource cache is closed");
    let error = h.cache.get(&1).await.unwrap_err();
    assert_reason(&error, ErrorCode::Unavailable, KEYED_CLOSED);
    assert!(h.cache.is_closed());
    assert!(!h.cache.invalidate(&1).await);

    let stats = h.cache.stats();
    assert_eq!((stats.warm, stats.creating, stats.retiring), (0, 0, 1));
    assert_eq!(stats.create_failures, 1);
    assert_eq!(held.serial, 1);
    drop(held);
    assert_eq!(h.next_disposed().await, (1, 1));
    h.cache.close().await;
    assert_eq!(h.cache.stats().retiring, 0);
}

#[tokio::test(start_paused = true)]
async fn close_waits_for_a_background_disposal_and_a_bounded_close_leaves_it_running() {
    let mut h = Harness::new(|b| b.capacity(1));
    h.hold.store(true, Ordering::SeqCst);
    drop(h.warm(1, 1).await);
    let two = h.spawn_get(2);
    let call = h.call().await;
    let disposal = h.disposed.recv().await.unwrap();
    assert_eq!((disposal.key, disposal.serial), (1, 1));
    h.hold.store(false, Ordering::SeqCst);
    call.ok(2);
    drop(two.await.unwrap().unwrap());

    let bounded = tokio::time::timeout(Duration::from_secs(60), h.cache.close()).await;
    assert!(bounded.is_err());
    assert!(h.cache.is_closed());
    assert_eq!(h.next_disposed().await, (2, 2));
    assert_eq!(h.cache.stats().retiring, 1);

    disposal.release.unwrap().send(()).unwrap();
    h.cache.close().await;
    assert_eq!(h.cache.stats().retiring, 0);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_future_of_invalidate_or_evict_does_not_stop_the_disposal() {
    let mut h = Harness::new(|b| b);
    h.hold.store(true, Ordering::SeqCst);
    drop(h.warm(1, 1).await);
    drop(h.warm(2, 2).await);

    let gave_up = tokio::time::timeout(Duration::from_secs(1), h.cache.invalidate(&1)).await;
    assert!(gave_up.is_err());
    let first = h.disposed.recv().await.unwrap();
    let gave_up = tokio::time::timeout(
        Duration::from_secs(1),
        h.cache.evict_idle_older_than(Duration::ZERO),
    )
    .await;
    assert!(gave_up.is_err());
    let second = h.disposed.recv().await.unwrap();
    assert_eq!((first.serial, second.serial), (1, 2));
    assert_eq!(h.cache.stats().retiring, 2);

    first.release.unwrap().send(()).unwrap();
    second.release.unwrap().send(()).unwrap();
    h.idle().await;
    assert_eq!(h.cache.stats().retiring, 0);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_disposal_is_contained() {
    let cache = plain()
        .dispose(|_key, _value| async {
            panic!("disposal panicked");
        })
        .build()
        .unwrap();
    drop(cache.get(&1).await.unwrap());
    assert_eq!(cache.evict_idle_older_than(Duration::ZERO).await, 1);
    assert_eq!(cache.stats().retiring, 0);

    let early = plain()
        .dispose(|_key, _value| -> std::future::Ready<()> { panic!("before the future") })
        .build()
        .unwrap();
    drop(early.get(&1).await.unwrap());
    assert!(early.invalidate(&1).await);
    assert_eq!(early.get(&1).await.unwrap().serial, 0);
    early.close().await;
    assert_eq!(early.stats().retiring, 0);
}

#[tokio::test(start_paused = true)]
async fn a_finisher_runs_after_the_value_was_disposed() {
    let (tx, mut order) = mpsc::unbounded_channel();
    let finished = tx.clone();
    let cache: Keyed<u32, Value> = KeyedBuilder::from_parts(move |key: u32, _tracker| {
        let finished = finished.clone();
        Box::pin(async move {
            Ok(super::Made {
                value: Value { key, serial: 0 },
                finisher: Some(Box::pin(async move {
                    let _ = finished.send("finisher");
                })),
            })
        })
    })
    .dispose(move |_key, _value| {
        let _ = tx.send("dispose");
        async {}
    })
    .build()
    .unwrap();
    drop(cache.get(&1).await.unwrap());
    assert!(cache.invalidate(&1).await);
    assert_eq!(order.try_recv().unwrap(), "dispose");
    assert_eq!(order.try_recv().unwrap(), "finisher");

    let panicking: Keyed<u32, Value> = KeyedBuilder::from_parts(|key: u32, _tracker| {
        Box::pin(async move {
            Ok(super::Made {
                value: Value { key, serial: 0 },
                finisher: Some(Box::pin(async {
                    panic!("finisher panicked");
                })),
            })
        })
    })
    .build()
    .unwrap();
    drop(panicking.get(&1).await.unwrap());
    panicking.close().await;
    assert_eq!(panicking.stats().retiring, 0);
}

#[test]
fn a_reference_dropped_outside_a_runtime_is_just_dropped() {
    let (tx, mut disposed) = mpsc::unbounded_channel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (cache, value) = runtime.block_on(async move {
        let cache = plain()
            .dispose(move |key, _value| {
                let _ = tx.send(key);
                async {}
            })
            .build()
            .unwrap();
        let value = cache.get(&1).await.unwrap();
        assert!(cache.invalidate(&1).await);
        (cache, value)
    });
    drop(runtime);
    assert_eq!(cache.stats().retiring, 1);
    drop(value);
    assert!(disposed.try_recv().is_err());
    assert_eq!(cache.stats().retiring, 0);
}

#[test]
fn a_creation_dropped_with_its_runtime_leaves_no_entry() {
    let (called_tx, mut called) = mpsc::unbounded_channel();
    let cache = Keyed::builder(move |_key: u32| {
        let _ = called_tx.send(());
        std::future::pending::<Result<Value, AppError>>()
    })
    .build()
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let waiting = cache.clone();
        drop(tokio::spawn(async move { waiting.get(&1).await }));
        called.recv().await.unwrap();
        assert_eq!(cache.stats().creating, 1);
    });
    drop(runtime);
    let stats = cache.stats();
    assert_eq!((stats.creating, stats.create_failures), (0, 1));
}

#[test]
fn build_refuses_zero_settings_and_names_them() {
    let refused = [
        (plain().capacity(0), "capacity"),
        (
            plain().evict_idle_after(Some(Duration::ZERO)),
            "evict_idle_after",
        ),
        (plain().create_timeout(Duration::ZERO), "create_timeout"),
        (plain().reap_interval(Duration::ZERO), "reap_interval"),
    ];
    for (builder, setting) in refused {
        let error = builder.build().unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(error.message().contains(setting), "{}", error.message());
    }
    let cache = plain().evict_idle_after(None).build().unwrap();
    assert_eq!(cache.stats().capacity, 64);
    assert!(!cache.is_closed());
}

#[tokio::test(start_paused = true)]
async fn debug_output_shows_settings_and_stats_but_no_key_or_value() {
    let builder = Keyed::builder(|key: String| async move { Ok(format!("value-of-{key}")) })
        .capacity(3)
        .dispose(|_key, _value| async {});
    let shown = format!("{builder:?}");
    assert!(shown.contains("capacity: 3"), "{shown}");
    assert!(shown.contains("create_timeout: 30s"), "{shown}");
    let cache = builder.build().unwrap();
    let key = "tenant-secret".to_owned();
    let value = cache.get(&key).await.unwrap();
    assert_eq!(*value, "value-of-tenant-secret");
    for shown in [
        format!("{cache:?}"),
        format!("{value:?}"),
        format!("{:?}", cache.peek(&key)),
    ] {
        assert!(!shown.contains("secret"), "{shown}");
    }
    assert!(format!("{cache:?}").contains("warm: 1"));
    assert_eq!(format!("{value:?}"), "KeyedRef { .. }");
}

#[tokio::test(start_paused = true)]
async fn close_waits_for_a_value_that_left_the_map_before_its_disposal_started() {
    let mut h = Harness::new(|b| b);
    drop(h.warm(1, 1).await);
    let leaving = {
        let mut state = h.cache.inner.lock();
        let Some(Entry::Warm(slot)) = state.map.remove(&1) else {
            panic!("a warm entry");
        };
        h.cache.inner.leave(slot)
    };
    let closing = {
        let cache = h.cache.clone();
        tokio::spawn(async move { cache.close().await })
    };
    tokio::task::yield_now().await;
    assert!(!closing.is_finished());
    assert_eq!(h.cache.stats().retiring, 1);

    drop(leaving.release());
    closing.await.unwrap();
    let disposed = h.disposed.try_recv().unwrap();
    assert_eq!((disposed.key, disposed.serial), (1, 1));
    assert_eq!(h.cache.stats().retiring, 0);
}

/// Pending until `ready` holds a value; panics when dropped while armed.
struct PanicsOnDrop<T> {
    ready: Option<T>,
    armed: bool,
}

impl<T: Unpin> Future for PanicsOnDrop<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        match self.ready.take() {
            Some(output) => Poll::Ready(output),
            None => Poll::Pending,
        }
    }
}

impl<T> Drop for PanicsOnDrop<T> {
    fn drop(&mut self) {
        assert!(!self.armed, "dropping the future panicked");
    }
}

#[tokio::test(start_paused = true)]
async fn a_factory_future_that_panics_when_dropped_still_times_out_once() {
    let attempts = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&attempts);
    let cache = Keyed::builder(move |key: u32| {
        let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
        PanicsOnDrop {
            ready: (!first).then_some(Ok(Value { key, serial: 1 })),
            armed: first,
        }
    })
    .create_timeout(Duration::from_secs(5))
    .build()
    .unwrap();

    let error = cache.get(&1).await.unwrap_err();
    assert_reason(&error, ErrorCode::Unavailable, KEYED_CREATE_TIMEOUT);
    let stats = cache.stats();
    assert_eq!((stats.creations, stats.create_failures), (1, 1));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(cache.get(&1).await.unwrap().serial, 1);
}

#[tokio::test(start_paused = true)]
async fn a_disposal_future_that_panics_when_dropped_still_runs_the_finisher() {
    let (tx, mut finished) = mpsc::unbounded_channel();
    let cache: Keyed<u32, Value> = KeyedBuilder::from_parts(move |key: u32, _tracker| {
        let tx = tx.clone();
        Box::pin(async move {
            Ok(Made {
                value: Value { key, serial: 0 },
                finisher: Some(Box::pin(async move {
                    let _ = tx.send(key);
                })),
            })
        })
    })
    .dispose(|_key, _value| PanicsOnDrop {
        ready: Some(()),
        armed: true,
    })
    .build()
    .unwrap();
    drop(cache.get(&1).await.unwrap());
    assert!(cache.invalidate(&1).await);
    assert_eq!(finished.try_recv(), Ok(1));
    cache.close().await;
    assert_eq!(cache.stats().retiring, 0);
}

#[tokio::test(start_paused = true)]
async fn the_last_use_never_moves_backwards() {
    let mut h = Harness::new(|b| b);
    let value = h.warm(1, 1).await;
    advance(Duration::from_secs(10)).await;
    let late = super::offset(h.cache.inner.epoch);
    value.slot.touch_at(late);
    value.slot.touch_at(late.saturating_sub(5_000_000_000));
    assert_eq!(value.slot.last_use(), late);
}

/// A key whose `Clone` panics on the `trap`-th clone from now (0: never).
#[derive(Debug)]
struct Touchy {
    id: u32,
    trap: Arc<AtomicU32>,
}

impl Clone for Touchy {
    fn clone(&self) -> Self {
        let left = self.trap.load(Ordering::SeqCst);
        if left > 0 {
            self.trap.store(left - 1, Ordering::SeqCst);
            assert!(left != 1, "cloning the key panicked");
        }
        Self {
            id: self.id,
            trap: Arc::clone(&self.trap),
        }
    }
}

impl PartialEq for Touchy {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Touchy {}

impl Hash for Touchy {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

type Answer = oneshot::Sender<Result<u32, AppError>>;

/// A one-entry cache over `Touchy` keys whose factory the test answers.
fn touchy() -> (
    Keyed<Touchy, u32>,
    mpsc::UnboundedReceiver<Answer>,
    mpsc::UnboundedReceiver<u32>,
) {
    let (call_tx, calls) = mpsc::unbounded_channel();
    let (dispose_tx, disposed) = mpsc::unbounded_channel();
    let cache = Keyed::builder(move |_key: Touchy| {
        let (reply, answer) = oneshot::channel();
        let _ = call_tx.send(reply);
        async move {
            answer
                .await
                .map_err(|_| AppError::cancelled("the call was dropped"))?
        }
    })
    .capacity(1)
    .dispose(move |_key, value| {
        let _ = dispose_tx.send(value);
        async {}
    })
    .build()
    .unwrap();
    (cache, calls, disposed)
}

#[tokio::test(start_paused = true)]
async fn a_key_panicking_while_its_creation_starts_leaves_no_entry() {
    let (cache, mut calls, _disposed) = touchy();
    let trap = Arc::new(AtomicU32::new(0));
    let key = Touchy {
        id: 1,
        trap: Arc::clone(&trap),
    };
    for armed in [1, 2] {
        let starting = {
            let cache = cache.clone();
            let key = key.clone();
            trap.store(armed, Ordering::SeqCst);
            tokio::spawn(async move { cache.get(&key).await.map(|value| *value) })
        };
        assert!(starting.await.unwrap_err().is_panic());
        let stats = cache.stats();
        assert_eq!((stats.creating, stats.warm, stats.creations), (0, 0, 0));
    }

    let get = {
        let cache = cache.clone();
        let key = key.clone();
        tokio::spawn(async move { cache.get(&key).await.map(|value| *value) })
    };
    calls.recv().await.unwrap().send(Ok(7)).unwrap();
    assert_eq!(get.await.unwrap().unwrap(), 7);
}

#[tokio::test(start_paused = true)]
async fn a_key_panicking_while_its_creation_settles_fails_the_waiters_and_frees_the_key() {
    let (cache, mut calls, mut disposed) = touchy();
    let trap = Arc::new(AtomicU32::new(0));
    let key = Touchy {
        id: 1,
        trap: Arc::clone(&trap),
    };
    let spawn_get = |cache: &Keyed<Touchy, u32>| {
        let cache = cache.clone();
        let key = key.clone();
        tokio::spawn(async move { cache.get(&key).await.map(|value| *value) })
    };
    let waiters = [spawn_get(&cache), spawn_get(&cache)];
    let reply = calls.recv().await.unwrap();
    trap.store(1, Ordering::SeqCst);
    reply.send(Ok(5)).unwrap();
    for waiter in waiters {
        let error = waiter.await.unwrap().unwrap_err();
        assert_reason(&error, ErrorCode::Internal, KEYED_FACTORY_PANICKED);
    }
    cache.inner.tracker.wait_idle().await;
    let stats = cache.stats();
    assert_eq!((stats.creating, stats.warm, stats.retiring), (0, 0, 0));
    assert_eq!((stats.creations, stats.create_failures), (1, 1));
    assert!(disposed.try_recv().is_err());

    let get = spawn_get(&cache);
    calls.recv().await.unwrap().send(Ok(6)).unwrap();
    assert_eq!(get.await.unwrap().unwrap(), 6);
}

#[tokio::test(start_paused = true)]
async fn an_entry_whose_creation_vanished_is_replaced_and_never_spun_on() {
    let mut h = Harness::new(|b| b.capacity(1));
    let (sender, outcome) = tokio::sync::watch::channel(super::Outcome::Pending);
    drop(h.cache.inner.lock().map.insert(
        1,
        Entry::Creating {
            generation: u64::MAX,
            outcome: outcome.clone(),
        },
    ));
    let stuck = tokio::spawn(super::settled(outcome));
    drop(sender);
    let super::Outcome::Failed(wire) = stuck.await.unwrap() else {
        panic!("a terminal failure");
    };
    let error = AppError::from_wire(wire);
    assert_reason(&error, ErrorCode::Internal, KEYED_FACTORY_PANICKED);

    assert_eq!(h.warm(1, 3).await.serial, 3);
    assert_eq!(h.cache.stats().creating, 0);
}

#[tokio::test(start_paused = true)]
async fn a_last_reference_dropped_on_a_plain_thread_is_disposed_on_the_runtime() {
    let mut h = Harness::new(|b| b);
    let value = h.warm(1, 1).await;
    assert!(h.cache.invalidate(&1).await);
    assert_eq!(h.cache.stats().retiring, 1);
    std::thread::spawn(move || drop(value)).join().unwrap();

    let disposed = tokio::time::timeout(Duration::from_secs(60), h.next_disposed()).await;
    assert_eq!(disposed, Ok((1, 1)));
    h.idle().await;
    assert_eq!(h.cache.stats().retiring, 0);
}
