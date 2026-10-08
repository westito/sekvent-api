# Errors (`sekvent::error`)

`sekvent::error` is the one error model every sekvent service returns,
whatever the transport. `AppError` carries a canonical `ErrorCode` (the gRPC
code set) and a caller-visible part (message, machine-readable reason,
domain, metadata, retry hint, field violations), plus an internal source
chain that only the server's logs ever see. Lossless mappings turn it into an
HTTP JSON response or a `tonic::Status` with standard `google.rpc` details,
and back again on the client side.

## Enable it

| | |
|---|---|
| Facade feature | `error` (on by default) |
| HTTP mapping | `error-http`: `impl IntoResponse for AppError`, `sekvent::error::http`, and `serde` for `WireError` |
| gRPC mapping | `error-grpc`: `From<AppError> for tonic::Status` and back, `sekvent::error::grpc` |
| Module | `use sekvent::error::{AppError, ErrorCode, …};` (`AppError` and `ErrorCode` are also in `sekvent::prelude`) |
| Internal crate | `sekvent-error` (features `serde`, `http`, `grpc`) |

```toml
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["error-http", "error-grpc"] }
```

The `runtime` feature (on by default) already compiles both mappings in,
because the server needs them; enable `error-http` / `error-grpc` anyway
when your code names them, so it keeps compiling if `runtime` is turned off.

## Quick example

```rust
use std::time::Duration;

use sekvent::error::{AppError, ErrorCode, Result};

fn find_order(id: &str) -> Result<Order> {
    let row = repo
        .get(id)
        .map_err(AppError::internal)?;          // caller sees "internal error"; the cause is logged
    row.ok_or_else(|| {
        AppError::not_found("no such order")
            .with_reason("ORDER_NOT_FOUND")
            .with_domain("orders")
            .with_metadata("order_id", id)
    })
}

fn validate(quantity: u32) -> Result<()> {
    if quantity == 0 {
        return Err(AppError::invalid_argument("the request is invalid")
            .with_field_violation("items[0].quantity", "must be positive"));
    }
    Ok(())
}

let busy = AppError::unavailable("billing is busy").with_retry_after(Duration::from_secs(5));
assert_eq!(busy.code(), ErrorCode::Unavailable);
assert!(busy.is_transient());
```

`sekvent::error::Result<T, E = AppError>` is the alias used across sekvent.

## Concepts

- **Caller-visible vs internal.** Everything except the source chain may be
  shown to the caller: the message, reason, domain, metadata, retry hint and
  field violations. Never put credentials, key material, SQL, other
  tenants' ids or upstream response bodies there. Attach the cause with
  `with_source` (or use `AppError::internal`); it is logged by the server and
  never serialized.
- **One pointer wide.** `AppError` boxes its contents, so `Result<T,
  AppError>` stays small on the happy path.
- **Not `Clone`, not `PartialEq`.** Compare by `code()`, `reason()` and the
  other getters in tests.
- **Reason.** A stable, machine-readable `UPPER_SNAKE_CASE` string
  (`google.rpc.ErrorInfo.reason`) that clients branch on. The message is for
  humans and may change.
- **Domain.** Which service or API the reason belongs to, e.g. `orders`.

## Error codes

`ErrorCode` is exhaustive on purpose: the set and numbering are fixed by the
gRPC specification, so downstream code may `match` on it without a wildcard.

| Code (`as_str`) | Variant | gRPC | HTTP (`http_status`) | `is_transient` | `trips_breaker` | Logged at the serving boundary |
|---|---|---|---|---|---|---|
| `OK` | `Ok` | 0 | 200 | no | no | no |
| `CANCELLED` | `Cancelled` | 1 | 499 | no | no | no |
| `UNKNOWN` | `Unknown` | 2 | 500 | no | no | **yes** |
| `INVALID_ARGUMENT` | `InvalidArgument` | 3 | 400 | no | no | no |
| `DEADLINE_EXCEEDED` | `DeadlineExceeded` | 4 | 504 | **yes** | **yes** | no |
| `NOT_FOUND` | `NotFound` | 5 | 404 | no | no | no |
| `ALREADY_EXISTS` | `AlreadyExists` | 6 | 409 | no | no | no |
| `PERMISSION_DENIED` | `PermissionDenied` | 7 | 403 | no | no | no |
| `RESOURCE_EXHAUSTED` | `ResourceExhausted` | 8 | 429 | **yes** | **yes** | no |
| `FAILED_PRECONDITION` | `FailedPrecondition` | 9 | 400 | no | no | no |
| `ABORTED` | `Aborted` | 10 | 409 | **yes** | no | no |
| `OUT_OF_RANGE` | `OutOfRange` | 11 | 400 | no | no | no |
| `UNIMPLEMENTED` | `Unimplemented` | 12 | 501 | no | no | no |
| `INTERNAL` | `Internal` | 13 | 500 | no | no | **yes** |
| `UNAVAILABLE` | `Unavailable` | 14 | 503 | **yes** | **yes** | no |
| `DATA_LOSS` | `DataLoss` | 15 | 500 | no | no | **yes** |
| `UNAUTHENTICATED` | `Unauthenticated` | 16 | 401 | no | no | no |

`ErrorCode` methods:

| Method | Purpose |
|---|---|
| `ErrorCode::ALL: [ErrorCode; 17]` | Every code in numeric order. |
| `as_i32()` / `from_i32(i32)` | gRPC number; an unknown number maps to `Unknown`. |
| `as_str()` / `parse(&str) -> Option<ErrorCode>` | The `SCREAMING_SNAKE_CASE` name used by gRPC and the JSON body; `Display` prints it too. |
| `http_status() -> u16` | The status from `google/rpc/code.proto`. |
| `is_transient()` | Worth retrying at all: `UNAVAILABLE`, `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`, `ABORTED`. Whether a particular call may be retried also depends on the method being idempotent, which the caller decides. |
| `trips_breaker()` | Counts as a callee failure for a circuit breaker: `UNAVAILABLE`, `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`. The code cannot tell a slow callee from a caller whose own deadline ran out; never report your own expired deadline or cancellation to a breaker. |

## How to build an error

Constructors (all take `impl Into<String>` for the message):

| Constructor | Code |
|---|---|
| `AppError::new(code, message)` | any |
| `invalid_argument` | `INVALID_ARGUMENT` |
| `not_found` | `NOT_FOUND` |
| `already_exists` | `ALREADY_EXISTS` |
| `permission_denied` | `PERMISSION_DENIED` |
| `unauthenticated` | `UNAUTHENTICATED` |
| `failed_precondition` | `FAILED_PRECONDITION` |
| `unavailable` | `UNAVAILABLE` |
| `deadline_exceeded` | `DEADLINE_EXCEEDED` |
| `resource_exhausted` | `RESOURCE_EXHAUSTED` |
| `unimplemented` | `UNIMPLEMENTED` |
| `cancelled` | `CANCELLED` |
| `internal(source)` | `INTERNAL`, fixed message `internal error`; `source` is any `Into<Box<dyn Error + Send + Sync>>` (an error, a `String`, a `&str`) |

Builders (each `#[must_use]`, consuming `self`):

| Builder | Sets |
|---|---|
| `with_reason(reason)` | machine-readable reason, `UPPER_SNAKE_CASE` by convention |
| `with_domain(domain)` | the reason's domain |
| `with_metadata(key, value)` | one caller-visible key/value (sorted map; a repeated key replaces) |
| `with_retry_after(Duration)` | retry hint |
| `with_field_violation(field, description)` | appends a `FieldViolation { field, description }` |
| `with_source(source)` | the internal cause (replaces an earlier one) |

Getters: `code()`, `message()`, `reason()`, `domain()`, `metadata() ->
&BTreeMap<String, String>`, `retry_after()`, `field_violations() ->
&[FieldViolation]`, `is_transient()`. `std::error::Error::source()` returns
the internal cause. `Display` prints `CODE: message`; `Debug` prints every
field, the source included, so keep `Debug` output of an `AppError` out of
responses.

## How to answer over HTTP (`error-http`)

`AppError` implements axum's `IntoResponse`, so a handler returns
`Result<Json<T>, AppError>`:

- status from `ErrorCode::http_status`;
- `Content-Type: application/json`, body `{"error": WireError}`;
- `Retry-After` in whole seconds, rounded up, when a retry hint is set (the
  body keeps the exact milliseconds).

```json
{
  "error": {
    "code": "FAILED_PRECONDITION",
    "message": "the order is closed",
    "reason": "ORDER_CLOSED",
    "domain": "orders",
    "metadata": { "order_id": "ord-1" },
    "retry_after_ms": 1234,
    "field_violations": [
      { "field": "currency", "description": "unsupported" }
    ]
  }
}
```

Only `code` and `message` are always present; `reason`, `domain`,
`metadata`, `retry_after_ms` and `field_violations` are omitted when absent
or empty. A minimal body is `{"error":{"code":"NOT_FOUND","message":"gone"}}`.

Client side, `sekvent::error::http::from_json_body(status: u16, body: &[u8])
-> Option<AppError>` accepts only a body this module could have produced: a
known code other than `OK` whose `http_status()` equals `status`. Anything
else (not JSON, a flat shape, an unknown code, a code that disagrees with the
status) is `None`, so you map the response by its status instead of trusting
a foreign message. The [client](client.md) does this for you.

## How to answer over gRPC (`error-grpc`)

`From<AppError> for tonic::Status` encodes the code and message, and the
details as a `google.rpc.Status` in the standard `grpc-status-details-bin`
trailer:

| `AppError` part | gRPC detail |
|---|---|
| reason, domain, metadata | `ErrorInfo` (emitted when any of the three is present; an absent reason or domain travels as `""`) |
| retry hint | `RetryInfo` |
| field violations | `BadRequest` |

`From<tonic::Status> for AppError` decodes it; missing or malformed details
yield code and message only, an unknown code number becomes `UNKNOWN`, and an
empty reason or domain reads back as absent.

The `sekvent::error::grpc` module:

| Function | Purpose |
|---|---|
| `to_status(&AppError) -> Status` | Encode only (never logs). |
| `from_status(&Status) -> AppError` | Decode. |
| `with_legacy_detail(status, key: &'static str, &M) -> Status` | Add an extra binary trailer (`key` must be lowercase and end in `-bin`; panics otherwise) carrying any prost message, for clients that predate the standard details. |
| `legacy_detail::<M>(&Status, key) -> Option<M>` | Read it back; `None` when absent or undecodable. |

## How the serving boundary logs

`IntoResponse for AppError` and `From<AppError> for tonic::Status` are the
serving-boundary conversions. For a server-side failure (`UNKNOWN`,
`INTERNAL`, `DATA_LOSS`) they emit exactly one event before answering:

- level `error`, target `sekvent::error`, message `request failed`;
- fields `code`, `reason` (if any) and `source`: the source chain joined
  with `": "`, capped at 2 KiB on a character boundary and ending in `…`
  when cut.

The caller-visible message and the metadata are not logged (the message is
already in the response; metadata may carry identifiers). Caller errors (all
other codes) are not logged; the [access log](telemetry.md#how-to-log-every-request-access_log)
records their status. The component gRPC binding logs the same way.

The plain encoders, `grpc::to_status` and `AppError::to_wire`, never log. A
boundary that answers with them directly calls
`sekvent::error::log_server_side(&error)` itself, exactly once per answer
(the function is `#[doc(hidden)]` and exists for such custom boundaries).

Do not log an `AppError` yourself before returning it from a handler; it
would be logged twice.

## How to persist or forward an error (`WireError`)

`AppError::to_wire() -> WireError` detaches the caller-visible part;
`AppError::from_wire(WireError) -> AppError` rebuilds it (an unknown code
name becomes `UNKNOWN`; there is no source). `WireError` is the same shape as
the HTTP body's `error` object, with public fields `code: String`, `message`,
`reason`, `domain`, `metadata: BTreeMap<String, String>`, `retry_after_ms:
Option<u64>` and `field_violations: Vec<FieldViolation>`. It derives
`Serialize`/`Deserialize` when the crate's `serde` feature is on (the facade
turns it on with `error-http`), which makes it suitable for dead-letter rows
and job failure records.

## Typed component errors (`ComponentError`)

Component methods may return a typed enum instead of `AppError`.
`#[derive(sekvent::ComponentError)]` (facade feature `component`) maps each
variant to a fixed code and reason, carries its named fields as metadata, and
decodes a received `AppError` back into the variant, falling back to the
`#[other]` variant for anything it does not recognise:

```rust
use sekvent::component::{AppError, ComponentError};

#[derive(Debug, ComponentError)]
#[component_error(domain = "orders.v1")]
pub enum OrdersError {
    #[reason("ORDER_NOT_FOUND", code = NotFound, message = "order {order_id} not found")]
    OrderNotFound { order_id: String },
    #[reason("ORDERS_CLOSED", code = Unavailable)]   // message defaults to "orders closed"
    Closed,
    #[other]
    Other(AppError),
}
```

Every binding delivers the same value because the error always travels as
`AppError`. See [components](components.md) for the full rules.

## Reason constants

`sekvent-error` itself sets no reasons. Reasons produced by other sekvent
crates are listed on their module pages. Three modules export them as
constants: [components](components.md) (`sekvent::component::reasons`, for
example `METHOD_TIMEOUT`, `DOWNSTREAM_FAILURE` and `HANDLER_PANICKED`),
[jobs](jobs.md) (`sekvent::runtime::reasons`) and [db](db.md)
(`sekvent::db::reasons`). [Auth](auth.md), [resilience](resilience.md) and
[client](client.md) set reasons as plain string values without constants.

## Testing tips

- Assert on `code()`, `reason()`, `metadata()`, `field_violations()` and
  `retry_after()`; `AppError` has no `PartialEq`.
- Round-trip through the wire to check what a caller actually sees:
  `AppError::from_wire(error.to_wire())`, `grpc::from_status(&grpc::to_status(&e))`,
  or `http::from_json_body(status, &body)` on an `into_response()` body.
- To check what the boundary logs, capture events with target
  `sekvent::error` (for example with a `LogBuffer` layer from
  [telemetry](telemetry.md)) and assert that the message and metadata are
  absent.

## Pitfalls and security rules

- The message, reason, domain and metadata reach the caller verbatim. Keep
  diagnostic detail out of them and attach it with `with_source` instead.
- The source chain is not shown to the caller, but for `UNKNOWN`,
  `INTERNAL` and `DATA_LOSS` the serving boundary logs it verbatim (capped
  at 2 KiB, which is a length limit, not redaction). Sources must therefore
  be sanitized: never put credentials, tokens, database URLs or raw
  upstream bodies into an error you attach with `with_source`.
- `AppError::internal(err)` hides `err` from the caller; `AppError::new(
  ErrorCode::Internal, err.to_string())` does not. Prefer the former.
- Choose codes for the caller's next step: `INVALID_ARGUMENT` /
  `FAILED_PRECONDITION` are not retried, `UNAVAILABLE` is. Returning
  `UNAVAILABLE` for a bad request makes clients retry it.
- A retry hint is a hint; whether to retry also needs an idempotent method.
- An empty reason or domain is indistinguishable from an absent one on gRPC.

## See also

- [Server](server.md): handlers, the access log and the boundary in the combined server
- [Client](client.md): decoding HTTP error bodies
- [Resilience](resilience.md): retries, breakers and `is_transient` / `trips_breaker`
- [Components](components.md) and [the component model](../component-model.md)
- [Telemetry](telemetry.md)
