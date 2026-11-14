//! Task bookkeeping, panic containment and the failure log of the keyed
//! cache.

use std::error::Error as StdError;
use std::fmt::{self, Write as _};
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use sekvent_error::AppError;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// A boxed, sendable future.
pub(crate) type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Counts the cache's background tasks and its retiring values.
#[derive(Default)]
pub(crate) struct Tracker {
    /// Creation and disposal tasks still running.
    tasks: AtomicUsize,
    /// Creation tasks among them.
    creating: AtomicUsize,
    /// Values out of the cache that are referenced or being disposed.
    retiring: AtomicUsize,
    /// Notified when `tasks` drops to zero.
    idle: Notify,
}

impl Tracker {
    /// Counts one task in until the token drops. Taken before the task is
    /// spawned, so a `close` that runs in between still waits for it.
    pub(crate) fn task(self: &Arc<Self>, creation: bool) -> TaskToken {
        self.tasks.fetch_add(1, Ordering::AcqRel);
        if creation {
            self.creating.fetch_add(1, Ordering::AcqRel);
        }
        TaskToken {
            tracker: Arc::clone(self),
            creation,
        }
    }

    /// Counts one retiring value in until the token drops.
    pub(crate) fn retire(self: &Arc<Self>) -> RetireToken {
        self.retiring.fetch_add(1, Ordering::AcqRel);
        RetireToken {
            tracker: Arc::clone(self),
        }
    }

    pub(crate) fn retiring(&self) -> usize {
        self.retiring.load(Ordering::Acquire)
    }

    /// Creation tasks still running; reported by the unit when its close
    /// runs out of time.
    #[cfg_attr(not(feature = "runtime"), allow(dead_code))]
    pub(crate) fn creating(&self) -> usize {
        self.creating.load(Ordering::Acquire)
    }

    /// Runs `work` on a tracked task of its own; `retire` keeps the value
    /// counted as retiring until the task ends. Outside a tokio runtime
    /// `work` is dropped instead.
    pub(crate) fn spawn_retiring(
        self: &Arc<Self>,
        retire: RetireToken,
        work: impl Future<Output = ()> + Send + 'static,
    ) -> Option<JoinHandle<()>> {
        self.spawn_retiring_on(None, retire, work)
    }

    /// [`spawn_retiring`](Self::spawn_retiring) on the current runtime, or on
    /// `fallback` when the caller runs outside one (a plain thread). A
    /// runtime that shut down drops `work` at once; with neither, `work` is
    /// dropped here.
    pub(crate) fn spawn_retiring_on(
        self: &Arc<Self>,
        fallback: Option<&Handle>,
        retire: RetireToken,
        work: impl Future<Output = ()> + Send + 'static,
    ) -> Option<JoinHandle<()>> {
        let current = Handle::try_current().ok();
        let Some(runtime) = current.as_ref().or(fallback) else {
            drop(work);
            drop(retire);
            return None;
        };
        let task = self.task(false);
        Some(runtime.spawn(async move {
            let _task = task;
            let _retire = retire;
            work.await;
        }))
    }

    /// Returns once no tracked task is running.
    pub(crate) async fn wait_idle(&self) {
        loop {
            let mut notified = pin!(self.idle.notified());
            notified.as_mut().enable();
            if self.tasks.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// One running task; counts it out on every exit, a panic included.
pub(crate) struct TaskToken {
    tracker: Arc<Tracker>,
    creation: bool,
}

impl Drop for TaskToken {
    fn drop(&mut self) {
        if self.creation {
            self.tracker.creating.fetch_sub(1, Ordering::AcqRel);
        }
        if self.tracker.tasks.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.tracker.idle.notify_waiters();
        }
    }
}

/// One retiring value; lowers the gauge when its disposal ended.
pub(crate) struct RetireToken {
    tracker: Arc<Tracker>,
}

impl Drop for RetireToken {
    fn drop(&mut self) {
        self.tracker.retiring.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The future panicked; its payload is dropped unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Panicked;

/// Calls `make` and runs the future it returns, turning a panic of either
/// into [`Panicked`]. The future is dropped under the same guard, when it
/// ended and when this future is dropped before that (a deadline, a
/// cancellation), so a panicking `Drop` never unwinds into the caller.
pub(crate) async fn guarded<F, Fut>(make: F) -> Result<Fut::Output, Panicked>
where
    F: FnOnce() -> Fut,
    Fut: Future + Unpin,
{
    let future = catch_unwind(AssertUnwindSafe(make)).map_err(|_| Panicked)?;
    let mut contained = Contained(Some(future));
    let output = poll_fn(|cx| {
        let Some(future) = contained.0.as_mut() else {
            return Poll::Ready(Err(Panicked));
        };
        match catch_unwind(AssertUnwindSafe(|| Pin::new(future).poll(cx))) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(Panicked)),
        }
    })
    .await;
    contained.release();
    output
}

/// Holds a future and drops it with its panics caught.
struct Contained<F>(Option<F>);

impl<F> Contained<F> {
    fn release(&mut self) {
        if let Some(future) = self.0.take() {
            let _ = catch_unwind(AssertUnwindSafe(move || drop(future)));
        }
    }
}

impl<F> Drop for Contained<F> {
    fn drop(&mut self) {
        self.release();
    }
}

/// Hard cap on the logged source chain, in bytes, marker included: the cap
/// the serving boundary's log applies.
const SOURCE_LIMIT: usize = 2048;

/// Appended when the source chain was cut at [`SOURCE_LIMIT`].
const CUT_MARKER: &str = "…";

/// Logs a failed creation once: its code, its reason and its source chain.
/// The message and the metadata are left out, as at the serving boundary.
pub(crate) fn log_failure(error: &AppError) {
    let source = source_chain(error);
    tracing::warn!(
        code = error.code().as_str(),
        reason = error.reason(),
        source = source.as_deref(),
        "creating a cached value failed"
    );
}

/// Logs a panic of a disposal hook or a finisher; the payload is never
/// shown.
pub(crate) fn log_dispose_panic() {
    tracing::error!("disposing a cached value panicked");
}

/// The source chain below `error`, joined with `": "` and cut at
/// [`SOURCE_LIMIT`] bytes on a character boundary; `None` without a source.
pub(crate) fn source_chain(error: &AppError) -> Option<String> {
    let mut next = StdError::source(error)?;
    let mut out = Capped::new(SOURCE_LIMIT - CUT_MARKER.len());
    loop {
        if write!(out, "{next}").is_err() {
            break;
        }
        let Some(cause) = next.source() else {
            break;
        };
        if out.write_str(": ").is_err() {
            break;
        }
        next = cause;
    }
    Some(out.finish())
}

/// A string writer that refuses to grow past `limit` bytes, cutting on a
/// character boundary, so an enormous `Display` never materialises in full.
struct Capped {
    text: String,
    limit: usize,
    cut: bool,
}

impl Capped {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            cut: false,
        }
    }

    fn finish(mut self) -> String {
        if self.cut {
            self.text.push_str(CUT_MARKER);
        }
        self.text
    }
}

impl fmt::Write for Capped {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.limit - self.text.len();
        if s.len() <= room {
            self.text.push_str(s);
            return Ok(());
        }
        self.text.push_str(&s[..s.floor_char_boundary(room)]);
        self.cut = true;
        Err(fmt::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Layer {
        text: String,
        below: Option<Box<Layer>>,
    }

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.text)
        }
    }

    impl StdError for Layer {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.below
                .as_deref()
                .map(|below| below as &(dyn StdError + 'static))
        }
    }

    #[test]
    fn the_chain_is_joined_and_cut_on_a_character_boundary() {
        assert_eq!(source_chain(&AppError::unavailable("x")), None);

        let error = AppError::unavailable("hidden").with_source(Layer {
            text: "outer".to_owned(),
            below: Some(Box::new(Layer {
                text: "inner".to_owned(),
                below: None,
            })),
        });
        assert_eq!(source_chain(&error).as_deref(), Some("outer: inner"));

        let long = AppError::unavailable("hidden").with_source(Layer {
            text: "é".repeat(5 * 1024),
            below: None,
        });
        let chain = source_chain(&long).unwrap();
        assert!(chain.len() <= SOURCE_LIMIT, "{}", chain.len());
        assert!(chain.ends_with(CUT_MARKER));
        assert!(chain.len() > SOURCE_LIMIT - 8);

        let at_separator = AppError::unavailable("hidden").with_source(Layer {
            text: "a".repeat(SOURCE_LIMIT - CUT_MARKER.len()),
            below: Some(Box::new(Layer {
                text: "b".to_owned(),
                below: None,
            })),
        });
        let chain = source_chain(&at_separator).unwrap();
        assert!(!chain.contains('b'));
    }

    #[tokio::test]
    async fn guarded_contains_panics_of_the_call_and_of_the_future() {
        assert_eq!(guarded(|| Box::pin(async { 7 })).await, Ok(7));
        let early = guarded(|| -> std::future::Ready<()> { panic!("before the future") }).await;
        assert_eq!(early, Err(Panicked));
        let late = guarded(|| {
            Box::pin(async {
                tokio::task::yield_now().await;
                panic!("inside the future");
            })
        })
        .await;
        assert_eq!(late, Err(Panicked));
    }

    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("drop panicked");
        }
    }

    /// Ready (or never ready) at once, and panics when dropped.
    struct Holding {
        ready: bool,
        _flag: PanicOnDrop,
    }

    impl Future for Holding {
        type Output = u32;

        fn poll(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<u32> {
            if self.ready {
                Poll::Ready(7)
            } else {
                Poll::Pending
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn guarded_contains_a_panicking_drop_after_the_end_and_when_abandoned() {
        let finished = guarded(|| Holding {
            ready: true,
            _flag: PanicOnDrop,
        })
        .await;
        assert_eq!(finished, Ok(7));

        let abandoned = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            guarded(|| Holding {
                ready: false,
                _flag: PanicOnDrop,
            }),
        )
        .await;
        assert!(abandoned.is_err());
    }

    #[tokio::test]
    async fn tasks_are_waited_for_and_retiring_values_counted() {
        let tracker = Arc::new(Tracker::default());
        tracker.wait_idle().await;

        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let handle = tracker
            .spawn_retiring(tracker.retire(), async move {
                let _ = gate.await;
            })
            .unwrap();
        assert_eq!(tracker.retiring(), 1);
        let creation = tracker.task(true);
        assert_eq!(tracker.creating(), 1);
        drop(creation);
        assert_eq!(tracker.creating(), 0);

        let waiting = tokio::spawn({
            let tracker = Arc::clone(&tracker);
            async move { tracker.wait_idle().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        release.send(()).unwrap();
        handle.await.unwrap();
        waiting.await.unwrap();
        assert_eq!(tracker.retiring(), 0);
    }

    #[test]
    fn outside_a_runtime_the_work_is_dropped() {
        let tracker = Arc::new(Tracker::default());
        assert!(tracker.spawn_retiring(tracker.retire(), async {}).is_none());
        assert_eq!(tracker.retiring(), 0);
    }

    #[test]
    fn outside_a_runtime_the_work_runs_on_the_fallback_until_it_shut_down() {
        let tracker = Arc::new(Tracker::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, ran) = tokio::sync::oneshot::channel();
        let handle = tracker
            .spawn_retiring_on(Some(runtime.handle()), tracker.retire(), async move {
                let _ = tx.send(());
            })
            .unwrap();
        runtime.block_on(async {
            handle.await.unwrap();
            ran.await.unwrap();
        });
        assert_eq!(tracker.retiring(), 0);

        let fallback = runtime.handle().clone();
        drop(runtime);
        let (tx, mut never) = tokio::sync::oneshot::channel::<()>();
        drop(
            tracker.spawn_retiring_on(Some(&fallback), tracker.retire(), async move {
                let _ = tx.send(());
            }),
        );
        assert!(never.try_recv().is_err());
        assert_eq!(tracker.retiring(), 0);
    }
}
