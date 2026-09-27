//! A bounded in-memory ring of recent log records.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Serialize, Serializer};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::fields::FieldVisitor;

/// One captured log event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LogRecord {
    /// Position in the buffer's history: strictly increasing, starting at 1,
    /// never reused (not even after [`LogBuffer::clear`]).
    pub seq: u64,
    /// Wall-clock time of the event, in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Severity.
    #[serde(serialize_with = "serialize_level")]
    pub level: Level,
    /// Target: the module path, or the original target of a record bridged
    /// from the `log` crate.
    pub target: String,
    /// The formatted message (empty when the event had none).
    pub message: String,
    /// The event's fields, preceded by the fields of its enclosing spans
    /// (an event field wins over a span field of the same name).
    pub fields: BTreeMap<String, String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde `serialize_with` passes `&T`.
fn serialize_level<S: Serializer>(level: &Level, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(level.as_str())
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A bounded, thread-safe ring of the most recent [`LogRecord`]s.
///
/// Clones share the same ring. Attach it to a subscriber with
/// [`LogBuffer::layer`] (or through `TelemetryOptions::log_buffer`); once
/// `capacity` records are held, each new record evicts the oldest.
#[derive(Clone)]
pub struct LogBuffer {
    shared: Arc<Shared>,
    clock: Clock,
}

struct Shared {
    capacity: usize,
    min_level: AtomicU8,
    ring: Mutex<Ring>,
}

#[derive(Default)]
struct Ring {
    records: VecDeque<LogRecord>,
    last_seq: u64,
}

impl LogBuffer {
    /// An empty buffer holding at most `capacity` records, capturing every
    /// level the subscriber lets through.
    pub fn new(capacity: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                capacity,
                min_level: AtomicU8::new(level_rank(Level::TRACE)),
                ring: Mutex::new(Ring {
                    records: VecDeque::with_capacity(capacity.min(4096)),
                    last_seq: 0,
                }),
            }),
            clock: Arc::new(system_millis),
        }
    }

    /// Only keep events at `level` or more severe (builder style).
    #[must_use]
    pub fn with_min_level(self, level: Level) -> Self {
        self.set_min_level(level);
        self
    }

    /// Replace the time source (milliseconds since the Unix epoch).
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Change the minimum level at runtime; applies to every clone.
    pub fn set_min_level(&self, level: Level) {
        self.shared
            .min_level
            .store(level_rank(level), Ordering::Relaxed);
    }

    /// The least severe level currently kept.
    pub fn min_level(&self) -> Level {
        level_from_rank(self.shared.min_level.load(Ordering::Relaxed))
    }

    /// Maximum number of records held.
    pub fn capacity(&self) -> usize {
        self.shared.capacity
    }

    /// Number of records currently held.
    pub fn len(&self) -> usize {
        self.ring().records.len()
    }

    /// Whether no record is held.
    pub fn is_empty(&self) -> bool {
        self.ring().records.is_empty()
    }

    /// Sequence number of the newest record ever stored (0 if none).
    pub fn last_seq(&self) -> u64 {
        self.ring().last_seq
    }

    /// Every held record, oldest first.
    pub fn snapshot(&self) -> Vec<LogRecord> {
        self.ring().records.iter().cloned().collect()
    }

    /// Held records with a sequence number greater than `seq`, oldest
    /// first. Pass the last `seq` seen to poll for new records; `0` returns
    /// everything held.
    pub fn since(&self, seq: u64) -> Vec<LogRecord> {
        let ring = self.ring();
        let skip = ring.records.partition_point(|record| record.seq <= seq);
        ring.records.iter().skip(skip).cloned().collect()
    }

    /// Drop every held record. Sequence numbers keep increasing.
    pub fn clear(&self) {
        self.ring().records.clear();
    }

    /// A `tracing_subscriber` layer feeding this buffer.
    pub fn layer(&self) -> LogBufferLayer {
        LogBufferLayer {
            buffer: self.clone(),
        }
    }

    fn accepts(&self, level: Level) -> bool {
        level_rank(level) <= self.shared.min_level.load(Ordering::Relaxed)
    }

    fn push(
        &self,
        level: Level,
        target: String,
        message: String,
        fields: BTreeMap<String, String>,
    ) {
        if self.shared.capacity == 0 {
            return;
        }
        let timestamp_ms = (self.clock)();
        let mut ring = self.ring();
        ring.last_seq += 1;
        let seq = ring.last_seq;
        if ring.records.len() == self.shared.capacity {
            ring.records.pop_front();
        }
        ring.records.push_back(LogRecord {
            seq,
            timestamp_ms,
            level,
            target,
            message,
            fields,
        });
    }

    fn ring(&self) -> MutexGuard<'_, Ring> {
        self.shared
            .ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogBuffer")
            .field("capacity", &self.capacity())
            .field("len", &self.len())
            .field("min_level", &self.min_level())
            .finish_non_exhaustive()
    }
}

fn system_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Severity rank: a lower rank is more severe.
fn level_rank(level: Level) -> u8 {
    match level {
        Level::ERROR => 1,
        Level::WARN => 2,
        Level::INFO => 3,
        Level::DEBUG => 4,
        _ => 5,
    }
}

fn level_from_rank(rank: u8) -> Level {
    match rank {
        1 => Level::ERROR,
        2 => Level::WARN,
        3 => Level::INFO,
        4 => Level::DEBUG,
        _ => Level::TRACE,
    }
}

/// Span fields remembered for the events inside the span.
struct SpanFields(BTreeMap<String, String>);

/// The layer that feeds a [`LogBuffer`]; build it with [`LogBuffer::layer`].
///
/// It never filters what other layers see: the buffer's minimum level only
/// decides what the buffer keeps.
#[derive(Debug, Clone)]
pub struct LogBufferLayer {
    buffer: LogBuffer,
}

impl<S> Layer<S> for LogBufferLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        let fields = visitor.into_text_fields();
        let mut extensions = span.extensions_mut();
        if let Some(existing) = extensions.get_mut::<SpanFields>() {
            existing.0.extend(fields);
        } else {
            extensions.insert(SpanFields(fields));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);
        let fields = visitor.into_text_fields();
        let mut extensions = span.extensions_mut();
        if let Some(existing) = extensions.get_mut::<SpanFields>() {
            existing.0.extend(fields);
        } else {
            extensions.insert(SpanFields(fields));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = *metadata.level();
        if !self.buffer.accepts(level) {
            return;
        }
        let mut fields = BTreeMap::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(span_fields) = span.extensions().get::<SpanFields>() {
                    fields.extend(span_fields.0.clone());
                }
            }
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let target = visitor
            .log_target
            .take()
            .unwrap_or_else(|| metadata.target().to_owned());
        let message = visitor.message.take().unwrap_or_default();
        fields.extend(visitor.into_text_fields());
        self.buffer.push(level, target, message, fields);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use tracing::subscriber::with_default;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    fn counting_clock() -> impl Fn() -> u64 + Send + Sync + 'static {
        let now = AtomicU64::new(1_000);
        move || now.fetch_add(1, Ordering::Relaxed)
    }

    fn capture(buffer: &LogBuffer, emit: impl FnOnce()) {
        with_default(Registry::default().with(buffer.layer()), emit);
    }

    #[test]
    fn records_events_with_span_fields() {
        let buffer = LogBuffer::new(8).with_clock(counting_clock());
        capture(&buffer, || {
            let outer = tracing::info_span!("request", request_id = "r-1", user = "a");
            let _outer = outer.enter();
            let inner = tracing::info_span!("db", user = "b", rows = tracing::field::Empty);
            inner.record("rows", 3u64);
            let _inner = inner.enter();
            tracing::info!(target: "orders::db", user = "c", "query {}", "done");
            tracing::debug!(attempt = 2u64);
        });
        let records = buffer.snapshot();
        assert_eq!(records.len(), 2);
        let first = &records[0];
        assert_eq!(first.seq, 1);
        assert_eq!(first.timestamp_ms, 1_000);
        assert_eq!(first.level, Level::INFO);
        assert_eq!(first.target, "orders::db");
        assert_eq!(first.message, "query done");
        assert_eq!(first.fields["request_id"], "r-1");
        assert_eq!(first.fields["user"], "c");
        assert_eq!(first.fields["rows"], "3");
        let second = &records[1];
        assert_eq!((second.seq, second.timestamp_ms), (2, 1_001));
        assert_eq!(second.message, "");
        assert_eq!(second.fields["attempt"], "2");
        assert_eq!(second.fields["user"], "b");
    }

    #[test]
    fn ring_evicts_oldest_and_keeps_sequence() {
        let buffer = LogBuffer::new(2);
        assert!(buffer.is_empty());
        capture(&buffer, || {
            for index in 0..5u64 {
                tracing::info!(index, "event");
            }
        });
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.capacity(), 2);
        assert_eq!(buffer.last_seq(), 5);
        let seqs: Vec<u64> = buffer.snapshot().iter().map(|record| record.seq).collect();
        assert_eq!(seqs, [4, 5]);
        assert_eq!(buffer.since(0).len(), 2);
        assert_eq!(buffer.since(4).len(), 1);
        assert!(buffer.since(5).is_empty());

        buffer.clear();
        assert!(buffer.is_empty());
        capture(&buffer, || tracing::info!("after clear"));
        assert_eq!(buffer.snapshot()[0].seq, 6);
        assert!(buffer.snapshot()[0].timestamp_ms > 0);
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let buffer = LogBuffer::new(0);
        capture(&buffer, || tracing::error!("dropped"));
        assert!(buffer.is_empty());
        assert_eq!(buffer.last_seq(), 0);
    }

    #[test]
    fn min_level_filters_and_is_shared() {
        let buffer = LogBuffer::new(16).with_min_level(Level::WARN);
        assert_eq!(buffer.min_level(), Level::WARN);
        capture(&buffer, || {
            tracing::error!("e");
            tracing::warn!("w");
            tracing::info!("i");
            tracing::debug!("d");
            tracing::trace!("t");
        });
        let levels: Vec<Level> = buffer.snapshot().iter().map(|r| r.level).collect();
        assert_eq!(levels, [Level::ERROR, Level::WARN]);

        let clone = buffer.clone();
        clone.set_min_level(Level::TRACE);
        assert_eq!(buffer.min_level(), Level::TRACE);
        buffer.clear();
        capture(&buffer, || {
            tracing::debug!("d");
            tracing::trace!("t");
        });
        assert_eq!(buffer.len(), 2);
    }

    #[test]
    fn level_ranks_round_trip() {
        for level in [
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ] {
            assert_eq!(level_from_rank(level_rank(level)), level);
        }
    }

    #[test]
    fn records_bridged_log_events_under_their_real_target() {
        let buffer = LogBuffer::new(4);
        capture(&buffer, || {
            tracing::event!(
                target: "log",
                tracing::Level::WARN,
                log.target = "orders::pool",
                log.module_path = "orders::pool",
                log.file = "src/pool.rs",
                log.line = 7u64,
                "pool exhausted"
            );
        });
        let record = &buffer.snapshot()[0];
        assert_eq!(record.target, "orders::pool");
        assert_eq!(record.message, "pool exhausted");
        assert!(record.fields.is_empty());
    }

    #[test]
    fn serializes_level_as_text() {
        let buffer = LogBuffer::new(1).with_clock(|| 42);
        capture(&buffer, || tracing::warn!(key = "v", "hello"));
        let json = serde_json::to_value(buffer.snapshot()).expect("records serialize");
        assert_eq!(
            json,
            serde_json::json!([{
                "seq": 1,
                "timestamp_ms": 42,
                "level": "WARN",
                "target": module_path!(),
                "message": "hello",
                "fields": { "key": "v" },
            }])
        );
    }

    #[test]
    fn debug_output_summarises_the_buffer() {
        let debug = format!("{:?}", LogBuffer::new(3));
        assert!(
            debug.starts_with("LogBuffer { capacity: 3, len: 0"),
            "{debug}"
        );
        assert!(format!("{:?}", LogBuffer::new(1).layer()).contains("LogBufferLayer"));
    }

    #[test]
    fn system_clock_is_after_the_epoch() {
        assert!(system_millis() > 0);
    }
}
