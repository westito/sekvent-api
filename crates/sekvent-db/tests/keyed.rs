//! The keyed cache's runtime unit and its failure log, on tokio's paused
//! clock.
//!
//! This file is its own test binary, so it installs a global subscriber
//! that keeps every callsite's interest open and records per thread.

#![cfg(feature = "keyed")]

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, Mutex, Once};

use sekvent_db::Keyed;
use sekvent_db::reasons::KEYED_FACTORY_PANICKED;
use sekvent_error::AppError;
use tokio::sync::{mpsc, watch};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::{Interest, Subscriber};
use tracing::{Event, Level, Metadata};

#[cfg(feature = "runtime")]
mod unit {
    use std::time::Duration;

    use sekvent_db::Keyed;
    use sekvent_runtime::{Runtime, RuntimeHandle, Stage, UnitExit, UnitPolicy};
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    const UNIT: &str = "tenant-cache";

    type Cache = Keyed<u32, u32>;

    fn cache(dispose: mpsc::UnboundedSender<u32>, hold: bool) -> Cache {
        Keyed::builder(|key: u32| async move { Ok(key * 10) })
            .evict_idle_after(Some(Duration::from_secs(60)))
            .reap_interval(Duration::from_secs(30))
            .dispose(move |key, _value| {
                let _ = dispose.send(key);
                async move {
                    if hold {
                        std::future::pending::<()>().await;
                    }
                }
            })
            .build()
            .unwrap()
    }

    async fn start(cache: &Cache) -> RuntimeHandle {
        Runtime::builder()
            .without_signals()
            .unit(
                UNIT,
                Stage::Infrastructure,
                UnitPolicy::Critical,
                cache.clone().into_unit(),
            )
            .build()
            .unwrap()
            .start()
            .await
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn the_unit_evicts_idle_entries_and_closes_the_cache_on_shutdown() {
        let (tx, mut disposed) = mpsc::unbounded_channel();
        let cache = cache(tx, false);
        let begin = Instant::now();
        drop(cache.get(&1).await.unwrap());
        let held = cache.get(&2).await.unwrap();
        let handle = start(&cache).await;

        assert_eq!(disposed.recv().await, Some(1));
        assert!(begin.elapsed() >= Duration::from_secs(60));
        let stats = cache.stats();
        assert_eq!((stats.warm, stats.in_use, stats.evictions), (1, 1, 1));

        drop(held);
        handle.shutdown();
        let report = handle.wait().await.unwrap();
        assert_eq!(report.unit(UNIT).unwrap().exit, UnitExit::Completed);
        assert!(cache.is_closed());
        assert_eq!(disposed.try_recv(), Ok(2));
        assert_eq!(cache.stats().retiring, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_disposal_that_outlasts_the_bound_is_abandoned_before_the_grace_ends() {
        let (tx, mut disposed) = mpsc::unbounded_channel();
        let cache = cache(tx, true);
        drop(cache.get(&1).await.unwrap());
        let handle = start(&cache).await;

        let begin = Instant::now();
        handle.shutdown();
        let report = handle.wait().await.unwrap();
        let waited = begin.elapsed();
        assert!(waited >= Duration::from_secs(9), "{waited:?}");
        assert!(waited < Duration::from_secs(10), "{waited:?}");
        assert_eq!(report.unit(UNIT).unwrap().exit, UnitExit::Completed);
        assert!(cache.is_closed());
        assert_eq!(disposed.try_recv(), Ok(1));
        assert_eq!(cache.stats().retiring, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_disposal_that_never_ends_does_not_stall_the_next_sweep() {
        let (tx, mut disposed) = mpsc::unbounded_channel();
        let cache = cache(tx, true);
        drop(cache.get(&1).await.unwrap());
        let held = cache.get(&2).await.unwrap();
        let handle = start(&cache).await;

        assert_eq!(disposed.recv().await, Some(1));
        drop(held);
        let next = tokio::time::timeout(Duration::from_secs(600), disposed.recv()).await;
        assert_eq!(next, Ok(Some(2)));
        let stats = cache.stats();
        assert_eq!((stats.warm, stats.evictions, stats.retiring), (0, 2, 2));

        handle.shutdown();
        let report = handle.wait().await.unwrap();
        assert_eq!(report.unit(UNIT).unwrap().exit, UnitExit::Completed);
        assert!(cache.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_value_still_referenced_at_shutdown_is_disposed_when_it_is_let_go() {
        let (tx, mut disposed) = mpsc::unbounded_channel();
        let cache = cache(tx, false);
        let held = cache.get(&1).await.unwrap();
        let handle = start(&cache).await;

        handle.shutdown();
        let report = handle.wait().await.unwrap();
        assert_eq!(report.unit(UNIT).unwrap().exit, UnitExit::Completed);
        assert!(cache.is_closed());
        assert!(disposed.try_recv().is_err());
        assert_eq!(cache.stats().retiring, 1);
        assert_eq!(*held, 10);

        drop(held);
        assert_eq!(disposed.recv().await, Some(1));
    }
}

/// One recorded event.
struct Captured {
    level: Level,
    fields: BTreeMap<String, String>,
}

/// Records events when `events` is set; the global default (no `events`)
/// records nothing but keeps every callsite's interest open.
struct Recorder {
    events: Option<Arc<Mutex<Vec<Captured>>>>,
}

impl Subscriber for Recorder {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.events.is_some() && metadata.target().starts_with("sekvent_db")
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if let Some(events) = &self.events {
            events.lock().unwrap().push(Captured {
                level: *event.metadata().level(),
                fields: fields.0,
            });
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

/// Starts recording this thread's `sekvent_db` events until the guard
/// drops.
fn record() -> (Arc<Mutex<Vec<Captured>>>, tracing::subscriber::DefaultGuard) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::subscriber::set_global_default(Recorder { events: None })
            .expect("no other test installs a global subscriber");
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let guard = tracing::subscriber::set_default(Recorder {
        events: Some(Arc::clone(&events)),
    });
    (events, guard)
}

fn warnings(events: &Mutex<Vec<Captured>>) -> Vec<BTreeMap<String, String>> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.level == Level::WARN)
        .map(|event| event.fields.clone())
        .collect()
}

fn mentions(fields: &BTreeMap<String, String>, text: &str) -> bool {
    fields.values().any(|value| value.contains(text))
}

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

/// A cache whose factory reports each call, waits for `open`, then fails
/// with `error()`.
fn failing(
    error: fn() -> AppError,
) -> (
    Keyed<u32, u32>,
    mpsc::UnboundedReceiver<()>,
    watch::Sender<bool>,
) {
    let (called_tx, called) = mpsc::unbounded_channel();
    let (open, gate) = watch::channel(false);
    let cache = Keyed::builder(move |_key: u32| {
        let _ = called_tx.send(());
        let mut gate = gate.clone();
        async move {
            let _ = gate.wait_for(|open| *open).await;
            Err::<u32, _>(error())
        }
    })
    .build()
    .unwrap();
    (cache, called, open)
}

fn upstream_error() -> AppError {
    AppError::unavailable("message-text")
        .with_reason("UPSTREAM_DOWN")
        .with_metadata("detail", "metadata-text")
        .with_source(Layer {
            text: "outer".to_owned(),
            below: Some(Box::new(Layer {
                text: "inner".to_owned(),
                below: None,
            })),
        })
}

fn huge_error() -> AppError {
    AppError::unavailable("message-text").with_source(Layer {
        text: "é".repeat(5 * 1024),
        below: None,
    })
}

#[tokio::test(start_paused = true)]
async fn a_failed_creation_is_logged_once_with_code_reason_and_source_chain() {
    let (events, _guard) = record();
    let (cache, mut called, open) = failing(upstream_error);
    let waiters: Vec<_> = (0..20)
        .map(|_| {
            let cache = cache.clone();
            tokio::spawn(async move { cache.get(&1).await })
        })
        .collect();
    called.recv().await.unwrap();
    open.send_replace(true);
    for waiter in waiters {
        let error = waiter.await.unwrap().unwrap_err();
        assert_eq!(error.reason(), Some("UPSTREAM_DOWN"));
        assert!(StdError::source(&error).is_none());
    }
    assert!(called.try_recv().is_err());

    let warnings = warnings(&events);
    assert_eq!(warnings.len(), 1, "one creation, one event");
    let fields = &warnings[0];
    assert_eq!(fields.get("code").map(String::as_str), Some("UNAVAILABLE"));
    assert_eq!(
        fields.get("reason").map(String::as_str),
        Some("UPSTREAM_DOWN")
    );
    assert_eq!(
        fields.get("source").map(String::as_str),
        Some("outer: inner")
    );
    assert!(!mentions(fields, "message-text"));
    assert!(!mentions(fields, "metadata-text"));
}

#[tokio::test(start_paused = true)]
async fn a_long_source_is_cut_at_two_kib_on_a_character_boundary() {
    let (events, _guard) = record();
    let (cache, _called, open) = failing(huge_error);
    open.send_replace(true);
    assert!(cache.get(&1).await.is_err());

    let warnings = warnings(&events);
    assert_eq!(warnings.len(), 1);
    let source = &warnings[0]["source"];
    assert!(source.len() <= 2048, "{}", source.len());
    assert!(source.len() > 2000, "{}", source.len());
    assert!(source.ends_with('…'));
    assert!(source.trim_end_matches('…').chars().all(|c| c == 'é'));
    assert!(!mentions(&warnings[0], "message-text"));
}

#[tokio::test(start_paused = true)]
async fn a_panic_is_logged_with_its_code_and_reason_and_without_its_payload() {
    let (events, _guard) = record();
    let cache = Keyed::builder(|key: u32| async move {
        assert!(key == 0, "panic-payload-text");
        Ok(key)
    })
    .build()
    .unwrap();
    let error = cache.get(&1).await.unwrap_err();
    assert_eq!(error.reason(), Some(KEYED_FACTORY_PANICKED));
    assert!(!format!("{error:?}").contains("panic-payload-text"));

    let warnings = warnings(&events);
    assert_eq!(warnings.len(), 1);
    let fields = &warnings[0];
    assert_eq!(fields.get("code").map(String::as_str), Some("INTERNAL"));
    assert_eq!(
        fields.get("reason").map(String::as_str),
        Some(KEYED_FACTORY_PANICKED)
    );
    assert!(!fields.contains_key("source"));
    assert!(!mentions(fields, "panic-payload-text"));
}
