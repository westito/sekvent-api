# Call context (`sekvent::context`)

`sekvent::context` carries what one logical call needs to know through every
handler, client and queued job: the request id, the deadline, cancellation,
the authenticated caller, the end-user subject and tenant, the idempotency
key, the W3C `traceparent` and the component hop count. It also defines how
that context travels as HTTP/gRPC headers (with separate rules for sekvent
peers and third-party APIs) and the injectable `Clock` that time-dependent
code should use instead of `SystemTime::now()`.

## Enable it

| | |
|---|---|
| Facade feature | `context` (on by default) |
| Module | `use sekvent::context::{CallContext, ServiceIdentity, Clock, SystemClock, ManualClock};` |
| Header codec | `sekvent::context::headers` |
| Prelude | `CallContext` |
| Internal crate | `sekvent-context` (no features; depends on `http`, `tokio`, `tokio-util`, `uuid`) |

## Quick example

```rust
use std::time::Duration;

use sekvent::context::{CallContext, ServiceIdentity};

// At the edge: a fresh root context (UUID v7 request id) with a 5 s budget.
let ctx = CallContext::new()
    .with_timeout(Duration::from_secs(5))
    .with_caller(ServiceIdentity::trusted("billing"))
    .with_subject("user-7")
    .with_tenant("tenant-a");

// For an outbound call: same id, deadline, subject and tenant;
// a child cancellation token; no caller.
let outbound = ctx.child();

// For work that outlives the call (a queued job): no deadline, no cancellation.
let queued = ctx.detached();

if let Some(left) = ctx.remaining() {
    tracing::debug!(?left, "time left");
}
```

In a service you rarely build the root context yourself: the
[server](server.md) decodes it from each request's headers (axum handlers
take the `Ctx` extractor, tonic handlers read `CallContext` from the request
extensions), and [component](components.md) handles pass it along.

## Concepts

| Field | Getter | Meaning |
|---|---|---|
| request id | `request_id() -> &str` | Correlates logs across services. A new context gets a UUID v7. |
| deadline | `deadline() -> Option<Instant>`, `remaining()`, `is_expired()` | Absolute in process; relative `grpc-timeout` on the wire. Only ever narrows. |
| cancellation | `cancel_token() -> &CancellationToken`, `cancelled().await` | Fires when the token, or the parent it was derived from, is cancelled; see [what cancels a context](#what-cancels-a-context). |
| caller | `caller() -> Option<&ServiceIdentity>` | Who is calling, as established by authentication. |
| subject / tenant | `subject()`, `tenant()` | End-user identity, accepted only from a trusted link. |
| idempotency key | `idempotency_key()`, `outbound_idempotency_key()` | Names one operation; forwarded only when set for this call. |
| traceparent | `traceparent()` | W3C trace context. |
| hops | `hops() -> u32` | How many component calls led to this one (0 at the edge); a guard against call cycles. |

`ServiceIdentity { name: String, trusted: bool }` is the authenticated
caller. Build it with `ServiceIdentity::trusted("billing")` (may assert
subject and tenant on a user's behalf) or `ServiceIdentity::untrusted("web")`.
The [link](link.md) layer produces it from a service token.

## How to build and derive contexts

Builders (all `#[must_use]`, consuming `self`):

| Builder | Effect |
|---|---|
| `CallContext::new()` / `Default` | Fresh request id, no deadline, own cancellation token, hops 0. |
| `with_request_id(id)` | Replace the request id. |
| `with_deadline(Instant)` | Set the deadline; a later one than the current is ignored. |
| `with_timeout(Duration)` | Deadline relative to now (same narrowing rule); a timeout too large to represent sets nothing. |
| `with_cancel(CancellationToken)` | Use this token. |
| `with_caller(ServiceIdentity)` | Set the authenticated caller. |
| `with_subject(s)` / `with_tenant(t)` | Set end-user identity. |
| `with_idempotency_key(k)` | Set this call's own key (it will be forwarded). |
| `with_traceparent(tp)` | Set the trace context. |
| `with_hops(n)` | Replace the hop count. |

Derivations:

| Method | Keeps | Drops / changes |
|---|---|---|
| `child()` | request id, deadline, subject, tenant, traceparent, hops, an own idempotency key | caller cleared (the callee learns it from authentication); cancellation becomes a **child token** (cancelling the parent cancels the child, not the reverse); a received idempotency key is not kept |
| `detached()` | request id, subject, tenant, traceparent, hops, idempotency key (in the same received/own state) | no deadline, a fresh cancellation token, no caller |
| `into_inbound()` | everything | marks a present idempotency key as *received*: still readable, no longer forwarded |
| `sanitize_for_caller()` | everything else | drops subject and tenant unless the caller is a trusted `ServiceIdentity` |

Use `detached()` for anything queued beyond the call's lifetime (a job, an
outbox row, a spawned task that must finish after the response): it must not
inherit the caller's deadline or be cancelled with the caller.

## What cancels a context

A context's token is a plain `CancellationToken`. It fires only when
something cancels it:

- your code calls `ctx.cancel_token().cancel()`, or cancels the token you
  passed to `with_cancel`;
- the context came from `child()` and the parent's token is cancelled;
- a component binding cancels it. Serving a component over gRPC, the call's
  token is cancelled when the client resets the stream (the serving future
  is dropped) or the call's deadline passes. Over the local and serialized
  bindings, the callee's context shares the caller's child token, so the
  callee sees the caller cancel.

What does **not** cancel a context: the combined [server](server.md) builds
each inbound context with `headers::from_headers`, which gives it a fresh
token that nothing else holds. For ordinary axum and tonic handlers, a
client disconnect does not fire it (hyper may drop the handler future, but
`cancelled()` never resolves for tasks you spawned with the context), and
neither does server shutdown, which stops accepting and lets in-flight
requests drain. Outside component gRPC serving, a deadline passing does
not fire the token either: the deadline and the token are independent, so
wait on both (next section). To
tie handler work to shutdown, pass a token you control with `with_cancel`.

## How to honour deadlines and cancellation

`cancelled()` does not wake when the deadline passes, so select on a
deadline timer as well:

```rust
use sekvent::context::CallContext;
use sekvent::error::AppError;

async fn reserve(ctx: &CallContext) -> Result<(), AppError> {
    let deadline = async {
        match ctx.deadline() {
            Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = ctx.cancelled() => Err(AppError::cancelled("the caller went away")),
        () = deadline => Err(AppError::deadline_exceeded("no time left")),
        result = do_reserve() => result,
    }
}
```

`biased;` checks cancellation and an already-passed deadline before
`do_reserve()` is first polled. With the `resilience` feature (not on by
default), `sekvent::resilience::Timeout::deadline_only().call(ctx, fut)`
does the deadline half for you: it runs `fut` within the time left and
returns `DEADLINE_EXCEEDED` when it runs out (it does not watch the token,
so keep the `cancelled()` branch):

```rust
use sekvent::resilience::Timeout;

tokio::select! {
    biased;
    () = ctx.cancelled() => Err(AppError::cancelled("the caller went away")),
    result = Timeout::deadline_only().call(ctx, do_reserve()) => result,
}
```

- `remaining()` is `None` without a deadline and `Duration::ZERO` once it
  has passed; `is_expired()` is `remaining() == Some(ZERO)`.
- A callee never gets more time than its caller: `with_deadline` /
  `with_timeout` only narrow, and the header codec only narrows
  `grpc-timeout`.
- The deadline is a `std::time::Instant`. `with_timeout`, `remaining()` and
  `is_expired()` read `std::time::Instant::now()`, the OS monotonic clock,
  so they do **not** follow tokio's paused or advanced test clock. Code
  that converts the deadline to a tokio instant does: the component
  bindings' deadline enforcement, `tokio::time::sleep_until` as above, and
  `sekvent::resilience::remaining(ctx)` / `Timeout`. The injectable `Clock`
  below is for wall-clock time and has nothing to do with deadlines.

## How idempotency keys flow

A key identifies **one** operation to **one** callee. Handing an inbound key
to a different operation would make that callee treat distinct requests as
duplicates, so:

- A key set for this call (`with_idempotency_key`) is forwarded by `child()`
  and by `headers::inject`.
- A key received with the call (`headers::from_headers` returns contexts in
  this state, or `into_inbound()`) stays readable through
  `idempotency_key()` but `outbound_idempotency_key()` is `None` and it is
  not forwarded.
- `headers::propagate` and `headers::propagate_external` never write the
  key; set it on each outbound request that needs one.

```rust
let inbound = sekvent::context::headers::from_headers(&headers, None);
assert_eq!(inbound.idempotency_key(), Some("upstream-key"));
assert_eq!(inbound.outbound_idempotency_key(), None);

let own = inbound.child().with_idempotency_key("reserve-ord-1");
assert_eq!(own.outbound_idempotency_key(), Some("reserve-ord-1"));
```

## How the context travels as headers

Header names (constants in `sekvent::context::headers`):

| Constant | Header | Inbound validation |
|---|---|---|
| `REQUEST_ID` | `x-request-id` | 1–128 visible ASCII characters, no spaces; otherwise replaced by a fresh id |
| `GRPC_TIMEOUT` | `grpc-timeout` | 1–8 digits plus one unit `H M S m u n`; otherwise ignored |
| `TRACEPARENT` | `traceparent` | W3C version `00`, lowercase hex, non-zero trace and parent ids; otherwise dropped |
| `SUBJECT` | `x-sekvent-subject` | 1–255 visible ASCII; kept **only for a trusted caller** |
| `TENANT` | `x-sekvent-tenant` | 1–255 visible ASCII; kept **only for a trusted caller** |
| `IDEMPOTENCY_KEY` | `idempotency-key` | 1–255 visible ASCII; otherwise dropped |
| `HOPS` | `x-sekvent-hops` | 1–4 ASCII digits from any caller; otherwise 0 |

Anything that fails validation is dropped (or, for the request id,
replaced), never rejected, so a sloppy client is still served.

Functions:

| Function | Direction | Use it for |
|---|---|---|
| `from_headers(&HeaderMap, caller: Option<ServiceIdentity>) -> CallContext` | inbound | Building the server-side context. Applies the validation above, sets the caller, runs `sanitize_for_caller()` and `into_inbound()`. |
| `inject(&CallContext, &mut HeaderMap)` | outbound, peer | Re-encoding the whole context for the callee of a component call. **Replaces** existing values and **removes** a header for a field the context does not carry (hops 0 counts as absent), so a reused header map never leaks a previous call's values. Writes only an own idempotency key. An expired deadline is sent as `1n`. |
| `propagate(&CallContext, &mut HeaderMap)` | outbound, peer | A request to another sekvent service. Writes request id, `traceparent`, subject, tenant and hops (> 0) only where **the caller left the header unset**; never removes; never writes the idempotency key. `grpc-timeout` is the one exception: a caller value longer than the time left (or unparsable) is replaced by the remaining time. |
| `propagate_external(&CallContext, &mut HeaderMap)` | outbound, third party | A third-party API. Only request id, `traceparent` and `grpc-timeout`, under the same rules as `propagate`. Never subject, tenant, hops or the idempotency key. |
| `encode_grpc_timeout(Duration) -> String` | | At most 8 digits in the finest unit that fits, truncated (never rounded up); zero becomes `1n`; capped at `99999999H`. |
| `parse_grpc_timeout(&str) -> Option<Duration>` | | The inverse; rejects signs, decimals, spaces, lowercase `s` and more than 8 digits. |

```rust
use http::HeaderMap;
use sekvent::context::headers;

let mut outbound = HeaderMap::new();
headers::propagate_external(&ctx, &mut outbound); // to a payment provider: id, trace, timeout only
```

The [client](client.md) applies `propagate` or `propagate_external` for you,
and the component gRPC binding uses `inject` / `from_headers`.

## How to inject time (`Clock`)

```rust
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sekvent::context::{Clock, ManualClock, SystemClock};

struct InvoiceService {
    clock: Arc<dyn Clock>,
}

impl InvoiceService {
    fn is_overdue(&self, due_unix_ms: u64) -> bool {
        self.clock.now_unix_millis() > due_unix_ms
    }
}

let production = InvoiceService { clock: Arc::new(SystemClock) };

let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
let service = InvoiceService { clock: Arc::new(clock.clone()) };
assert!(!service.is_overdue(1_700_000_000_500));
clock.advance(Duration::from_secs(1));
assert!(service.is_overdue(1_700_000_000_500));
```

| Item | Purpose |
|---|---|
| `trait Clock: Send + Sync + 'static` | `now() -> SystemTime`; provided `now_unix_millis() -> u64` (0 before the epoch, saturating at `u64::MAX`). |
| `SystemClock` | The real wall clock (`Copy`, `Default`). |
| `ManualClock::new(start)` | Frozen at `start`; `advance(Duration)` and `set(SystemTime)` move it. Clones share the same time. Survives a poisoned lock. |

## Configuration keys

None. The trust decision that lets a caller assert subject and tenant comes
from the [link](link.md) configuration (`SEKVENT_LINK_TRUSTED`).

## Errors

The crate returns no errors: inbound headers are validated and dropped, not
rejected. Handlers map an expired deadline or cancellation to
`DEADLINE_EXCEEDED` / `CANCELLED` themselves ([error](error.md)).

## Testing tips

- Inject `ManualClock` wherever logic depends on wall-clock time (token
  expiry, TTLs, due dates) and move it with `advance` / `set`.
- For deadline logic, run under `#[tokio::test(start_paused = true)]` (or
  `tokio::time::pause()`) and make the code under test wait on the deadline
  through tokio (`sleep_until(Instant::from_std(..))`, `Timeout`,
  `sekvent::resilience::remaining`). Anchor the deadline on tokio's clock,
  `ctx.with_deadline(tokio::time::Instant::now().into_std() + budget)`, not
  with `with_timeout`, which reads the OS clock; then `tokio::time::advance`
  moves the test past it deterministically.
- `remaining()` and `is_expired()` read the OS clock, so a paused tokio
  clock does not move them: assert on them only with a deadline already in
  the past (exactly `ZERO` / `true`) or with lower and upper bounds, never
  after an `advance`.
- Build inbound contexts in tests with `headers::from_headers` on a
  hand-made `HeaderMap`, passing `Some(ServiceIdentity::trusted(..))` or
  `None` to exercise the trust rule.
- Cancel with `ctx.cancel_token().cancel()` and await `ctx.cancelled()`
  instead of sleeping.

## Pitfalls and security rules

- Never trust `x-sekvent-subject` / `x-sekvent-tenant` yourself: only
  `from_headers` with a trusted `ServiceIdentity` keeps them. Building a
  context by hand from request headers bypasses that rule.
- Use `propagate_external` (or the client's external mode) for third-party
  APIs; `propagate` and `inject` would leak subject, tenant and hop count.
- Queued work must use `detached()`; a child context would be cancelled or
  time out with the request that enqueued it.
- Do not forward an inbound idempotency key; set a fresh one per outbound
  operation.
- `CallContext` derives `Debug`, which includes subject and tenant; avoid
  logging the whole context where those are sensitive.

## See also

- [Server](server.md): where the inbound context is built and how handlers get it
- [Client](client.md): outbound propagation
- [Link](link.md): service identities and trusted links
- [Components](components.md) and [the component model](../component-model.md): hops, timeouts and idempotency across component calls
- [Jobs](jobs.md): detached contexts for background work
- [Telemetry](telemetry.md): the `x-request-id` layer
