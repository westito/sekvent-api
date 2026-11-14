# Call context (`sekvent::context`)

`sekvent::context` carries what one logical call needs to know through every
handler, client and queued job: the request id, the deadline, cancellation,
the authenticated caller (a service or an end user), the end-user subject
and tenant, the client address, the idempotency key, the W3C `traceparent`
and the component hop count. It also defines how
that context travels as HTTP/gRPC headers (with separate rules for sekvent
peers and third-party APIs) and the injectable `Clock` that time-dependent
code should use instead of `SystemTime::now()`.

## Enable it

| | |
|---|---|
| Facade feature | `context` (on by default) |
| Module | `use sekvent::context::{CallContext, ClientAddr, ServiceIdentity, EndUser, RequestScope, Clock, SystemClock, ManualClock};` |
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
| caller | `caller() -> Option<&ServiceIdentity>` | Which service is calling, as established by authentication. |
| end user | `end_user() -> Option<&EndUser>` | The authenticated end user, when an end user made this call directly (a component served to end users). Never set together with `caller`. |
| subject / tenant | `subject()`, `tenant()` | End-user identity, accepted only from a trusted link or taken from the authenticated end user. |
| idempotency key | `idempotency_key()`, `outbound_idempotency_key()` | Names one operation; forwarded only when set for this call. |
| traceparent | `traceparent()` | W3C trace context. |
| hops | `hops() -> u32` | How many component calls led to this one (0 at the edge); a guard against call cycles. |
| client address | `client_addr() -> Option<IpAddr>` | The address of the client the call is made for: the edge server resolves it (TCP peer, or the client behind trusted proxies), and a trusted link may forward its own visitor's. Personal data: for limits and abuse checks, never logged. |
| request scope | `scope() -> Option<&RequestScope>`, `scoped::<T>() -> Option<&T>` | What a serving boundary resolved from the request head before it authenticated the caller: the tenant and typed values such as a tenant's database. Never encoded in headers, never carried by `child()` or `detached()`; see [request scope](#request-scope). |

`ServiceIdentity { name: String, trusted: bool }` is the authenticated
caller. Build it with `ServiceIdentity::trusted("billing")` (may assert
subject and tenant on a user's behalf) or `ServiceIdentity::untrusted("web")`.
The [link](link.md) layer produces it from a service token.

`EndUser` is an end user authenticated at a serving boundary: a component
served with `SERVE_AUTH=bearer` or `link,bearer` builds it with the App's
end-user authenticator ([components](components.md#how-to-serve-components-to-end-users-browsers-apps)).

```rust
use sekvent::context::EndUser;

let user = EndUser::new("user-7")
    .with_tenant("tenant-a")
    .with_roles(["admin", "billing"]);
assert_eq!(user.subject(), "user-7");
assert_eq!(user.tenant(), Some("tenant-a"));
assert!(user.has_role("admin"));
let roles: &[String] = user.roles();
```

The direct caller is either a service (`caller()`) or an end user
(`end_user()`), never both. Roles travel with the end user only: they are
never encoded in headers and never reach the components a handler calls.

An authenticator can attach whatever else it learned and handlers need as
typed **extensions**, one value per type (the scope of an API token, a
session id):

```rust
use sekvent::context::EndUser;

#[derive(Clone)]
struct TokenScope { permissions: Vec<String> }   // a type of your own, not String

let user = EndUser::new("agent-3")
    .with_roles(["developer"])
    .with_extension(TokenScope { permissions: vec!["issues.edit".into()] });

// in the handler
let scope: Option<&TokenScope> = cx.end_user().and_then(EndUser::extension::<TokenScope>);
```

- `with_extension::<T: Clone + Send + Sync + 'static>(value)` replaces an
  earlier value of the same type; `extension::<T>()` is `None` when no value
  of `T` was attached.
- Extensions follow the roles' rule: they stay with the end user in this
  process, are never encoded in headers and never reach the components a
  handler calls (`child()` and `detached()` drop the end user).
- `Debug` prints only their number (`extensions: 2`), never values or type
  names.
- Equality (`PartialEq`, `Eq`) compares subject, tenant and roles and
  ignores extensions.

### Request scope

A `RequestScope` is what a serving boundary resolved for one request before
it authenticated the caller: the tenant the request belongs to (from the
host, say) and typed values its handlers need (that tenant's database). The
App's `ScopeResolver` builds it ([components](components.md#how-to-resolve-a-request-scope));
`cx.with_scope(scope)` attaches it.

```rust
use sekvent::context::RequestScope;

#[derive(Clone)]
struct TenantDb(String);                         // a type of your own, not String

let scope = RequestScope::new()
    .with_tenant("tenant-a")
    .with_extension(TenantDb("pool-a".into()));
let cx = CallContext::new().with_scope(scope);

assert_eq!(cx.tenant(), Some("tenant-a"));       // a tenant the scope names becomes the context's
assert!(cx.scoped::<TenantDb>().is_some());      // the scope's extension of that type
let scope: Option<&RequestScope> = cx.scope();
```

- `with_tenant(t)` names the tenant. It must pass
  `headers::is_valid_tenant` (non-empty visible ASCII without spaces, at most
  255 bytes: the rule an inbound tenant header is held to), or the header
  codec loses it and a downstream call runs without a tenant; a serving
  boundary therefore fails a call whose resolved scope names an invalid
  tenant with `INTERNAL` / `SCOPE_TENANT_INVALID`. `with_extension::<T: Clone + Send +
  Sync + 'static>(value)` replaces an earlier value of the same type;
  `tenant()` and `extension::<T>()` read them back (`extension` is `None`
  when no value of `T` was attached).
- **Storage.** A scope holds an optional tenant and typed extensions, one
  value per type, behind a private `Arc`: cloning a scope or a context
  shares them and never clones a value. A value in a scope lives as long as
  some clone of the scope does, so a `KeyedRef` kept there keeps its entry in
  use for that long.
- `with_scope` replaces an earlier scope. A tenant it names becomes the
  context's tenant; a scope without a tenant leaves the tenant as it is.
  After it, `with_end_user` takes the tenant from the scope when the scope
  names one, otherwise from the user. Neither makes a check: `with_scope`
  does not look at an end user the context already has, and
  `with_end_user` does not compare the user's tenant with the scope's.
  The serving boundaries do: when the scope names a tenant, a user who does
  not name the same one (another tenant, or none) is refused
  (`sekvent::component::admit_end_user`, and
  `sekvent::component::App::resolve_scope` for a context that already has
  an end user).
- `sanitize_for_caller()` keeps a tenant the scope names (it came from the
  serving boundary, not from the caller) and drops one a header carried.
- `Debug` prints the tenant and the number of extensions
  (`extensions: 2`), never a value or a type name.
- **What `child()` and `detached()` keep.** Neither keeps the scope. A
  callee and a queued job get the tenant, which travels like any tenant, and
  nothing else of the scope. Work that outlives the call (a spawned task, a
  job queued with `detached()`) must not carry a value it cloned out of the
  scope, such as a pool taken from a `KeyedRef`: the reference that kept the
  value alive is released with the call, and the value may be closed under the
  work. Such work takes its own reference (`pools.get(tenant)`) and holds it
  for as long as it runs.
- **The tenant travels, the extensions do not.** `headers::inject` and
  `headers::propagate` write the context's tenant as they always did, so a
  tenant the scope set reaches peers that trust this service's link exactly
  like an end user's; nothing ever writes an extension, and
  `from_headers` never produces a scope.
## How to build and derive contexts

Builders (all `#[must_use]`, consuming `self`):

| Builder | Effect |
|---|---|
| `CallContext::new()` / `Default` | Fresh request id, no deadline, own cancellation token, hops 0. |
| `with_request_id(id)` | Replace the request id. |
| `with_deadline(Instant)` | Set the deadline; a later one than the current is ignored. |
| `with_timeout(Duration)` | Deadline relative to now (same narrowing rule); a timeout too large to represent sets nothing. |
| `with_cancel(CancellationToken)` | Use this token. |
| `with_caller(ServiceIdentity)` | Set the authenticated caller; clears the end user. |
| `with_end_user(EndUser)` | The end user is the direct caller: sets the subject from it and clears the caller. The tenant is the scope's when the context has a scope that names one, otherwise the user's. |
| `with_subject(s)` / `with_tenant(t)` | Set end-user identity. |
| `with_idempotency_key(k)` | Set this call's own key (it will be forwarded). |
| `with_traceparent(tp)` | Set the trace context. |
| `with_hops(n)` | Replace the hop count. |
| `with_client_addr(IpAddr)` | Set the client address. |
| `with_scope(RequestScope)` | Attach the [request scope](#request-scope); a tenant it names becomes the context's tenant. A plain setter: it does not check the scope's tenant against an end user already set. |

Derivations:

| Method | Keeps | Drops / changes |
|---|---|---|
| `child()` | request id, deadline, subject, tenant, client address, traceparent, hops, an own idempotency key | caller, end user and request scope cleared (the callee learns its caller from authentication); cancellation becomes a **child token** (cancelling the parent cancels the child, not the reverse); a received idempotency key is not kept |
| `detached()` | request id, subject, tenant, client address, traceparent, hops, idempotency key (in the same received/own state) | no deadline, a fresh cancellation token, no caller, no end user, no request scope |
| `into_inbound()` | everything | marks a present idempotency key as *received*: still readable, no longer forwarded |
| `sanitize_for_caller()` | everything else | drops subject, tenant and the client address unless the caller is a trusted `ServiceIdentity` or an end user is set; a tenant the request scope names is kept |

Because `child()` keeps subject and tenant but drops the end user, a
component that a handler calls on an end user's behalf sees them exactly as
a trusted link would assert them, without the end user's roles.

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
| `CLIENT_ADDR` | `x-sekvent-client-addr` | one IPv4 or IPv6 address, no port or brackets; kept **only for a trusted caller** |

Anything that fails validation is dropped (or, for the request id,
replaced), never rejected, so a sloppy client is still served.

The [request scope](#request-scope) is never encoded: only the tenant it set
(an ordinary context field) is written, and `from_headers` never builds a
scope.

Functions:

| Function | Direction | Use it for |
|---|---|---|
| `from_headers(&HeaderMap, caller: Option<ServiceIdentity>) -> CallContext` | inbound | Building the server-side context. Applies the validation above, sets the caller, runs `sanitize_for_caller()` and `into_inbound()`. |
| `inject(&CallContext, &mut HeaderMap)` | outbound, peer | Re-encoding the whole context for the callee of a component call. **Replaces** existing values and **removes** a header for a field the context does not carry (hops 0 counts as absent), so a reused header map never leaks a previous call's values. Writes only an own idempotency key. An expired deadline is sent as `1n`. |
| `propagate(&CallContext, &mut HeaderMap)` | outbound, peer | A request to another sekvent service. Writes request id, `traceparent`, subject, tenant, the client address and hops (> 0) only where **the caller left the header unset**; never removes; never writes the idempotency key. `grpc-timeout` is the one exception: a caller value longer than the time left (or unparsable) is replaced by the remaining time. |
| `propagate_external(&CallContext, &mut HeaderMap)` | outbound, third party | A third-party API. Only request id, `traceparent` and `grpc-timeout`, under the same rules as `propagate`. Never subject, tenant, the client address, hops or the idempotency key. |
| `encode_grpc_timeout(Duration) -> String` | | At most 8 digits in the finest unit that fits, truncated (never rounded up); zero becomes `1n`; capped at `99999999H`. |
| `parse_grpc_timeout(&str) -> Option<Duration>` | | The inverse; rejects signs, decimals, spaces, lowercase `s` and more than 8 digits. |
| `is_valid_tenant(&str) -> bool` | | Whether a tenant passes the `TENANT` rule above (1–255 visible ASCII). A tenant that fails it does not reach a callee: either it cannot be written or the callee's `from_headers` drops it. |

```rust
use http::HeaderMap;
use sekvent::context::headers;

let mut outbound = HeaderMap::new();
headers::propagate_external(&ctx, &mut outbound); // to a payment provider: id, trace, timeout only
```

The [client](client.md) applies `propagate` or `propagate_external` for you,
and the component gRPC binding uses `inject` / `from_headers`.

The client address follows the subject's trust rule. The [server](server.md#resolve-the-client-address)
sets it on every inbound context from the connection (and also puts the
transport's address into the request extensions as `ClientAddr`); a
trusted link's `x-sekvent-client-addr` replaces it in the context, so a
server-rendered frontend that calls with its link token can name the
visitor it renders for:

```rust
// in the frontend, per page view
let cx = CallContext::new().with_client_addr(visitor_ip);
catalog.quote(&cx, request).await?; // the catalog's per-client limit counts the visitor
```

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
- Unit-test a component handler as an end user would call it with
  `CallContext::new().with_end_user(EndUser::new("user-7").with_roles(["admin"]))`.

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
- Never build an `EndUser` from unverified request data. Let the App's
  end-user authenticator check a credential and the component serving
  boundary call `with_end_user`.
- `CallContext` derives `Debug`, which includes subject, tenant, the client
  address and the end user (its extensions as a count only); avoid logging
  the whole context where those are sensitive. The access log never records
  the client address.
- Never set `with_client_addr` from a request header yourself: the server
  resolves it, and trusts forwarding headers only from the proxies you
  configure (`ClientAddrPolicy`).

## See also

- [Server](server.md): where the inbound context is built and how handlers get it
- [Client](client.md): outbound propagation
- [Link](link.md): service identities and trusted links
- [Components](components.md) and [the component model](../component-model.md): hops, timeouts and idempotency across component calls, and serving components to end users
- [Auth](auth.md): `BearerAuth::end_user` and role checks on an `EndUser`
- [Jobs](jobs.md): detached contexts for background work
- [Telemetry](telemetry.md): the `x-request-id` layer
