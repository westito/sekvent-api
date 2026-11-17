# Admission before scope resolution

What a call served over gRPC or gRPC-Web may cost before anybody vouches
for its caller: a cheap check of the end-user credential on the head, a
serve admission shared by every component an App serves and capped per
client address, and the request read whole, bounded in size and time,
before the App's scope resolver runs.

**Serve admission** is this note's bound, in front of the scope resolver.
It is not the **gate admission** of C1 (a component's gate: draining,
dead-call shedding, the bulkhead), which a call reaches after the serve
admission has been given back.

> Status: implemented. Builds on
> [`component-end-user.md`](component-end-user.md),
> [`multi-tenant-building-blocks.md`](multi-tenant-building-blocks.md),
> [`client-limits.md`](client-limits.md) and
> [`call-authorizer.md`](call-authorizer.md). Section 4 is the serving
> order those notes refer to; where this note is more specific, it wins. The user guide is [modules/components.md](../modules/components.md)
> ("How to bound what a call costs before it is authenticated").

## 1. Problem

The scope resolver is a hook of the application, and it may do expensive
work: open a database pool for the request's site and hold a lease on it
for the call. Until now the component service ran it right after routing
and the address-keyed client limit, on the request head, before the body
was read and before the end-user authenticator. Three consequences:

1. **No credential, full price.** A call to a method that needs an end
   user, sent without any `Authorization` header (or with one that can
   never be a credential), still ran the resolver before the
   authenticator refused it.
2. **A slow body holds the lease.** The body was read by tonic after the
   resolver, with no time limit of its own. A client that trickles its
   body, or never finishes it, kept whatever the resolver acquired for as
   long as it liked (bounded only by a deadline the client chose, or the
   method's `TIMEOUT`).
3. **No bound across methods or per client.** Only the per-address rate
   limits of selected methods (`CLIENT_*`) stood in front of the resolver.
   Nothing bounded how many calls could be in that state at once across
   every public method of a listener, or how many of them one client
   address could hold. A rate limit bounds how often calls start, not how
   many slow ones are in flight at once.

## 2. Decisions

1. **Read the request first.** The request body of every unary call, and
   the request message of every server-streaming call, is read whole
   before the scope resolver runs, then handed to tonic as one buffered
   frame. gRPC and gRPC-Web framing, decoding and errors stay tonic's.
   Request trailers are dropped; they never reached the context anyway.
2. **Bound the read.** In size: at most one frame of the largest message,
   the 5-byte frame header plus `max_message_bytes` (default and upper
   bound 4 MiB, the cap the framework always had), counted on the body the
   service receives (for gRPC-Web the bytes after the server's
   translation). The cap applies to the message as it is on the wire, so a
   compressed payload counts with its compressed size; invalid or
   unsupported compression, several frames and trailing data are still
   answered by tonic when it decodes the buffered body. A body over the
   cap is refused as soon as its 5-byte frame header declares an oversized
   message (including a truncated or compressed message), or the bytes read
   exceed it. One buffer is reserved from the declared length. Wire size
   hints are ignored because gRPC-Web text retains its base64 wire hint
   while yielding decoded bytes. In time: `read_timeout` (default 10 s, at most `MAX_READ_TIMEOUT`,
   5 min) from the start of the read, for every caller, link callers
   included, together with the call's own deadline (the caller's, the
   method's `TIMEOUT`, for a stream its open bound); whichever comes first
   answers.
3. **One serve admission per App, capped per client.** A semaphore of
   `max_pending` slots (default 512) shared by every component the App
   serves, so one listener mounting `app.grpc_routes()` has one admission
   across all its public methods, and a cap of `max_pending_per_client`
   simultaneous calls (default 32) per client address. The address is
   `cx.client_addr()`, keyed as the address-keyed client limit (canonical
   IPv4 or IPv6 /64); every call whose address is unknown shares one bucket. A client's entry
   exists only while it holds a slot and is removed when its count drops to
   zero, so the map never grows with addresses that went away. Every call
   that no link token authenticated takes both a per-client slot and a
   shared slot right after the head-only checks, without waiting: when
   either is not free it is refused at once. Link callers take neither;
   they are peers whose own capacity is not this App's to bound, and
   end-user load must not crowd them out.
4. **Held until the gate.** The slot covers the request read, the scope
   resolver, the end-user authenticator, the end-user-keyed client limit
   and the call authorizer, and is given back when the call reaches the
   component's gate (unary: before the bulkhead and the handler; stream:
   before the handler's open). This is wider than "until the resolver
   returned": the authenticator also runs for callers nobody has vouched
   for yet and may do I/O (a session store in the tenant's database), so
   it belongs inside the same bound. A call answered earlier, or dropped
   because the client went away, gives its slot back at once. The
   long-lived response of a stream is outside the admission. Both slots
   are released together, whichever way the call ends, cancellation
   included.
5. **A precheck before anything that costs.** A synchronous hook on the
   head, `EndUserPrecheck`, runs for exactly the calls the end-user
   authenticator will see (the bearer modes, no matching link token, not
   `anonymous`, and `optional_end_user` only with an `Authorization`
   header), before the admission, the read and the resolver. Its error is
   answered like the authenticator's: `UNAUTHENTICATED` becomes the
   constant answer (or an `EndUserRejection`'s reason and domain), any
   other code passes unchanged, so a precheck must never format a header
   value into its error; a panic is `INTERNAL` / `HANDLER_PANICKED`, and
   the process's panic hook still reports it. The default, `BearerShape`,
   accepts exactly one `Authorization` header of the form
   `Bearer <token68>` (a second `Authorization` header is refused); an App
   whose authenticator takes another credential registers its own. A
   refused call takes no slot and is never read or resolved.
6. **The hop limit moves forward.** It depends on the head alone, so it now
   runs with the other head-only checks, before the precheck and the read:
   a call over the limit is refused before end-user authentication and takes no
   slot.
7. **No configuration keys.** The bounds are set in code on the builder
   (`AppBuilder::serve_admission`); defaults apply without a call. They are
   capacity decisions of the service, like the scope resolver itself.
8. **The framework's part of slow-body protection.** The per-client cap
   (set below `max_pending`) and the read timeout bound what one address
   can hold; neither bounds a
   distributed attack, and rate limits alone bound neither in-flight slow
   requests nor many addresses. The deployment supplies the rest: a
   gateway that buffers request bodies or enforces its own body deadlines, and
   forwards the real client address through a trusted path (the runtime
   server's `ServerBuilder::client_addr` with a `ClientAddrPolicy` that
   trusts only the gateway), so `cx.client_addr()` names the client
   and not the gateway. Without it every call behind the gateway shares
   one address and one per-client cap.

### The answers

| Refusal | Code | Reason | Metadata |
|---|---|---|---|
| the precheck refuses (`BearerShape`: no well-formed bearer) | `UNAUTHENTICATED` | none, or the `EndUserRejection`'s | none |
| no shared slot free, or the client address holds `max_pending_per_client` calls | `RESOURCE_EXHAUSTED` | `ADMISSION_FULL` | `component`, `method` |
| request larger than `max_message_bytes` | `OUT_OF_RANGE` | `REQUEST_TOO_LARGE` | `max_bytes`, `component`, `method` |
| `read_timeout` passed before the request arrived whole | `DEADLINE_EXCEEDED` | `REQUEST_READ_TIMEOUT` | `timeout_ms`, `component`, `method` |
| the call's own deadline passed first (or had passed) | `DEADLINE_EXCEEDED` | none | `component`, `method` |
| the request body failed (a reset, a broken gRPC-Web body) | as tonic answers it (`tonic::Status::from_error`) | | |

- `OUT_OF_RANGE` is the code tonic gave an oversize request before, and the
  code an oversize stream item gets, so a client sees no change of kind.
- `REQUEST_READ_TIMEOUT` carries a reason so a client tells the server's
  read bound from its own deadline, which keeps the plain
  `DEADLINE_EXCEEDED`. On a tie the call's own deadline is the answer.
- `ADMISSION_FULL` is `RESOURCE_EXHAUSTED` like `BULKHEAD_FULL`: the
  server (or the client's share of it) is full, not the request wrong.
  Only callers without a link token ever see it. It is a serve admission
  refusal; the gate's own refusals (`COMPONENT_DRAINING`, shedding,
  `BULKHEAD_FULL`) are unchanged.

## 3. Public API

In `sekvent-component`, re-exported as `sekvent::component`; no feature
needed for any of it.

```rust
// src/admission.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeAdmission { /* private */ }

impl ServeAdmission {
    pub const DEFAULT_MAX_PENDING: usize = 512;
    pub const DEFAULT_MAX_PENDING_PER_CLIENT: usize = 32;
    pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);
    pub const MAX_READ_TIMEOUT: Duration = Duration::from_secs(300);
    pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
    pub const fn new() -> Self;                     // the defaults; also `Default`
    #[must_use] pub const fn with_max_pending(self, calls: usize) -> Self;
    #[must_use] pub const fn with_max_pending_per_client(self, calls: usize) -> Self;
    #[must_use] pub const fn with_read_timeout(self, timeout: Duration) -> Self;
    #[must_use] pub const fn with_max_message_bytes(self, bytes: usize) -> Self;
    pub const fn max_pending(&self) -> usize;
    pub const fn max_pending_per_client(&self) -> usize;
    pub const fn read_timeout(&self) -> Duration;
    pub const fn max_message_bytes(&self) -> usize;
}

// src/end_user.rs
pub trait EndUserPrecheck: Send + Sync + 'static {
    fn precheck(&self, request: &http::request::Parts, target: CallTarget) -> Result<(), AppError>;
}
impl<F> EndUserPrecheck for F
where
    F: Fn(&http::request::Parts, CallTarget) -> Result<(), AppError> + Send + Sync + 'static;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BearerShape;
impl BearerShape {
    /// The token of the one well-formed `Authorization: Bearer` header.
    pub fn token(headers: &http::HeaderMap) -> Option<&str>;
}
impl EndUserPrecheck for BearerShape;

// src/app.rs
impl AppBuilder<'_> {
    /// Optional; a later call replaces an earlier one.
    pub fn serve_admission(&mut self, admission: ServeAdmission) -> Result<(), BuildError>;
    /// One per App; without one, `BearerShape`.
    pub fn end_user_precheck(&mut self, precheck: impl EndUserPrecheck) -> Result<(), BuildError>;
}

// src/error.rs, in `BuildError` (non-exhaustive)
DuplicateEndUserPrecheck,
InvalidServeAdmission { reason: &'static str },

// src/reasons.rs
pub const ADMISSION_FULL: &str = "ADMISSION_FULL";
pub const REQUEST_TOO_LARGE: &str = "REQUEST_TOO_LARGE";
pub const REQUEST_READ_TIMEOUT: &str = "REQUEST_READ_TIMEOUT";
```

`serve_admission` refuses `max_pending` 0 or above
`tokio::sync::Semaphore::MAX_PERMITS`, `max_pending_per_client` 0, a zero
`read_timeout` or one above `MAX_READ_TIMEOUT`, and `max_message_bytes` 0
or above `MAX_MESSAGE_BYTES`, with
`InvalidServeAdmission` naming the bound. `CallTarget` is the call
authorizer's: the component, the method and its flags, so one precheck can
treat components differently. `AppBuilder`'s `Debug` gains
`end_user_precheck: bool` and `serve_admission`.

## 4. Serving order

| # | Step | Needs | Refusal answered from the head alone |
|---|---|---|---|
| 1 | link token | head | yes |
| 2 | route | head | yes |
| 3 | context and deadline | head | — |
| 4 | client limit by address (rate) | head | yes |
| 5 | hop limit | head | yes |
| 6 | **end-user precheck**, synchronous | head | yes |
| 7 | **serve admission**, shared and per client address (no link token) | — | yes |
| 8 | **request read**, bounded in size and time | body | |
| 9 | request scope (no link token) | head | |
| 10 | end-user authenticator | head, scope | |
| 11 | client limit by end user | end user | |
| 12 | call authorizer | context | |
| 13 | serve admission given back; gate (gate admission, shedding), bulkhead, method timeout, handler | body | |

Every refusal in steps 1 to 7 is answered without reading the body and
before any authentication beyond the link token comparison. The call's
cancellation guard is taken before step 8, so a client that goes away
during the read or the resolver cancels `cx.cancel_token()`.

## 5. Cancellation and work that outlives the client

When the client goes away (hyper drops the call's future) during the read
or the resolver, everything the call holds is dropped with the future: the
shared and the per-client serve admission slots (an address whose count
reaches zero loses its entry), the body, the resolver's future and what it owned, and the
cancel token is cancelled. A resolver whose work should finish anyway (a
pool for a site that many requests will need) starts that work on a task of
its own and bounds it with a budget of its own; the keyed resource cache
already does this for a creation, which goes on when the `get` waiting for
it is dropped. That budget is the application's choice, not the
framework's.

## 6. Out

| Item | Note |
|---|---|
| Configuration keys for the bounds | set in code (decision 7) |
| Queueing for a slot | refused at once; a queue would hold exactly the connections the bound protects against |
| A retry hint on `ADMISSION_FULL` | none; a client backs off as for `BULKHEAD_FULL` without one |
| Admission per method or per component | one per App; give a method its own capacity with its bulkhead |
| REST routes through `App::resolve_scope` | not admitted; a route bounds its body with the runtime server's `rest_body_limit` and its own timeouts |
| Streaming the request into the handler | requests are unary messages of at most 4 MiB; a larger upload goes through presigned storage |

## 7. Upgrade compatibility

What an App sees change when it moves to this version:

- **The default precheck accepts only a token68 bearer.** A request whose
  `Authorization` is not `Bearer <token68>` (another scheme, a token with
  characters outside token68) is refused before the authenticator. An App
  whose authenticator takes such a credential registers its own
  `EndUserPrecheck`.
- **A duplicate `Authorization` header is refused** by the default
  precheck, also when one of the values would be a valid credential.
- **Link callers get the read timeout.** They take no serve admission
  slot, but their request read is bounded by `read_timeout` like every
  other caller's; a peer that sends large requests over a slow path needs a
  longer `read_timeout`.
- **A missing credential gets the constant answer.** A call without a
  well-formed credential is refused by the precheck with
  `UNAUTHENTICATED` and `END_USER_REJECTED_MESSAGE`; the authenticator no
  longer sees it, so an answer it gave for that case (an
  `EndUserRejection`'s reason, another code) is replaced. An App that
  wants that answer back returns it from its own precheck.
- **The per-client cap.** One client address may hold at most
  `max_pending_per_client` calls (default 32) between admission and the
  gate; behind a gateway that does not forward the client address, every
  caller shares that cap. Raise it with `with_max_pending_per_client`, or
  forward the address through a trusted proxy.

## 8. Tests

Unit tests in `grpc/service.rs` drive `ComponentService` directly with
instrumented resolvers and bodies (paused clock where time matters, no
sleeps): a missing, foreign or malformed credential on a required or
optional method never reaches the resolver and takes no slot, while
anonymous and optional methods without one do; a registered precheck
decides, its errors map like the authenticator's and its panic is
contained; a stalled request never reaches the resolver and ends at the
read timeout (unary and stream), and an earlier call deadline answers
instead (including ties and an existing later deadline); the timeout tests
use paused time rather than waiting on a socket timer; one admission refuses the next call across two services, never
counts a link caller, frees the slot when the held call is dropped, and is
given back when a call reaches the handler or a stream is open; the
per-client cap refuses an address's next call while another address is
served, puts every unknown address in one bucket, removes an address's
entry at zero and releases both slots on cancellation; a client gone
during the read frees its slots; a request over the limit is refused
from its frame header or as soon as its bytes pass the limit, also
for a stream; a message of exactly the limit split over chunks with
trailers is served; a failing body is answered as tonic answers it.

`tests/grpc_admission.rs` checks the same over the wire through the runtime
server, natively over HTTP/2 and as gRPC-Web over HTTP/1.1: one admission
across two components, link callers outside it; refusals without a
credential answered while the request body never ends; a registered
precheck through the builder; binary and text-mode size limits; unary
gRPC-Web calls and server streams still served; the builder's refusals.
