# Telemetry (`sekvent::telemetry`)

`sekvent::telemetry` sets up logging for a service and gives it the
per-request plumbing: one call installs the process-wide `tracing`
subscriber (filter from `SEKVENT_LOG` / `RUST_LOG`, compact, pretty or JSON
output on stderr, every line stamped with the service name, the `log` crate
bridged), an optional in-memory `LogBuffer` keeps recent records for an
admin endpoint or a test, tower layers ensure every HTTP request carries an
`x-request-id` and emit one access-log event per request, and
`truncate_for_log` bounds untrusted text before it reaches a log line.

## Enable it

| | |
|---|---|
| Facade feature | `telemetry` (on by default) |
| Module | `use sekvent::telemetry::{init, TelemetryOptions, LogFormat, LogBuffer, truncate_for_log};` |
| Layers | `sekvent::telemetry::request_id`, `sekvent::telemetry::access_log` |
| Internal crate | `sekvent-telemetry` (no features) |

You also depend on `tracing` itself for the `info!`/`warn!` macros.

## Quick example

```rust
use std::process::ExitCode;

use sekvent::telemetry::{self, LogFormat, TelemetryOptions};

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = telemetry::init(
        "billing",
        TelemetryOptions::new().format(LogFormat::Json),
    ) {
        eprintln!("billing: cannot initialise logging: {error}");
        return ExitCode::FAILURE;
    }
    tracing::info!(port = 8080u16, "starting");
    // … load config, build and run the service …
    ExitCode::SUCCESS
}
```

Initialise telemetry first, before reading configuration, so configuration
errors are logged in the chosen format.

## Concepts

- **One global subscriber.** `init` installs it once per process; later
  calls are no-ops. It writes to stderr.
- **Filter precedence:** `SEKVENT_LOG`, else `RUST_LOG`, else the default
  directive (`info` unless `TelemetryOptions::default_directive` says
  otherwise). A variable that is set but blank counts as unset.
- **Format precedence:** `SEKVENT_LOG_FORMAT`, else
  `TelemetryOptions::format`, else `compact`.
- **Service name on every line**: `service=<name>` after the timestamp in
  the text formats, a `service` key in JSON.
- **`log` bridge:** records of the `log` crate are forwarded with their
  original target, unless another `log` logger was installed first.
- **Targets the framework uses:** `sekvent::access` (access log) and
  `sekvent::error` (server-side failures logged at the serving boundary,
  see [error](error.md#how-the-serving-boundary-logs)).

## How to initialise logging

```rust
pub fn init(service: &str, options: TelemetryOptions) -> Result<InitGuard, TelemetryError>;
```

`TelemetryOptions` (`Default`, `Clone`):

| Builder | Default | Effect |
|---|---|---|
| `TelemetryOptions::new()` | | compact, `info`, no buffer |
| `.format(LogFormat)` | `Compact` | format unless `SEKVENT_LOG_FORMAT` names another |
| `.default_directive("info,orders=debug")` | `info` | filter when neither env variable is set |
| `.log_buffer(LogBuffer)` | none | also feed every event that passes the filter into the buffer |

`InitGuard` only reports what happened; holding it is not required:
`installed()` is `false` for repeat calls, `log_bridge()` is `false` when
another `log` logger was already in place (a warning is logged then).

`TelemetryError` (`#[non_exhaustive]`):

| Variant | When |
|---|---|
| `InvalidFormat { variable }` | `SEKVENT_LOG_FORMAT must be one of compact, json or pretty` |
| `InvalidFilter { origin, reason }` | directives failed to parse; `origin` is `SEKVENT_LOG`, `RUST_LOG` or `the default directive` |
| `SubscriberAlreadySet` | a global subscriber not installed by sekvent is already set |

ANSI colours are used only when stderr is a terminal (never in JSON).

## Output formats

`LogFormat` (`#[non_exhaustive]`, `Default = Compact`); `from_name` parses
`compact`, `json`, `pretty` ignoring case and surrounding whitespace;
`as_str` / `Display` give the canonical name.

| Format | Shape |
|---|---|
| `Compact` | one human-readable line per event: timestamp, `service=billing`, level, target, message and fields |
| `Pretty` | multi-line, for local development |
| `Json` | one object per line, for log shippers |

A JSON line:

```json
{"timestamp":"2026-10-08T09:30:00.123456Z","level":"INFO","service":"billing","target":"billing::api","message":"created","fields":{"order":7},"spans":[{"name":"request","request_id":"0192…","method":"POST","path":"/api/orders"}]}
```

- `message` is omitted when the event has none; `fields` when it has no
  other fields; `spans` (root first, each with `name` and its fields) when
  the event is outside any span.
- Records bridged from the `log` crate show their original target, and the
  bridge's bookkeeping fields (`log.file`, `log.line`, …) are dropped.

## Configuration keys

| Variable | Constant | Meaning |
|---|---|---|
| `SEKVENT_LOG` | `LOG_FILTER_ENV` | `EnvFilter` directives, e.g. `info,orders=debug,sekvent::access=warn`; wins over `RUST_LOG` |
| `RUST_LOG` | `RUST_LOG_ENV` | used when `SEKVENT_LOG` is unset or blank |
| `SEKVENT_LOG_FORMAT` | `LOG_FORMAT_ENV` | `compact`, `json` or `pretty`; overrides `TelemetryOptions::format` |

Both `SEKVENT_*` keys are in the framework's reserved list, so
[`sekvent::config::load`](config.md#configuration-keys) accepts them and
suggests them for typos such as `SEKVENT_LOG_FORMT`.

Useful filters:

| Goal | `SEKVENT_LOG` |
|---|---|
| Default | `info` |
| Only failed requests in the access log | `info,sekvent::access=warn` |
| No access log at all | `info,sekvent::access=off` |
| Debug one module | `info,billing::invoices=debug` |

## How to keep recent logs in memory (`LogBuffer`)

```rust
use sekvent::telemetry::{self, LogBuffer, TelemetryOptions};
use tracing::Level;

let buffer = LogBuffer::new(1_000).with_min_level(Level::INFO);
telemetry::init("billing", TelemetryOptions::new().log_buffer(buffer.clone()))?;

// Later, e.g. in an admin handler (LogRecord is `Serialize`):
let mut last_seen = 0;
let fresh = buffer.since(last_seen);
if let Some(newest) = fresh.last() {
    last_seen = newest.seq;
}
```

| Method | Purpose |
|---|---|
| `LogBuffer::new(capacity)` | Ring of at most `capacity` records (0 keeps nothing); captures every level the subscriber lets through. |
| `.with_min_level(Level)` / `set_min_level(Level)` / `min_level()` | Keep only events at that level or more severe; the setter applies to every clone at runtime. |
| `.with_clock(impl Fn() -> u64)` | Replace the time source (Unix milliseconds), for deterministic tests. |
| `snapshot()` | Every held record, oldest first. |
| `since(seq)` | Records with a greater sequence number; poll with the last `seq` seen, `0` for all. |
| `last_seq()`, `len()`, `is_empty()`, `capacity()`, `clear()` | Bookkeeping; `clear` keeps sequence numbers increasing. |
| `layer() -> LogBufferLayer` | A `tracing_subscriber` layer feeding this buffer, for a subscriber you build yourself. It never filters what other layers see. |

Clones share the same ring. `LogRecord` (`#[non_exhaustive]`, `Serialize`)
has `seq` (from 1, never reused), `timestamp_ms`, `level` (serialized as
`"INFO"`, …), `target`, `message` (empty when absent) and `fields:
BTreeMap<String, String>` (the enclosing spans' fields, then the event's; an
event field wins over a span field of the same name).

A buffer holds whatever your events contain; expose it only on an
authenticated admin endpoint.

## How to give every request an id (`request_id`)

`RequestIdLayer` (tower) makes sure every HTTP request carries a usable
`x-request-id`:

- a valid incoming value (1–128 visible ASCII characters, no spaces) is
  kept, repeated headers collapse to one; anything else is replaced by a
  fresh UUID v7;
- the id is stored in the request extensions as `RequestId` and echoed on
  the response unless the handler set its own;
- the inner call runs in a `request` span with a `request_id` field.

| Item | Purpose |
|---|---|
| `RequestIdLayer::new()` / `RequestIdService::new(inner)` | The middleware. |
| `request_id(&Request<B>) -> Option<&str>` | The id the layer stored. |
| `REQUEST_ID_HEADER` | `x-request-id` as a `HeaderName`. |
| `MAX_REQUEST_ID_LEN` | 128. |
| `is_valid_request_id(&[u8]) -> bool` | The acceptance rule above. |
| `new_request_id() -> HeaderValue` | A fresh hyphenated UUID v7. |
| `MakeRequestUuidV7` | A `tower_http` `MakeRequestId` for stacks built from `SetRequestIdLayer` directly (which keeps incoming values unvalidated). |
| `RequestId` | Re-export of `tower_http::request_id::RequestId`. |

The same validation is applied by `sekvent::context::headers::from_headers`,
so the id in the `CallContext` and the one in the logs agree
([context](context.md)).

## How to log every request (`access_log`)

`AccessLogLayer` is `RequestIdLayer` plus an access log; a stack uses one or
the other, not both. The [combined server](server.md) installs it for you
(`Server::builder().access_log(bool)`), with the health endpoints quiet; use
the layer directly only for a router you serve yourself.

```rust
use sekvent::telemetry::access_log::AccessLogLayer;

let router = axum::Router::new()
    .route("/orders", axum::routing::get(list_orders))
    .layer(AccessLogLayer::new().quiet(["/livez", "/readyz", "/internal/"]));
```

| Builder | Default | Effect |
|---|---|---|
| `AccessLogLayer::new()` / `Default` | | events on, no quiet paths |
| `.quiet(paths)` | none | log these paths at `debug`; an entry ending in `/` is a prefix; calls add up |
| `.events(bool)` | `true` | turn the completion event off; ids and the span stay |

Per request it produces:

- a **`request` span** with `request_id`, `method` and `path`, recording
  `status`, `grpc_status` and `latency_ms` on completion;
- exactly one **event**, target `sekvent::access` (`ACCESS_LOG_TARGET`),
  message `request completed`, when the response body ends, fails or is
  dropped (or the inner service fails or its future is dropped). A response
  with no body by definition (`HEAD`, `204`, `304`) completes when its
  headers are ready.

Event fields:

| Field | Value |
|---|---|
| `request_id` | also on the event, so it survives a filter that disables the `info` span |
| `method` | HTTP method |
| `path` | URI path, never the query, at most 256 characters |
| `route` | the `RouteTemplate` inner routing put on the response (e.g. `/api/orders/{id}`); the path for gRPC; empty when nothing matched |
| `protocol` | `http`, `grpc` or `grpc-web`, by the request's content type |
| `status` | HTTP status |
| `grpc_status` | gRPC only: from the trailers, a trailers-only answer's headers, or the trailer frame of a gRPC-Web body (binary or base64 text) |
| `latency_ms` | to the end of the body |
| `aborted` | the response did not reach its end |

Levels:

| Level | When |
|---|---|
| `debug` | quiet paths |
| `warn` | HTTP 5xx, or `grpc_status` 2, 13 or 15 (`UNKNOWN`, `INTERNAL`, `DATA_LOSS`) |
| `info` | everything else |

Headers, query strings, bodies, peer addresses and user agents are never
logged. Paths are, so secrets must never travel in paths. To read a
gRPC-Web trailer frame the layer holds at most a frame header, three base64
characters and an 8 KiB trailer block; a longer block leaves `grpc_status`
unset.

`RouteTemplate(pub String)` is the response extension your router sets to
report the matched template; the combined server does this for its REST
routes. Other public types: `AccessLogService<S>`, `AccessLogBody<B>` (the
wrapped response body) and `ResponseFuture<F>`.

## How to log untrusted text (`truncate_for_log`)

```rust
use sekvent::telemetry::truncate_for_log;

// Log the status and fields you chose to keep, not the upstream body.
fn log_rejection(status: u16, error_code: &str, sku: &str) {
    tracing::warn!(
        status,
        error_code = %truncate_for_log(error_code, 64), // the upstream's error code string
        sku = %truncate_for_log(sku, 64),               // a user-supplied identifier
        "inventory rejected the request"
    );
}
```

`truncate_for_log(s: &str, max_chars: usize) -> Cow<'_, str>` cuts on a
character boundary and appends `…(+N chars)` naming how many characters were
dropped; text that fits is returned borrowed. `truncate_for_log("abcdef", 4)`
is `abcd…(+2 chars)`. Use it for untrusted text that is not secret but may
be arbitrarily long, such as an upstream's error code string or an
identifier a user sent. It bounds length; it does **not** make secrets safe
to log, and text shorter than the limit is logged whole.

Do not log upstream bodies. If you must for debugging, log them at `debug`
level, truncated, and only from upstreams known not to echo secrets
(credentials, tokens, request bodies) back.

## Testing tips

- `try_init_for_tests() -> bool` installs a subscriber that writes through
  the test harness's capture (output appears only for failing tests), with
  `debug` as the default directive. It returns whether this call installed
  it and leaves any existing subscriber alone; call it at the start of each
  test that wants logs. After it, `init` is a no-op.
- To assert on log output, attach a `LogBuffer` to a scoped subscriber
  instead of the global one (needs `tracing-subscriber` as a
  dev-dependency):

  ```rust
  use sekvent::telemetry::LogBuffer;
  use tracing_subscriber::{Registry, layer::SubscriberExt};

  let buffer = LogBuffer::new(16).with_clock(|| 1_700_000_000_000);
  tracing::subscriber::with_default(Registry::default().with(buffer.layer()), || {
      tracing::info!(order = 7u64, "created");
  });
  let records = buffer.snapshot();
  assert_eq!(records[0].message, "created");
  assert_eq!(records[0].fields["order"], "7");
  assert_eq!(records[0].timestamp_ms, 1_700_000_000_000);
  ```

  In async tests use `tracing::subscriber::set_default(..)` and keep the
  guard alive for the test's duration.
- The access event is emitted when the body finishes: collect the response
  body (`http_body_util::BodyExt::collect`) before asserting on the buffer.
- Tests that install a global subscriber, or a level-filtered subscriber
  (which lowers tracing's process-wide maximum level while installed), go in
  their own test binary so they do not hide events from neighbouring tests.

## Pitfalls and security rules

- Never log `Secret::expose()`, tokens, passwords or database URLs.
- Do not log upstream bodies; log the status and allowlisted fields. If a
  debugging session needs a body, log it at `debug` level, truncated, and
  only from an upstream known not to echo secrets. `truncate_for_log` only
  bounds length and does not redact: a short body is logged whole.
  `Secret` formats as `[redacted]`.
- Do not log an `AppError` before returning it from a handler; the serving
  boundary already logs server-side failures once ([error](error.md)).
- `init` fails with `SubscriberAlreadySet` if some other crate installed a
  global subscriber first; call it at the top of `main`.
- An invalid `SEKVENT_LOG` or `SEKVENT_LOG_FORMAT` is a startup error, not
  a silent fallback.
- Use either `RequestIdLayer` or `AccessLogLayer` in one stack, not both.

## See also

- [Server](server.md): the combined server's layer stack and `access_log`
- [Error](error.md): boundary logging of server-side failures
- [Context](context.md): request ids in `CallContext` and header propagation
- [Config](config.md): reserved `SEKVENT_*` keys
- [Testing](testing.md)
