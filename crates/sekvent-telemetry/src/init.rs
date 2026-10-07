//! Process-wide subscriber installation.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex, PoisonError};

use tracing::Subscriber;
use tracing::level_filters::LevelFilter;
use tracing_log::AsLog;
use tracing_subscriber::fmt::format::JsonFields;
use tracing_subscriber::fmt::{MakeWriter, TestWriter};
use tracing_subscriber::layer::{Layer, Layered, SubscriberExt};
use tracing_subscriber::{EnvFilter, Registry};

use crate::buffer::LogBuffer;
use crate::format::{JsonFormat, LogFormat, ServiceTimer};

/// Environment variable selecting the output format (`compact`, `json`,
/// `pretty`). It overrides [`TelemetryOptions::format`].
pub const LOG_FORMAT_ENV: &str = "SEKVENT_LOG_FORMAT";
/// Environment variable with filter directives (`info,orders=debug`). Takes
/// precedence over [`RUST_LOG_ENV`].
pub const LOG_FILTER_ENV: &str = "SEKVENT_LOG";
/// The conventional filter variable, used when [`LOG_FILTER_ENV`] is unset.
pub const RUST_LOG_ENV: &str = "RUST_LOG";

const DEFAULT_DIRECTIVE: &str = "info";

/// Options for [`init`].
#[derive(Debug, Clone)]
pub struct TelemetryOptions {
    format: Option<LogFormat>,
    default_directive: String,
    log_buffer: Option<LogBuffer>,
}

impl Default for TelemetryOptions {
    fn default() -> Self {
        Self {
            format: None,
            default_directive: DEFAULT_DIRECTIVE.to_owned(),
            log_buffer: None,
        }
    }
}

impl TelemetryOptions {
    /// Compact output, `info` filter, no log buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// The format to use unless [`LOG_FORMAT_ENV`] names another one.
    #[must_use]
    pub fn format(mut self, format: LogFormat) -> Self {
        self.format = Some(format);
        self
    }

    /// Filter directives used when neither [`LOG_FILTER_ENV`] nor
    /// [`RUST_LOG_ENV`] is set (default `info`).
    #[must_use]
    pub fn default_directive(mut self, directive: impl Into<String>) -> Self {
        self.default_directive = directive.into();
        self
    }

    /// Also feed every event that passes the filter into `buffer`.
    #[must_use]
    pub fn log_buffer(mut self, buffer: LogBuffer) -> Self {
        self.log_buffer = Some(buffer);
        self
    }
}

/// Why [`init`] failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TelemetryError {
    /// [`LOG_FORMAT_ENV`] holds an unknown format name.
    #[error("{variable} must be one of compact, json or pretty")]
    InvalidFormat {
        /// The offending variable.
        variable: &'static str,
    },
    /// Filter directives failed to parse.
    #[error("invalid log filter in {origin}: {reason}")]
    InvalidFilter {
        /// Where the directives came from: a variable name or `the default directive`.
        origin: &'static str,
        /// The parser's explanation.
        reason: String,
    },
    /// A global subscriber not installed by this crate is already in place.
    #[error("another global tracing subscriber is already installed")]
    SubscriberAlreadySet,
}

/// Outcome of [`init`]. Holding it is not required; it only reports what
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitGuard {
    installed: bool,
    log_bridge: bool,
}

impl InitGuard {
    /// Whether this call installed the subscriber (`false` for the no-op
    /// repeat calls).
    pub fn installed(&self) -> bool {
        self.installed
    }

    /// Whether records of the `log` crate are forwarded (`false` when
    /// another `log` logger was already installed).
    pub fn log_bridge(&self) -> bool {
        self.log_bridge
    }
}

/// `Some(log_bridge)` once a subscriber has been installed by this crate.
static INSTALLED: Mutex<Option<bool>> = Mutex::new(None);

/// Install the global subscriber for `service`.
///
/// Output goes to stderr in the format from [`LOG_FORMAT_ENV`], else
/// [`TelemetryOptions::format`], else compact; ANSI colours only when stderr
/// is a terminal. Every event carries the service name. The filter comes from
/// [`LOG_FILTER_ENV`], else [`RUST_LOG_ENV`], else the default directive.
/// Records of the `log` crate are bridged with their original target.
///
/// Idempotent: after the first success, later calls change nothing and
/// return a guard whose [`InitGuard::installed`] is `false`.
pub fn init(service: &str, options: TelemetryOptions) -> Result<InitGuard, TelemetryError> {
    install(
        service,
        options,
        &|key| std::env::var(key).ok(),
        std::io::stderr,
        std::io::stderr().is_terminal(),
    )
}

/// Install a subscriber that writes through the test harness's capture, so
/// output shows up only for failing tests. Filter directives default to
/// `debug`. Returns whether this call installed it; a subscriber that is
/// already set (by this or any other means) is left in place.
pub fn try_init_for_tests() -> bool {
    install(
        "test",
        TelemetryOptions::new().default_directive("debug"),
        &|key| std::env::var(key).ok(),
        TestWriter::new(),
        false,
    )
    .is_ok_and(|guard| guard.installed())
}

fn install<W>(
    service: &str,
    options: TelemetryOptions,
    env: &dyn Fn(&str) -> Option<String>,
    writer: W,
    ansi: bool,
) -> Result<InitGuard, TelemetryError>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let mut state = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(log_bridge) = *state {
        return Ok(InitGuard {
            installed: false,
            log_bridge,
        });
    }
    let settings = resolve(&options, env)?;
    let subscriber = build(service, settings, options.log_buffer, writer, ansi);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| TelemetryError::SubscriberAlreadySet)?;
    let log_bridge = tracing_log::LogTracer::builder()
        .with_max_level(LevelFilter::current().as_log())
        .init()
        .is_ok();
    if !log_bridge {
        tracing::warn!("another `log` logger is installed; `log` records are not captured");
    }
    *state = Some(log_bridge);
    Ok(InitGuard {
        installed: true,
        log_bridge,
    })
}

/// Format and filter after applying the environment.
#[derive(Debug)]
struct Settings {
    format: LogFormat,
    filter: EnvFilter,
}

fn resolve(
    options: &TelemetryOptions,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Settings, TelemetryError> {
    let set = |key: &str| env(key).filter(|value| !value.trim().is_empty());

    let format = match set(LOG_FORMAT_ENV) {
        Some(name) => LogFormat::from_name(&name).ok_or(TelemetryError::InvalidFormat {
            variable: LOG_FORMAT_ENV,
        })?,
        None => options.format.unwrap_or_default(),
    };

    let (origin, directives) = if let Some(directives) = set(LOG_FILTER_ENV) {
        (LOG_FILTER_ENV, directives)
    } else if let Some(directives) = set(RUST_LOG_ENV) {
        (RUST_LOG_ENV, directives)
    } else {
        ("the default directive", options.default_directive.clone())
    };
    let filter =
        EnvFilter::try_new(&directives).map_err(|error| TelemetryError::InvalidFilter {
            origin,
            reason: error.to_string(),
        })?;

    Ok(Settings { format, filter })
}

type Base = Layered<EnvFilter, Registry>;
type BoxedLayer = Box<dyn Layer<Base> + Send + Sync>;

fn build<W>(
    service: &str,
    settings: Settings,
    buffer: Option<LogBuffer>,
    writer: W,
    ansi: bool,
) -> impl Subscriber + Send + Sync + 'static
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let service: Arc<str> = Arc::from(service);
    let output: BoxedLayer = match settings.format {
        LogFormat::Compact => tracing_subscriber::fmt::layer()
            .compact()
            .with_timer(ServiceTimer::new(service))
            .with_ansi(ansi)
            .with_writer(writer)
            .boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .pretty()
            .with_timer(ServiceTimer::new(service))
            .with_ansi(ansi)
            .with_writer(writer)
            .boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .fmt_fields(JsonFields::new())
            .event_format(JsonFormat::new(service))
            .with_ansi(false)
            .with_writer(writer)
            .boxed(),
    };
    let mut layers = vec![output];
    if let Some(buffer) = buffer {
        layers.push(buffer.layer().boxed());
    }
    Registry::default().with(settings.filter).with(layers)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;

    use serde_json::Value;
    use tracing::subscriber::with_default;

    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn settings(options: &TelemetryOptions, pairs: &[(&str, &str)]) -> Settings {
        resolve(options, &env_of(pairs)).expect("settings resolve")
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("not poisoned").clone()).expect("utf-8 output")
        }
    }

    impl io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("not poisoned")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn run(
        format: LogFormat,
        directives: &str,
        buffer: Option<LogBuffer>,
        emit: impl FnOnce(),
    ) -> String {
        crate::test_support::keep_interest_open();
        let out = Captured::default();
        let writer = out.clone();
        let settings = Settings {
            format,
            filter: EnvFilter::try_new(directives).expect("valid directives"),
        };
        let subscriber = build("billing", settings, buffer, move || writer.clone(), false);
        with_default(subscriber, emit);
        out.text()
    }

    #[test]
    fn format_precedence() {
        let json = TelemetryOptions::new().format(LogFormat::Json);
        assert_eq!(
            settings(&TelemetryOptions::new(), &[]).format,
            LogFormat::Compact
        );
        assert_eq!(settings(&json, &[]).format, LogFormat::Json);
        assert_eq!(
            settings(&json, &[(LOG_FORMAT_ENV, "Pretty")]).format,
            LogFormat::Pretty
        );
        assert_eq!(
            settings(&json, &[(LOG_FORMAT_ENV, "  ")]).format,
            LogFormat::Json
        );
        assert_eq!(
            resolve(&json, &env_of(&[(LOG_FORMAT_ENV, "xml")])).map(|s| s.format),
            Err(TelemetryError::InvalidFormat {
                variable: LOG_FORMAT_ENV
            })
        );
    }

    #[test]
    fn filter_precedence() {
        let options = TelemetryOptions::new().default_directive("warn");
        let shown = |pairs: &[(&str, &str)]| {
            settings(&options, pairs)
                .filter
                .to_string()
                .to_ascii_lowercase()
        };
        assert_eq!(shown(&[]), "warn");
        assert_eq!(shown(&[(RUST_LOG_ENV, "debug")]), "debug");
        assert_eq!(
            shown(&[(RUST_LOG_ENV, "debug"), (LOG_FILTER_ENV, "error")]),
            "error"
        );
        assert_eq!(
            shown(&[(LOG_FILTER_ENV, ""), (RUST_LOG_ENV, "trace")]),
            "trace"
        );
    }

    #[test]
    fn invalid_filters_name_their_origin() {
        let bad = "orders=loud";
        for (pairs, origin) in [
            (vec![(LOG_FILTER_ENV, bad)], LOG_FILTER_ENV),
            (vec![(RUST_LOG_ENV, bad)], RUST_LOG_ENV),
            (vec![], "the default directive"),
        ] {
            let options = TelemetryOptions::new().default_directive(bad);
            match resolve(&options, &env_of(&pairs)) {
                Err(TelemetryError::InvalidFilter {
                    origin: got,
                    reason,
                }) => {
                    assert_eq!(got, origin);
                    assert!(!reason.is_empty());
                }
                other => panic!("expected an invalid filter, got {other:?}"),
            }
        }
    }

    #[test]
    fn errors_display() {
        assert_eq!(
            TelemetryError::InvalidFormat {
                variable: LOG_FORMAT_ENV
            }
            .to_string(),
            "SEKVENT_LOG_FORMAT must be one of compact, json or pretty"
        );
        assert_eq!(
            TelemetryError::SubscriberAlreadySet.to_string(),
            "another global tracing subscriber is already installed"
        );
        assert!(
            TelemetryError::InvalidFilter {
                origin: RUST_LOG_ENV,
                reason: "x".into()
            }
            .to_string()
            .contains("RUST_LOG")
        );
    }

    #[test]
    fn compact_lines_carry_the_service() {
        let text = run(LogFormat::Compact, "info", None, || {
            tracing::info!(order = 7u64, "created");
            tracing::debug!("filtered out");
        });
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains(" service=billing "), "{text}");
        assert!(text.contains("created"), "{text}");
        assert!(text.contains("order=7"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
    }

    #[test]
    fn pretty_output_carries_the_service() {
        let text = run(LogFormat::Pretty, "info", None, || {
            tracing::warn!("careful");
        });
        assert!(text.contains("service=billing"), "{text}");
        assert!(text.contains("careful"), "{text}");
    }

    #[test]
    fn json_lines_are_objects_with_service_fields_and_spans() {
        let text = run(LogFormat::Json, "info", None, || {
            let span = tracing::info_span!("request", request_id = "r-9");
            let _entered = span.enter();
            tracing::info!(order = 7u64, "created");
            tracing::event!(
                target: "log",
                tracing::Level::WARN,
                log.target = "orders::legacy",
                log.line = 3u64,
                "bridged"
            );
            let bare = tracing::info_span!("bare");
            let _bare = bare.enter();
            tracing::error!(done = true);
        });
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect();
        assert_eq!(lines.len(), 3, "{text}");

        let first = &lines[0];
        assert_eq!(first["service"], "billing");
        assert_eq!(first["level"], "INFO");
        assert_eq!(first["target"], module_path!());
        assert_eq!(first["message"], "created");
        assert_eq!(first["fields"]["order"], 7);
        assert_eq!(first["spans"][0]["name"], "request");
        assert_eq!(first["spans"][0]["request_id"], "r-9");
        assert!(first["timestamp"].as_str().is_some_and(|t| !t.is_empty()));

        let second = &lines[1];
        assert_eq!(second["target"], "orders::legacy");
        assert!(second.get("fields").is_none(), "{second}");

        let third = &lines[2];
        assert!(third.get("message").is_none(), "{third}");
        assert_eq!(third["fields"]["done"], true);
        assert_eq!(third["spans"][1]["name"], "bare");
    }

    #[test]
    fn events_without_spans_have_no_span_list() {
        let text = run(LogFormat::Json, "info", None, || tracing::info!("alone"));
        let line: Value = serde_json::from_str(text.trim()).expect("JSON line");
        assert!(line.get("spans").is_none(), "{line}");
    }

    #[test]
    fn buffer_sees_what_the_filter_lets_through() {
        let buffer = LogBuffer::new(8);
        run(LogFormat::Compact, "info", Some(buffer.clone()), || {
            tracing::info!("kept");
            tracing::debug!("dropped");
        });
        let messages: Vec<String> = buffer.snapshot().into_iter().map(|r| r.message).collect();
        assert_eq!(messages, ["kept"]);
    }

    #[test]
    fn options_builder() {
        let buffer = LogBuffer::new(1);
        let options = TelemetryOptions::new()
            .format(LogFormat::Pretty)
            .default_directive("debug")
            .log_buffer(buffer);
        assert_eq!(options.format, Some(LogFormat::Pretty));
        assert_eq!(options.default_directive, "debug");
        assert!(options.log_buffer.is_some());
        let guard = InitGuard {
            installed: true,
            log_bridge: false,
        };
        assert!(guard.installed() && !guard.log_bridge());
    }
}
