# Component streaming: server-streaming methods

> Status: design for review. Builds on [`component-c1.md`](component-c1.md)
> (bindings, gate, lifecycle), [`component-c2.md`](component-c2.md) (gRPC,
> contracts, resilience) and [`component-end-user.md`](component-end-user.md)
> (serving to end users, gRPC-Web). Where this note is more specific, it wins
> for `#[call(stream)]` methods. It lifts C2's rule "component methods are
> unary" for **server streaming only**; client-streaming and bidirectional
> RPCs stay rejected. Everything else in those notes stays as it is.

## 1. Problem

Components answer every call with one reply. A browser frontend that shows
records other people change (a board, an order, a stock level) has to poll,
or the application writes a hand-made tonic streaming service next to its
components, with its own authentication, error mapping, deadlines and
shutdown handling: exactly what C2 and the end-user note removed for unary
calls. Inside one process the same need exists: a component that wants to
follow another component's changes has no way to say so through a handle.

This note adds **server-streaming component methods**: one request, a
stream of replies. A browser reaches them over gRPC-Web, which supports
server streaming and nothing more; services reach them over gRPC; code in
the same process reaches them through the same handle under `local` and
`local-serialized`. The open of a stream is a call (authenticated, gated,
timed, retried and counted like one); the stream after it is bounded by the
caller's deadline, a maximum lifetime, the consumer, and the component's
own drain and shutdown.

## 2. Scope

### In

- `#[call(stream)]` on a trait method returning
  `Result<CallStream<Item, ItemError>, Error>`, on standard, `remote_only`
  and `local_only` components, with the new option
  `#[call(stream, max_lifetime = "…")]`.
- `CallStream<T, E>`, the stream type implementations return and handles
  hand back.
- Contracts: `rpc X(A) returns (stream B)` maps to `#[call(stream)]`, checked
  both ways at compile time; the service constant of `sekvent-proto-build`
  carries both streaming flags.
- All three bindings, gRPC serving (native gRPC over HTTP/2 and gRPC-Web
  through the runtime server), the gate (admission, bulkhead, drain), the
  caller-side resilience of `grpc` (open retries, breaker), the hop limit,
  link and end-user authentication.
- A stream lifetime: `SEKVENT_COMPONENT_<C>[_<M>]_STREAM_MAX_LIFETIME`
  (default 15 minutes).
- Shutdown that never waits on a stream: streams end with `UNAVAILABLE` /
  `COMPONENT_DRAINING` when their component drains and, under
  `App::register`, as soon as the runtime begins shutting down;
  `App::close_streams` for Apps run outside the runtime.
- Observability: the access log line of a served stream gains `messages`;
  a `debug` event per stream where the component runs.
- `examples/shop`: `Inventory.WatchStock`, running under all three
  profiles (over gRPC in `split-grpc`).

### Out (later, or never)

| Item | Note |
|---|---|
| Client-streaming and bidirectional RPCs | Remain non-goals and compile errors (4.1). Browsers over gRPC-Web can only do server streaming. |
| Automatic reconnect or resume | The framework never reopens a stream. Resumption is the application's: put a cursor (last seen version or sequence) in the request (3.4). |
| An idle timeout | Decision 6. |
| Re-authenticating during a stream | Decision 5: the stream lifetime bounds how long a credential is trusted. |
| Heartbeat items | Application-level: a live-update stream sends its own heartbeat items so proxies in front of gRPC-Web over HTTP/1.1 keep it open (decided, 17). HTTP/2 keep-alive on the runtime server is in scope (section 7.1). |
| Buffer-size keys | Fixed: 16 items under `local-serialized`, HTTP/2 flow control under `grpc`. |
| Streams on `#[async_call]`, `#[deferred]`, topics | C3. |
| Metrics | sekvent has none; observability is logs and spans (section 12). |
| Compression | As for unary calls: none. |

## 3. Declaring, implementing and calling

### 3.1 Contract and trait

```proto
service Inventory {
  rpc Stock(StockRequest) returns (StockReply);
  // Stock of one SKU now, then again after every change.
  rpc WatchStock(StockRequest) returns (stream StockReply);
}
```

```rust
#[sekvent::component(name = "inventory", package = "shop.inventory.v1", proto = "crate::proto::shop::inventory::v1")]
pub trait Inventory: Send + Sync + 'static {
    #[call(idempotent, timeout = "500ms")]
    async fn stock(&self, cx: &CallContext, req: StockRequest) -> Result<StockReply, InventoryError>;

    /// Stock of one SKU now, then again after every change.
    #[call(stream, idempotent, timeout = "500ms")]
    async fn watch_stock(&self, cx: &CallContext, req: StockRequest)
        -> Result<CallStream<StockReply, InventoryError>, InventoryError>;
}
```

Rules for a `#[call(stream)]` method, besides C1's method rules:

- It returns `Result<CallStream<Item, ItemError>, Error>`. `Item` is the
  RPC's output message (a prost message; any `Send + 'static` type on a
  `local_only` component). `Error`, the error of the **open**, and
  `ItemError`, the error that **ends** the stream, both implement
  `ComponentError`; they are usually the same type, and may differ.
- The outer `Result` is the open: authentication, validation, access checks
  and subscription happen there and fail before any item. The stream is
  what follows.
- Every other `#[call]` option combines with `stream`: `idempotent` (the
  open may be retried under `grpc`), `timeout` (bounds the open, 5.2),
  `bulkhead` (bounds concurrently open streams, 5.4), `anonymous`,
  `optional_end_user`. `max_lifetime = "<humantime>"` sets the default
  lifetime and is only valid with `stream`.
- Allowed on every mode. On `local_only` the item may be any
  `Send + 'static` type (no contract, `local` only).

### 3.2 `CallStream<T, E>`

A concrete, boxed, sendable stream (section 8.1 has the signatures):

- `CallStream::new(stream)` wraps any
  `Stream<Item = Result<T, E>> + Send + 'static` (`futures_core::Stream`, the
  trait tonic and tokio-stream use). `from_receiver(mpsc::Receiver)`,
  `iter(items)` and `empty()` cover the common producers.
- It implements `Stream` and `FusedStream`, plus an inherent
  `async fn next(&mut self)`, so consumers need no extension trait.
- **It ends at its first error.** After it yields `Err` or `None` it yields
  `None` forever and has already dropped the stream it wrapped (releasing
  whatever that held). gRPC cannot continue after an error status, so this
  is the only semantics every binding can honour.
- Dropping it ends the call under every binding: the producer is dropped
  and the call's cancellation token is cancelled (6.6).

### 3.3 Implementing

```rust
impl Inventory for InventoryService {
    async fn watch_stock(&self, _cx: &CallContext, req: StockRequest)
        -> Result<CallStream<StockReply, InventoryError>, InventoryError>
    {
        let Some(levels) = self.levels(&req.sku) else {
            return Err(InventoryError::UnknownSku { sku: req.sku });   // the open fails
        };
        let sku = req.sku;
        // WatchStream yields the current value first, then every change.
        let updates = WatchStream::new(levels.subscribe())
            .map(move |available| Ok(StockReply { sku: sku.clone(), available }));
        Ok(CallStream::new(updates))
    }
}
```

- The context the method receives is the **stream's** context (5.2): its
  deadline is the end of the stream (the caller's deadline or the lifetime,
  whichever comes first), never the open's `TIMEOUT`, so a producer may
  clone it and use it for calls it makes while streaming.
- Its token is cancelled when the stream ends for any reason. A producer
  that spawns a task (typically feeding `CallStream::from_receiver`) should
  stop on `cx.cancelled()` or `Sender::closed()`; work meant to outlive the
  stream takes `cx.detached()`.
- Items are delivered in order and at most once. Items buffered when a
  stream ends are lost; nothing is redelivered.

### 3.4 Calling

The handle method mirrors the trait signature: it returns
`impl Future<Output = Result<CallStream<Item, ItemError>, Error>> + Send + 'a`,
which resolves once the stream is **open**.

```rust
let mut stock = inventory.watch_stock(&cx, StockRequest { sku }).await?;   // open errors: InventoryError
while let Some(level) = stock.next().await {
    let level = level?;                                                    // the error that ended it
    render(level);
}
```

A consumer that must follow indefinitely loops: reopen after the stream
ends, with backoff on errors, and immediately (no backoff) after
`STREAM_EXPIRED` or `COMPONENT_DRAINING`. Where a gap matters, the request
carries the last version seen and the open replays from there.

### 3.5 Why this shape

- **A named boxed type, not `impl Stream`.** An RPITIT
  `-> Result<impl Stream + Send + 'static, E>` inside the already rewritten
  `impl Future` return would capture `&self` under edition 2024 rules unless
  precise capturing excludes it, the object-safe `__XDyn` twin would box it
  anyway, and callers could not name the type to store a stream in a
  struct. `CallStream` costs one allocation per stream, keeps the trait,
  the twin and the handle exactly as in C1, and gives the framework one
  place to enforce "ends at the first error".
- **A flag on `#[call]`, not a new kind.** A stream has call semantics:
  synchronous within its connection, the same bindings (never a queue),
  the same options. `MethodKind` stays `Call`; the descriptor gains
  `is_stream()`.
- **The flag is explicit.** `stream` must be written and the return type
  must be `Result<CallStream<…>, …>`; each without the other is a compile
  error (11.2), so a type alias or a typo never silently changes the wire.

## 4. Contract

### 4.1 Mapping, checked both ways

| Trait method | Proto RPC | Result |
|---|---|---|
| `#[call]` | `rpc X(A) returns (B)` | ok (unchanged) |
| `#[call(stream)]` | `rpc X(A) returns (stream B)` | ok; `B` is the item type |
| `#[call]` | `rpc X(A) returns (stream B)` | compile error |
| `#[call(stream)]` | `rpc X(A) returns (B)` | compile error |
| either | `rpc X(stream A) returns (B)` or `(stream A) returns (stream B)` | compile error |

### 4.2 The service constant carries both flags

**Contradiction with the tree as briefed:** the compile-time constant
`sekvent-proto-build` emits today records one bool per RPC,
`client_streaming || server_streaming`, which cannot tell a server stream
from a client or bidirectional one. It becomes a 5-tuple:

```rust
/// (RPC name, request full name, reply full name, client streaming, server streaming)
pub type ProtoRpc = (&'static str, &'static str, &'static str, bool, bool);
pub type ProtoService = (&'static str, &'static [ProtoRpc]);   // unchanged
```

```rust
pub const __sekvent_service_Inventory: (&str, &[(&str, &str, &str, bool, bool)]) = (
    "shop.inventory.v1.Inventory",
    &[
        ("Stock", "shop.inventory.v1.StockRequest", "shop.inventory.v1.StockReply", false, false),
        ("WatchStock", "shop.inventory.v1.StockRequest", "shop.inventory.v1.StockReply", false, true),
    ],
);
```

The `__sekvent_rpc_<Service>__<Rpc> = (Request, Reply)` alias is unchanged;
for a streaming RPC its second type is the item. This is a `__private`
format change: generated code follows on the next build, and every
hand-written constant (tests, the facade check, trybuild cases, docs) gains
the second flag. Projects with hand-written constants in their own tests do
the same. The generated constant carries
`#[allow(non_upper_case_globals, dead_code, clippy::type_complexity)]`: the
5-tuple type trips clippy's `type_complexity` in the crate that includes it
(hand-written constants under `-D warnings` need the same allow).

### 4.3 Compile-time checks

`assert_rpc` keeps its signature and now accepts only unary RPCs;
`assert_stream_rpc` is new (8.2). Const-panic messages (exact):

| Check | Message |
|---|---|
| `#[call]` method, RPC streams replies only (changed) | `component contract: the RPC named after this method streams its replies; declare the method #[call(stream)]` |
| any method, RPC streams requests (new) | `component contract: the RPC named after this method streams requests; component methods take one request` |
| `#[call(stream)]` method, unary RPC (new) | `component contract: the RPC named after this method does not stream its replies; remove stream from #[call]` |
| `#[call(stream)]` method, item type (new) | `component contract: this method's stream item type is not the RPC's output type` |
| request type, missing RPC, service name, RPC sets | unchanged C2 messages, for both functions |

`assert_rpc_types::<Req, Item, Alias>()` checks the item by type identity,
unchanged.

### 4.4 `contract emit` / `check`: no change

Verified in `sekvent-tasks`: baselines (format 2) already record
`client_streaming` and `server_streaming` per RPC, and R4 reports any change
of either. A new streaming RPC is compatible (a new RPC); turning a unary
RPC into a streaming one, or back, stays an R4 break: a peer built against
the old contract misreads the wire. Nothing in `sekvent-tasks` changes.

## 5. Semantics common to every binding

### 5.1 Two phases

1. **Open** — everything up to the stream existing: hop limit, admission,
   dead-call shedding, bulkhead wait, (when served) link and end-user
   authentication, request decoding, the handler's future resolving to
   `Ok(CallStream)` or `Err(e)`. It behaves like a unary call: timed by
   `TIMEOUT`, retried and counted by the breaker under `grpc` (5.5).
2. **Stream** — items until one of the ends of 5.3.

The open completes when the handler's future resolves, **not** at the
first item: a live stream may legitimately stay quiet for a long time,
and the open reply is the callee's signal that it is healthy and accepted
the call. On the wire it is the response headers (6.3).

### 5.2 Time

- **The open bound.** The caller side bounds the open by its own `TIMEOUT`
  and its deadline and reports expiry exactly as for a unary call:
  `DEADLINE_EXCEEDED` / `METHOD_TIMEOUT` when the `TIMEOUT` passed while the
  caller still had time (counted against the callee's breaker), plain
  `DEADLINE_EXCEEDED` otherwise. The serving side bounds the open by its own
  `TIMEOUT`, the incoming deadline and the stream's lifetime: plain
  `DEADLINE_EXCEEDED` for the first two, as for unary calls, and
  `STREAM_EXPIRED` for the lifetime.
- **`TIMEOUT` never narrows the stream's context.** Unlike a unary call,
  the method's `TIMEOUT` is a timer on the open only; it is not put into
  the context's deadline and not sent as `grpc-timeout`. Otherwise every
  call a producer makes while streaming would fail once the open's
  `TIMEOUT` had passed.
- **The caller's deadline bounds the whole stream**, open and items, as
  gRPC defines `grpc-timeout`. tonic's client channel only times the
  response headers (`GrpcTimeout` wraps the response future, verified in
  tonic 0.14.6), so the caller side enforces the deadline on the stream
  itself under every binding.
- **Lifetime.** Every stream has one, enforced where the component runs
  (local bindings and gRPC serving): the resolved
  `STREAM_MAX_LIFETIME` (section 9; default
  `DEFAULT_STREAM_MAX_LIFETIME` = 15 minutes), counted from admission.
  Each stream draws its own lifetime uniformly between 90 % and 100 % of
  that value, so streams opened together (after a deploy) do not all end
  together; it never exceeds the configured value. A value too large to
  add to the clock means no lifetime.
- **The stream's context.** The handler's context has the deadline
  `min(caller's deadline, admission + drawn lifetime)` and a token that is
  cancelled when the stream ends for any reason (3.3).
- All arithmetic uses tokio's clock (C1 2.5), so paused-clock tests are
  exact.

### 5.3 How a stream ends

| Cause | The consumer sees | Made (and tagged `component` / `method`) by |
|---|---|---|
| The producer returns `None` | `None`; gRPC `OK` | — |
| The producer yields `Err(e)` | `Err(e)`, then `None` | the producer (untouched) |
| The consumer drops the stream | — | — |
| The caller's context is cancelled | `CANCELLED`, "the call was cancelled" | caller side |
| The caller's deadline passes | `DEADLINE_EXCEEDED`, "the call deadline was exceeded" | the side that notices first (same values) |
| The lifetime passes | `UNAVAILABLE` / `STREAM_EXPIRED`, metadata `max_lifetime_ms` (the configured value) | serving side |
| The component closes its streams (5.4) | `UNAVAILABLE` / `COMPONENT_DRAINING` ("component … is shutting down"); `COMPONENT_STOPPED` once stopped | the side whose gate closed |
| The producer panics | as for unary: `local` propagates the panic into the consumer's poll; `local-serialized` `INTERNAL` "component … method … panicked"; `grpc` `INTERNAL` / `HANDLER_PANICKED` | serving side |
| The transport fails mid-stream (`grpc`) | `UNAVAILABLE` / `COMPONENT_UNREACHABLE` | caller side |
| An item does not decode | `INTERNAL` / `MALFORMED_REPLY` | caller side |
| An item is above 4 MiB (`grpc`) | `OUT_OF_RANGE` (tonic's message) | serving side |

- **Precedence.** Before polling the producer, a stream checks its end
  conditions: closing first, then whichever deadline passed first (the
  caller's when both passed at once). A terminal condition wins over an item
  that is ready at the same poll.
- **Resources are released at the end, not at drop.** When a stream yields
  its end (`None` or the terminal error), it drops the producer, its
  admission, its bulkhead permit and its timers at once, and cancels the
  stream's token, even if the consumer keeps the `CallStream` around.

### 5.4 The gate

- **Admission is held for the whole stream.** An open stream counts as in
  flight, so a drain sees it.
- **The bulkhead counts open streams**: the permit is taken during the open
  (queue settings included; the wait is part of the open) and held until
  the stream ends. A stream holds resources (a subscription, a task, a
  connection slot) for its whole life; counting only opens would let
  streams accumulate without bound, which is what a bulkhead exists to
  prevent. Size stream bulkheads for concurrent subscribers, not for a rate.
- **Closing.** Every gate gets a closing signal, fired when its component
  leaves `Serving` (drain or stop) and by `App::close_streams`. It ends
  every open stream of the component (5.3) and refuses new opens with
  `UNAVAILABLE` / `COMPONENT_DRAINING` (increment-then-check, as C1's
  admission). `App::close_streams` leaves unary calls alone.
- **Where the gate acts.** Under `local` and `local-serialized`, and when
  served over gRPC, on the serving side (the component's own gate, with
  bulkheads). Under `grpc`, the caller side's remote gate (no bulkhead)
  also holds an admission per outbound stream and closes them when the
  calling App stops, so `App::stop` and `App::state` behave the same under
  every binding (C2 decision 7).
- **A stream nobody polls** notices closing at its next poll. Until then it
  counts as in flight, and the drain waits for it within the grace, like
  any slow call. Streams that are being consumed (a loop on `next()`,
  hyper writing a response, the serialized task) notice at once.

### 5.5 Caller-side resilience under `grpc`

- **Retries apply to the open only**: `idempotent` streaming methods,
  budgeted, within the open bound, `Retry-After` capped, as for unary calls.
  A stream that ended is never reopened by the framework.
- **The breaker counts open outcomes only.** The permit is taken when the
  open starts and recorded when it completes (headers received, or a
  failure, with C2's caused-by-the-caller rules). Errors after the open
  never reach the breaker; half-open probes are opens.
- **Hop limit** at the open, on both sides, as for unary calls.

### 5.6 Errors

- **Open errors** are unary errors in every respect: `Error::into_app_error`
  on the serving side, `Error::from_app_error` at the caller, tagging per
  C1 2.5, `downstream` marking for opens made inside a handler, and
  `INTERNAL` / `DOWNSTREAM_FAILURE` at a serving boundary (C2 2.3).
- **Item errors** go through `ItemError` per item, are **never** marked
  `downstream` nor converted to `DOWNSTREAM_FAILURE`, and cross every hop
  unchanged. Their purpose differs: an edge client reconnects after an
  `UNAVAILABLE` / `COMPONENT_DRAINING` that a relaying component forwarded,
  retries are not involved, and no breaker counts them.
- **Identical values across bindings**, as for unary calls: the same
  serving-side outcome gives the same item sequence and the same terminal
  error (code, message, reason, metadata) under `local`, `local-serialized`
  and `grpc`. The differences are panics (as for unary calls), how far a
  producer runs ahead (none under `local`, 16 items under
  `local-serialized`, the HTTP/2 window under `grpc`) and how fast a
  dropped consumer is noticed (6.6).

### 5.7 Authentication

- Link and end-user authentication run on the request head before the open,
  exactly as for unary calls (end-user note, section 5). `anonymous` and
  `optional_end_user` mean what they mean for unary methods. The
  end-user authenticator is bounded by the open bound.
- The handler sees `cx.caller()` / `cx.end_user()`; a producer that needs
  them captures them during the open.
- **No re-check during the stream.** A token that expires, is revoked or is
  rotated after the open does not end the stream; the stream lifetime does.
  The default of 15 minutes keeps that window short; applications set
  `STREAM_MAX_LIFETIME` (or `max_lifetime`) no longer than their access
  tokens live. The same holds for link tokens.

## 6. Bindings

### 6.1 `local`

- `Endpoint::call_stream` runs C1's caller side for the open (dead-call
  shed, hop limit, callee context without `TIMEOUT` narrowing, the open
  bound raced with the caller's cancellation) and the serving side's
  `open_stream` (admission, shed, bulkhead, the handler's open future).
- The returned `CallStream` is two wrappers around the producer: the
  serving wrapper (closing, lifetime, the stream's deadline, the debug
  event, admission and permit, typed `ItemError` normalised through
  `into_app_error`) inside the caller wrapper (the caller's cancellation and
  deadline, `ItemError::from_app_error`). Nothing is encoded; the producer is
  polled by the consumer, in the consumer's task.

### 6.2 `local-serialized`

- The caller encodes the request and the context (`headers::inject`, the
  callee context carrying the caller's deadline only), and spawns the
  serving task (instrumented with the caller's span, as C1). The task
  rebuilds the context (trusted `local` caller, the caller's child token, the
  caller's deadline pinned as C1 does), runs `open_stream` around
  `dispatch.dispatch_stream(method, cx, body)`, and reports the open on a
  `oneshot`: `Ok(())` or the encoded error.
- Items travel over a bounded `mpsc` channel of **16**: `Ok(bytes)` per
  item, the encoded terminal error last; the end of the channel without an
  error is the end of the stream. The task sends with `send().await`, so a
  slow consumer stops the producer (backpressure); a producer runs at most
  16 items ahead.
- The caller wrapper owns the task's `AbortOnDrop`. Item bytes decode to
  `Item` (`INTERNAL` / `MALFORMED_REPLY` on failure, which ends the stream),
  error bytes to `AppError` then `ItemError`. When the channel ends, the
  wrapper reads the task's result: a panic is `INTERNAL`
  "component … method … panicked", a runtime cancellation `CANCELLED`, as
  C1's `task_failed`. A `oneshot` dropped without an answer is read the same
  way.
- No size limit, as for unary calls under this binding.

### 6.3 `grpc`, caller side

- `RemoteClient::open_stream(method, cx, body)`: dead-call shed, breaker
  permit, open bound (the method's `TIMEOUT` when it comes before the
  caller's deadline), the retry policy over open attempts (deadline-aware
  against the open bound, while the headers carry only the caller's
  deadline), and per attempt: fresh headers (context, then the bearer),
  `ready()`, `tonic::client::Grpc::server_streaming(request, path,
  BytesCodec)`. A trailers-only answer is an open failure and goes through
  C2's `classify`; headers received are a successful open. The breaker
  records the open's outcome, then the permit is gone.
- The tonic `Streaming<Bytes>` is mapped item by item: `Ok(bytes)` decodes
  as in 6.2; `Err(status)` with an underlying error is the transport
  (`UNAVAILABLE` / `COMPONENT_UNREACHABLE`, tagged), any other status is the
  server's terminal error, decoded with `from_status` and untouched. tonic's
  default 4 MiB decoding limit stays.
- The caller-side remote gate admits the stream and holds the admission;
  its closing ends the stream with `COMPONENT_DRAINING` (tagged) and drops
  the `Streaming`, which resets the HTTP/2 stream.
- The channel's HTTP/2 keep-alive (C2: 30 s interval, 10 s timeout) also
  detects a dead server under a quiet stream: the stream then ends with
  `COMPONENT_UNREACHABLE`.

### 6.4 `grpc`, serving side

C2 2.4 and the end-user note's serving order apply, with these changes for
a method whose descriptor says `is_stream()`:

1. **Route, link, context** as before, except that the method's `TIMEOUT`
   does not narrow the context; it yields the open bound
   (`min(context deadline, now + TIMEOUT)`).
2. **End user** (bearer modes) as before, bounded by the open bound.
3. **Hop limit** as before.
4. **Open.** `Grpc::new(BytesCodec).max_decoding_message_size(4 MiB)
   .max_encoding_message_size(4 MiB).server_streaming(…)` within the open
   bound. Its service runs `Server::open_stream` around
   `dispatch.dispatch_stream(method, cx, body)` (panics caught as for
   unary calls). An open error passes `from_handler` (`DOWNSTREAM_FAILURE`
   rule) and is answered trailers-only; the open bound passing answers
   `DEADLINE_EXCEEDED`, tagged, trailers-only. A drop guard cancels the
   call's token if the response future is dropped.
5. **Stream.** The response headers go out at once (that is the open for
   the caller). Each item is one gRPC message; the end is the trailers:
   `grpc-status: 0`, or the terminal error's status with its
   `google.rpc.Status` details (C2 2.3). Items are capped at 4 MiB
   (`OUT_OF_RANGE` from tonic ends the stream); unary replies keep their
   current behaviour. A panic while polling the producer is caught and
   ends the stream with `INTERNAL` / `HANDLER_PANICKED`; it never unwinds
   into hyper. The response carries a `MessageCount` the access log reads
   (section 12). The serving wrapper owns the token's drop guard, so the
   token is cancelled when hyper drops the body.

### 6.5 gRPC-Web

Nothing new is needed in `sekvent-runtime`: `tonic_web::GrpcWebLayer`
(feature `runtime-grpc-web`) passes data frames through as they come and
turns the trailers into the final `0x80` frame, so a browser receives each
item as it is produced (pinned by a test, 14). CORS, the prefix and the
health endpoints are unchanged; a trailers-only error answer keeps
`grpc-status` in the exposed headers.

Two deployment facts the docs state:

- Over HTTP/1.1 every open stream holds one connection: one of a browser's
  six per origin, and one of the server's `max_connections`. Browsers need
  HTTP/2 to the edge (TLS at the proxy) when a page opens several streams.
- Proxies cut quiet responses after their idle timeout (often 60 s). A
  stream that may stay quiet longer sends application heartbeats (a
  `oneof` in the item) or lets the client reopen.

### 6.6 Drop paths (what the tests prove)

| Binding | The consumer drops its `CallStream` → | The producer is dropped by | The stream's token is cancelled by |
|---|---|---|---|
| `local` | the caller wrapper drops the serving wrapper it owns | that drop, synchronously | the serving wrapper's drop guard |
| `local-serialized` | `AbortOnDrop` aborts the serving task | tokio dropping the aborted task's future | the serving wrapper's drop guard in the task |
| `grpc` | dropping tonic's `Streaming` resets the HTTP/2 stream (`RST_STREAM`) | hyper dropping the response body (it polls for a reset while the body is pending) | the serving wrapper's drop guard on the server |
| gRPC-Web over HTTP/1.1 | the browser closes the connection | hyper dropping the body when the connection fails, at the latest at the next write | same |

## 7. Shutdown and drain

The runtime drains **ingress before components**, and the server's
graceful shutdown waits for open responses until the stage grace, then
aborts the unit (verified in `sekvent-runtime`). An open stream would hold
every shutdown for the whole grace if streams only ended at the component
drain. Hence:

1. **`App::register`** takes `RuntimeBuilder::shutdown_trigger()`. Once
   the App is running, its unit waits for `ShutdownTrigger::triggered()`
   (shutdown begun, for any reason) and calls `App::close_streams()`: every
   open stream ends with `UNAVAILABLE` / `COMPONENT_DRAINING` and new opens
   are refused; unary calls continue. Then ingress drains without waiting on
   a stream, and the components unit stops the App when its stage drains,
   as today.
2. **`App::stop`** fires each component's closing as it moves it to
   `Draining` (reverse install order), before waiting for in-flight calls.
   A component that relays another's stream is installed after it, so it
   drains first and drops its inner streams.
3. **Without the runtime**, an application that serves `App::grpc_routes()`
   on its own server calls `app.close_streams()` before draining that
   server, then `app.stop(grace)`.

Streams opened during the runtime's `shutdown_delay` are refused (the
instance is leaving the load balancer then); clients reopen against another
instance at once. Decided: streams close when shutdown begins, not at the end
of the delay.

### 7.1 HTTP/2 keep-alive on the runtime server

A quiet stream to a vanished peer otherwise lives until its lifetime
(risk "undetected dead peers"). The runtime server gains

```rust
impl ServerBuilder {
    /// Sends HTTP/2 PING frames every `interval` on idle connections and
    /// closes a connection whose ping is not acknowledged within `timeout`.
    /// `None` (the default) sends no pings. Both durations must be non-zero.
    pub fn http2_keep_alive(mut self, keep_alive: Option<Http2KeepAlive>) -> Self;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Http2KeepAlive { pub interval: Duration, pub timeout: Duration }
```

wired to hyper-util's `http2().keep_alive_interval` / `keep_alive_timeout`
on the auto connection builder. Off by default, so existing services do not
change; `docs/modules/server.md` recommends `30 s` / `20 s` for services
that serve streams. HTTP/1.1 connections (gRPC-Web through most proxies)
get no pings; heartbeat items are the application's tool there. Tests: a
server with keep-alive closes a connection whose client stops answering
pings (a raw h2 client with ping handling disabled, real clock, guarded by
a 30 s timeout), and the builder rejects zero durations. Owner: agent C
(`crates/sekvent-runtime/src/server.rs` and its tests), see section 16.
## 8. Public API

Everything is additive except the `__private` contract types (4.2).

### 8.1 `sekvent-component`

Manifest: `futures` (`Stream`, `FusedStream`), `tokio-util` (the closing
signal) and `rand` (lifetime jitter) as `{ workspace = true }`;
`sekvent-telemetry = { workspace = true, optional = true }`, enabled by
`grpc`. All are already in the workspace and the graph; no root
`Cargo.toml` change.

```rust
// stream.rs (new), re-exported at the crate root
/// The stream a `#[call(stream)]` method answers with, and what its handle
/// returns: items until the producer ends, or until the first error, which
/// ends it. Dropping it ends the call under every binding.
#[must_use = "a stream does nothing unless polled, and dropping it ends the call"]
pub struct CallStream<T, E> { /* Option<Pin<Box<dyn Stream<Item = Result<T, E>> + Send>>> */ }

impl<T: Send + 'static, E: Send + 'static> CallStream<T, E> {
    /// Wrap any sendable stream.
    pub fn new<S>(stream: S) -> Self where S: Stream<Item = Result<T, E>> + Send + 'static;
    /// Items sent on `receiver`; the stream ends when every sender is gone.
    pub fn from_receiver(receiver: tokio::sync::mpsc::Receiver<Result<T, E>>) -> Self;
    /// `items`, then the end.
    pub fn iter<I>(items: I) -> Self where I: IntoIterator<Item = Result<T, E>>, I::IntoIter: Send + 'static;
    /// A stream that ends without an item.
    pub fn empty() -> Self;
}
impl<T, E> CallStream<T, E> {
    /// The next item; `None` once the stream has ended.
    pub async fn next(&mut self) -> Option<Result<T, E>>;
}
impl<T, E> Stream for CallStream<T, E> { type Item = Result<T, E>; /* poll_next, size_hint */ }
impl<T, E> FusedStream for CallStream<T, E> { /* is_terminated */ }
impl<T, E> fmt::Debug for CallStream<T, E>;   // CallStream { terminated: bool }

// lib.rs
pub use stream::CallStream;
/// Default of `STREAM_MAX_LIFETIME`.
pub const DEFAULT_STREAM_MAX_LIFETIME: Duration = Duration::from_secs(15 * 60);

// descriptor.rs
impl MethodDescriptor {
    /// A server-streaming call: the method answers with a `CallStream`.
    #[must_use] pub const fn with_stream(self) -> Self;
    pub const fn is_stream(&self) -> bool;
    /// Default lifetime of the method's streams (`#[call(stream, max_lifetime = …)]`).
    #[must_use] pub const fn with_max_lifetime(self, max_lifetime: Duration) -> Self;
    pub const fn max_lifetime(&self) -> Option<Duration>;
}   // MethodKind is unchanged: a stream is a Call.

// app.rs
impl App {
    /// End every open stream of every component with `UNAVAILABLE` /
    /// `COMPONENT_DRAINING` and refuse new ones the same way; unary calls
    /// are not affected. Irreversible. `App::register` calls it when the
    /// runtime begins shutting down; call it yourself before draining a
    /// server you run outside the runtime.
    pub fn close_streams(&self);
}

// reasons.rs
/// The stream reached its maximum lifetime (`UNAVAILABLE`); open it again.
pub const STREAM_EXPIRED: &str = "STREAM_EXPIRED";
```

`App::register` keeps its signature (its behaviour gains section 7, step
1). The crate docs gain a "Streams" paragraph.

### 8.2 `__private` (the contract with the macro)

```rust
pub type ProtoRpc = (&'static str, &'static str, &'static str, bool, bool);   // 4.2

pub trait Dispatch: Send + Sync + 'static {
    /// Unary methods. Default: `UNIMPLEMENTED` (a component without any).
    fn dispatch(&self, method: usize, cx: CallContext, body: Bytes)
        -> BoxFuture<'static, Result<Bytes, AppError>> { /* default */ }
    /// Streaming methods: decode the request and open the stream of
    /// encoded items. Default: `UNIMPLEMENTED` (a component without any).
    fn dispatch_stream(&self, method: usize, cx: CallContext, body: Bytes)
        -> BoxFuture<'static, Result<CallStream<Bytes, AppError>, AppError>> { /* default */ }
}

/// Decode `Req` (`INVALID_ARGUMENT` / `MALFORMED_REQUEST`), open the stream
/// with `call`, map an open error with `E::into_app_error`, then encode
/// every item and map the error that ends the stream with
/// `EI::into_app_error`.
pub fn serve_stream<Req, Item, E, EI, F, Fut>(cx: CallContext, body: Bytes, call: F)
    -> BoxFuture<'static, Result<CallStream<Bytes, AppError>, AppError>>
where
    Req: WireMessage, Item: WireMessage, E: ComponentError, EI: ComponentError,
    F: FnOnce(CallContext, Req) -> Fut + Send + 'static,
    Fut: Future<Output = Result<CallStream<Item, EI>, E>> + Send + 'static;

/// `UNIMPLEMENTED` naming the component and the index.
pub fn unknown_stream_method(descriptor: &'static ComponentDescriptor, method: usize)
    -> BoxFuture<'static, Result<CallStream<Bytes, AppError>, AppError>>;

impl<D: ?Sized + Send + Sync + 'static> Endpoint<D> {
    /// Open streaming method `method` of a standard or remote_only component.
    pub fn call_stream<'a, Req, Item, E, EI, L, Fut>(&'a self, method: usize, cx: &'a CallContext,
        req: Req, local: L) -> impl Future<Output = Result<CallStream<Item, EI>, E>> + Send + 'a
    where
        Req: WireMessage, Item: WireMessage, E: ComponentError, EI: ComponentError,
        L: FnOnce(Arc<D>, CallContext, Req) -> Fut + Send + 'a,
        Fut: Future<Output = Result<CallStream<Item, EI>, E>> + Send + 'a;
    /// The same for a local_only component (any other route is `INTERNAL`,
    /// as for `call_local`).
    pub fn call_stream_local<'a, Req, Item, E, EI, L, Fut>(&'a self, method: usize,
        cx: &'a CallContext, req: Req, local: L)
        -> impl Future<Output = Result<CallStream<Item, EI>, E>> + Send + 'a
    where
        Req: LocalMessage, Item: LocalMessage, E: ComponentError, EI: ComponentError,
        L: FnOnce(Arc<D>, CallContext, Req) -> Fut + Send + 'a,
        Fut: Future<Output = Result<CallStream<Item, EI>, E>> + Send + 'a;
}

/// Const-panics unless `proto` has an RPC `rpc` that streams its replies
/// and not its requests, whose request is `Req` and whose output is `Item`
/// (4.3).
pub const fn assert_stream_rpc<Req: ContractMessage, Item: ContractMessage>(proto: ProtoService, rpc: &str);
```

Internal shape, fixed so tests and reviewers can rely on it (names are the
implementer's): `Server::open_stream(self: &Arc<Self>, method, cx,
open_deadline, open)` returns the serving wrapper; the gate gains the
closing signal (fired by `set_state` to anything but `Serving` and by
`close_streams`) and an owned admission guard; `MethodPolicy` gains
`max_lifetime: Option<Duration>` (`Some` exactly for streaming methods);
`link.rs` gains `invoke_stream` and the caller wrapper; `grpc/client.rs`
gains `open_stream`; `grpc/service.rs` gains the streaming route of 6.4;
`wire.rs` gains a panic-catching stream wrapper next to `CatchPanic`.

### 8.3 `sekvent-telemetry`

```rust
// access_log.rs, next to RouteTemplate
/// Counts the messages of a streaming response. Put a clone in the
/// response's extensions and count as messages go out; the access log's
/// completion event then has `messages`.
#[derive(Debug, Clone, Default)]
pub struct MessageCount(Arc<AtomicU64>);
impl MessageCount {
    pub fn new() -> Self;
    /// Count one more message.
    pub fn increment(&self);
    /// Messages counted so far.
    pub fn get(&self) -> u64;
}
```

`Completion::respond` keeps a clone when the response has one; `finish`
logs `messages` (absent otherwise), read at completion, not at headers
time.

### 8.4 `sekvent-proto-build`

The service generator emits the 5-tuple of 4.2:
`(proto_name, input full name, output full name, client_streaming,
server_streaming)`; the doc comment says "whether it streams requests,
replies". `service_contracts(false)` and `services_only` still omit it.

### 8.5 Facade

`sekvent::prelude` gains `CallStream` (feature `component`), so `-api`
crates that `use sekvent::prelude::*` can declare streaming methods. No new
feature: streams are part of `component`, and of `component-grpc` over the
wire.

## 9. Configuration

| Key | Values | Acts on |
|---|---|---|
| `SEKVENT_COMPONENT_<C>_STREAM_MAX_LIFETIME` | humantime (`15m`, `2h`) or whole seconds, > 0 | default for every streaming method of `<C>`; known only when `<C>` has one |
| `SEKVENT_COMPONENT_<C>_<M>_STREAM_MAX_LIFETIME` | same | streaming method `<M>`; known only for streaming methods |

- Read with `sekvent_config::__private::opt_duration_value`; malformed and
  zero values are `Malformed` / `Invalid` naming the key, never the value.
- Precedence, lowest first: `DEFAULT_STREAM_MAX_LIFETIME`, the
  `max_lifetime` attribute, the component key, the method key. Named
  policies do not carry it (it is not a `PolicySpec` field):
  `SEKVENT_POLICY_<N>_STREAM_MAX_LIFETIME` is an unknown key.
- It acts where the component runs (local bindings and gRPC serving), like
  `BULKHEAD_*`; under the `grpc` binding the caller accepts it and ignores
  it. The key set still does not depend on the binding.
- Existing fields keep their meaning with one clarification: on a
  streaming method `TIMEOUT` bounds the open (5.2), `BULKHEAD_*` bounds
  concurrently open streams (5.4), `RETRY_*` applies to opens, `BREAKER_*`
  counts opens.
- C1's exact-equality collision check covers the new keys; for example a
  streaming method `stream` of component `a` and component `a_stream` both
  produce `SEKVENT_COMPONENT_A_STREAM_STREAM_MAX_LIFETIME`, a
  `KeyCollision`.
- No new framework prefix: the keys are under `SEKVENT_COMPONENT_`.

## 10. Errors and reasons

| Situation | Code | Reason | Retry by the framework |
|---|---|---|---|
| Lifetime reached | `UNAVAILABLE` | `STREAM_EXPIRED` (new; metadata `max_lifetime_ms`) | never (mid-stream) |
| Streams closed: drain, `close_streams`, runtime shutdown; open refused for the same reason | `UNAVAILABLE` | `COMPONENT_DRAINING` (`COMPONENT_STOPPED` once stopped) | mid-stream never; a refused open as any open (5.5) |
| Open bulkhead full | `RESOURCE_EXHAUSTED` | `BULKHEAD_FULL` | as for unary calls |
| Open timed out | `DEADLINE_EXCEEDED` | `METHOD_TIMEOUT` (caller side, its own `TIMEOUT`) or none | as for unary calls |
| Caller deadline mid-stream | `DEADLINE_EXCEEDED` | none | never |
| Transport failure mid-stream | `UNAVAILABLE` | `COMPONENT_UNREACHABLE` | never |
| Item too large (`grpc`) | `OUT_OF_RANGE` | none | never |
| Producer panic (`grpc`) | `INTERNAL` | `HANDLER_PANICKED` | never |

The documentation's error table gains `STREAM_EXPIRED` and the stream
meaning of `COMPONENT_DRAINING`.

## 11. Macro

### 11.1 Grammar

```text
#[call( idempotent | timeout = "<humantime>" | bulkhead = <int> | anonymous | optional_end_user
        | stream | max_lifetime = "<humantime>" ,* )]
```

- `stream` requires the return type `Result<CallStream<Item, ItemError>, Error>`:
  the first argument of `Result` is a path whose last segment is
  `CallStream` with exactly two type arguments, no `qself`, and no
  arguments on earlier segments. `Item`, `ItemError` and `Error` follow
  M19 (no lifetimes, no `impl Trait`).
- A method without `stream` whose reply's last path segment is
  `CallStream` is an error.
- `max_lifetime` requires `stream`; it parses like `timeout` (humantime at
  compile time, positive) and expands to `.with_max_lifetime(Duration::new(secs, nanos))`.

### 11.2 Errors

| # | Violation | Message |
|---|---|---|
| M4 (changed) | unknown `#[call]` argument | ``unknown #[call] argument `x`; expected idempotent, timeout, bulkhead, anonymous, optional_end_user, stream or max_lifetime`` |
| M25 | `stream` with another return shape | `a #[call(stream)] method returns Result<CallStream<Item, Error>, Error>` (spanned on the reply) |
| M26 | a `CallStream` reply without `stream` | `a method that returns a CallStream needs #[call(stream)]` (spanned on the reply) |
| M27 | `max_lifetime` without `stream` | `` `max_lifetime` applies to #[call(stream)] methods only `` |
| M28 | bad `max_lifetime` | `max_lifetime must be a positive duration such as "15m" or "1h"` |

Duplicates use the existing duplicate message. Type-level failures are the
const panics of 4.3 and rustc's own (`CallStream` items that are not prost
messages through `assert_wire`).

### 11.3 Expansion

Reference input (insta snapshot `component__stream`; the hand-written
`crates/sekvent-component/tests/support/ticker.rs` is the same expansion
with `proto = "crate::support::ticker::proto"`, hand-written messages, a
hand-written 5-tuple constant and aliases, and a hand-expanded
`TickerError`):

```rust
#[sekvent::component(name = "ticker", package = "shop.ticker.v1", proto = "crate::proto::shop::ticker::v1")]
pub trait Ticker: Send + Sync + 'static {
    /// Ticks of a series, from `from` on, as they happen.
    #[call(stream, idempotent, timeout = "1s", bulkhead = 8, max_lifetime = "10m")]
    async fn ticks(&self, cx: &CallContext, req: TicksRequest)
        -> Result<CallStream<Tick, TickerError>, TickerError>;

    /// The latest tick of a series.
    #[call(idempotent, timeout = "500ms")]
    async fn latest(&self, cx: &CallContext, req: LatestRequest) -> Result<Tick, TickerError>;
}
```

Messages: `TicksRequest { string series = 1; uint64 from = 2; }`,
`LatestRequest { string series = 1; }`, `Tick { string series = 1; uint64 seq = 2; }`
in package `shop.ticker.v1`; `rpc Ticks(TicksRequest) returns (stream Tick)`,
`rpc Latest(LatestRequest) returns (Tick)`.

The trait, `__TickerDyn`, its blanket impl, the handle struct, `binding`,
`install`, `install_with_lifecycle` and the `ComponentHandle` impl follow
C1 3.4 token for token (the return type is used as written). What differs:

```rust
    #[doc(hidden)]
    pub const __METHODS: &'static [::sekvent_component::MethodDescriptor] = &[
        ::sekvent_component::MethodDescriptor::call("ticks", "Ticks")
            .with_stream()
            .with_idempotent()
            .with_timeout(::core::time::Duration::new(1u64, 0u32))
            .with_bulkhead(8u32)
            .with_max_lifetime(::core::time::Duration::new(600u64, 0u32)),
        ::sekvent_component::MethodDescriptor::call("latest", "Latest")
            .with_idempotent()
            .with_timeout(::core::time::Duration::new(0u64, 500000000u32)),
    ];

    /// Ticks of a series, from `from` on, as they happen.
    pub fn ticks<'a>(&'a self, cx: &'a CallContext, req: TicksRequest)
        -> impl ::core::future::Future<Output = Result<CallStream<Tick, TickerError>, TickerError>>
        + ::core::marker::Send + 'a
    {
        self.0.call_stream(0usize, cx, req,
            |imp: ::std::sync::Arc<dyn __TickerDyn>,
             cx: ::sekvent_component::CallContext,
             req: TicksRequest| async move { __TickerDyn::__ticks(&*imp, &cx, req).await })
    }
    // `latest` is C1's `self.0.call(1usize, …)` unchanged.
```

Builder order: `.with_stream()` right after `call(…)`, then `idempotent`,
`timeout`, `bulkhead`, `max_lifetime`, `anonymous`, `optional_end_user`.

```rust
#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::__private::Dispatch for __TickerDispatcher {
    fn dispatch(&self, method: usize, cx: ::sekvent_component::CallContext,
                body: ::sekvent_component::__private::Bytes)
        -> ::sekvent_component::__private::BoxFuture<'static,
            ::core::result::Result<::sekvent_component::__private::Bytes, ::sekvent_component::AppError>>
    {
        let imp = ::std::sync::Arc::clone(&self.0);
        match method {
            1usize => ::sekvent_component::__private::serve(cx, body,
                move |cx: ::sekvent_component::CallContext, req: LatestRequest| async move {
                    __TickerDyn::__latest(&*imp, &cx, req).await
                }),
            _ => ::sekvent_component::__private::unknown_method(
                <TickerHandle as ::sekvent_component::ComponentHandle>::DESCRIPTOR, method),
        }
    }

    fn dispatch_stream(&self, method: usize, cx: ::sekvent_component::CallContext,
                       body: ::sekvent_component::__private::Bytes)
        -> ::sekvent_component::__private::BoxFuture<'static,
            ::core::result::Result<
                ::sekvent_component::CallStream<::sekvent_component::__private::Bytes, ::sekvent_component::AppError>,
                ::sekvent_component::AppError>>
    {
        let imp = ::std::sync::Arc::clone(&self.0);
        match method {
            0usize => ::sekvent_component::__private::serve_stream(cx, body,
                move |cx: ::sekvent_component::CallContext, req: TicksRequest| async move {
                    __TickerDyn::__ticks(&*imp, &cx, req).await
                }),
            _ => ::sekvent_component::__private::unknown_stream_method(
                <TickerHandle as ::sekvent_component::ComponentHandle>::DESCRIPTOR, method),
        }
    }
}

const _: () = {
    ::sekvent_component::__private::assert_wire::<TicksRequest>();
    ::sekvent_component::__private::assert_wire::<Tick>();
    ::sekvent_component::__private::assert_error::<TickerError>();
    ::sekvent_component::__private::assert_error::<TickerError>();
    ::sekvent_component::__private::assert_wire::<LatestRequest>();
    ::sekvent_component::__private::assert_wire::<Tick>();
    ::sekvent_component::__private::assert_error::<TickerError>();
};

const _: () = ::sekvent_component::__private::assert_service(
    crate::proto::shop::ticker::v1::__sekvent_service_Ticker, "shop.ticker.v1.Ticker", &["Ticks", "Latest"]);
const _: () = ::sekvent_component::__private::assert_stream_rpc::<TicksRequest, Tick>(
    crate::proto::shop::ticker::v1::__sekvent_service_Ticker, "Ticks");
const _: () = ::sekvent_component::__private::assert_rpc_types::<TicksRequest, Tick,
    crate::proto::shop::ticker::v1::__sekvent_rpc_Ticker__Ticks>();
const _: () = ::sekvent_component::__private::assert_rpc::<LatestRequest, Tick>(
    crate::proto::shop::ticker::v1::__sekvent_service_Ticker, "Latest");
const _: () = ::sekvent_component::__private::assert_rpc_types::<LatestRequest, Tick,
    crate::proto::shop::ticker::v1::__sekvent_rpc_Ticker__Latest>();
```

Rules behind it:

- `dispatch` holds only unary methods and `dispatch_stream` only streaming
  ones, each with its declaration index. A component without unary methods
  generates no `dispatch`, one without streaming methods no
  `dispatch_stream` (the trait defaults answer `UNIMPLEMENTED`), so no
  unused `imp`, `cx` or `body` appears and **unary-only components expand
  exactly as before** (existing snapshots unchanged).
- Assertions per streaming method: request, item, error, item error, each
  spanned on its type; `local_only` uses `assert_local` for request and item.
- `local_only`: the handle calls `self.0.call_stream_local(…)`; no
  dispatcher, no contract (snapshot `component__local_only_stream`, input
  `#[component(name = "feed", local_only)]` with
  `#[call(stream)] async fn lines(&self, cx: &CallContext, req: String) -> Result<CallStream<String, AppError>, AppError>`).
- `remote_only`: as standard, with `install_remote`.

## 12. Observability

- **Access log.** A served stream already gets exactly one
  `sekvent::access` line, when its response body ends or is dropped, with
  `latency_ms` (the stream's duration), `grpc_status` (from the trailers or
  the gRPC-Web trailer frame) and `aborted` (the client went away). The
  component service puts a `MessageCount` in the response and counts every
  item it hands to the transport, so the line gains `messages`.
- **Stream event.** Where the component runs, under every binding, one
  `debug` event `stream ended` per stream when it ends or is dropped, in the
  call's span (`component_call` for in-process streams, `component_serve`
  when served): `component`, `method`, `binding`, `items`, `duration_ms`,
  `outcome` (`ended`, `failed`, `dropped`, `deadline`, `expired`, `closed`,
  `panicked`), and `code` / `reason` of the terminal error. Never messages
  or values. In-process streams have no access log, as in-process calls
  have none.
- **Spans.** The caller's `component_call` span and the server's
  `component_serve` span are entered for the open and for every poll of the
  stream.
- **Metrics.** sekvent has no metrics hooks; nothing to add.

## 13. `examples/shop`: `Inventory.WatchStock`

Chosen over a notifications or orders stream because inventory is the
component the `split-grpc` profile serves remotely, so the example's
suite runs the stream over gRPC too, not only in process.

- `inventory.proto`: `rpc WatchStock(StockRequest) returns (stream StockReply);`
  (3.1). The trait gains `watch_stock` with
  `#[call(stream, idempotent, timeout = "500ms")]`; an unknown SKU fails
  the open with `InventoryError::UnknownSku`.
- `InventoryService` keeps one `tokio::sync::watch::Sender<u32>` per SKU,
  updated on every reserve and release, and answers with
  `CallStream::new(WatchStream::new(rx).map(…))` (3.3): the current level
  first, then the latest level after each change; a slow watcher skips
  intermediate levels and always ends on the latest. It becomes cheaply
  cloneable (state behind an `Arc`) and gains
  `pub fn watchers(&self, sku: &str) -> usize` so tests can observe
  subscriptions. The `inventory` crate gains `tokio` and
  `tokio-stream = { workspace = true, features = ["sync"] }`.
- The test fakes that implement `Inventory` answer `watch_stock` with
  `Err(InventoryError::Other(AppError::unimplemented(…)))`.
- `examples/shop/contracts/shop.inventory.v1.Inventory.json` is re-emitted
  on the Mac (a compatible change: a new RPC).
- New `shop/tests/streams.rs`, all three profiles (monolith profiles on the
  paused clock, the `split_grpc` twin on the real clock with the 30 s guard,
  C2 7.3):
  - the first item is the current stock; after `place_order` the next item
    is the reduced stock;
  - an unknown SKU is `InventoryError::UnknownSku` from the open, not an
    item;
  - dropping the stream releases the subscription (`watchers` back to 0,
    observed with `await_until!` where it is another task or process);
  - drain: monolith, `app.stop(30 s)` with a stream open ends it with
    `Other(e)` = `UNAVAILABLE` / `COMPONENT_DRAINING` and returns without
    waiting the grace (virtual elapsed zero); split, `service.runtime.shutdown()`
    ends it the same way, `wait()` is `Ok` and the service's ingress unit
    exited normally, not `Aborted`;
  - lifetime: `SEKVENT_COMPONENT_INVENTORY_WATCH_STOCK_STREAM_MAX_LIFETIME`
    of `10s` (monolith: ends between 9 s and 10 s of virtual time) or
    `300ms` (split: at least 270 ms) with `UNAVAILABLE` / `STREAM_EXPIRED`.
- `README.md` of the example: the stream and how it behaves per topology.

## 14. Tests

Rules as in C1 section 6 and C2 section 8: completion signals, paused clock
for in-process timing, sockets on `127.0.0.1:0` with the real clock and a
30 s guard, `await_until!` only for external state, no fixed sleeps. Absence
("the producer is not polled again") is proven on the paused clock with a
`tokio::time::timeout` that can only expire once every task is idle. On the
paused clock, a test that waits on a stream that is not meant to end can
auto-advance into the lifetime; such tests set a long lifetime or expect
`STREAM_EXPIRED`. Every library crate stays at ≥ 95 % lines.

**`sekvent-component`**

- `stream.rs`: each constructor; fused after `Err` and after `None`, with
  the wrapped stream dropped at that moment (drop signal); `next`,
  `is_terminated`, `Debug`.
- `descriptor.rs`: the new builders and getters.
- `contract.rs`: 5-tuple `SERVICE` fixture; `assert_stream_rpc` passing in
  `const` items; `#[should_panic(expected = …)]` for every new message of
  4.3, both functions, client-streaming and bidirectional RPCs.
- `config.rs` / `policy.rs`: `STREAM_MAX_LIFETIME` known only for streaming
  methods and components with one (unknown on a unary method, with a
  suggestion; unknown under `SEKVENT_POLICY_*`); precedence attribute <
  component < method; the default; malformed and zero values name the key
  only; the collision example of section 9.
- `server.rs`: closing fired by `Draining`, `Stopped` and
  `close_streams`; opens refused after closing with the reason per state;
  admission and permit held until the end and released at the end; the
  lifetime draw as a pure function of a unit value (0.0, 0.5, 1.0) bounded
  by 90–100 %.
- `tests/support/ticker.rs` (section 11.3) and fakes: a scripted ticker
  (items fed by the test over a channel, a drop signal, a gate holding the
  open, a panic switch).
- `tests/stream.rs`, every case under `local` and `local-serialized`
  (rstest), paused clock: items and the end; typed open error and typed
  terminal item error with identical values under both bindings; an unknown
  reason decodes to the catch-all variant; dropping the stream drops the
  producer and cancels the stream's token; caller cancellation is
  `CANCELLED` (tagged); caller deadline mid-stream is `DEADLINE_EXCEEDED`
  at exactly the deadline; an open that never completes is `METHOD_TIMEOUT`
  at exactly the `TIMEOUT`, and the open future is dropped; the lifetime is
  `STREAM_EXPIRED` within 90–100 % with `max_lifetime_ms`; the producer's
  context deadline is the stream's, not the `TIMEOUT`; bulkhead 2 counts
  open streams (the third open is `BULKHEAD_FULL`, a new open succeeds after
  one ends) and a queued open waits for a stream to end; hop limit and dead
  contexts rejected before the handler runs; `App::stop(30 s)` ends an open
  stream with `COMPONENT_DRAINING` and returns at once, `on_stop` after it;
  `close_streams` ends streams and refuses opens while unary calls succeed;
  `local` propagates a producer panic, `local-serialized` returns `INTERNAL`
  "component ticker method ticks panicked"; under `local-serialized` the
  producer runs at most 16 items ahead of a consumer that reads nothing;
  garbage request bytes through `dispatch_stream` are `MALFORMED_REQUEST`;
  item bytes that do not decode end the stream with `MALFORMED_REPLY` (a
  dispatcher paired by hand, like `install_with_dispatch`); a non-streaming
  index is `UNIMPLEMENTED`; `local_only` streams.
- `tests/grpc_stream.rs`, loopback: items and the end; typed open and item
  errors identical to the local bindings; the caller dropping the stream
  drops the server's producer and cancels its token; the server App's
  `stop` ends a stream with `COMPONENT_DRAINING`; the caller App's `stop`
  ends an outbound stream with `COMPONENT_DRAINING` (tagged) and resets it
  on the server; a runtime shutdown with a stream open (stage grace 20 s,
  guard 30 s) ends it with `UNAVAILABLE` / `COMPONENT_DRAINING`, `wait()` is
  `Ok` and the ingress unit's exit is not `Aborted`; an idempotent open
  retried after one `UNAVAILABLE`, a non-idempotent one not; six streams
  ending with `UNAVAILABLE` mid-stream leave the breaker (window 4, default
  50 % failure rate) closed while two failed opens then open it; caller `TIMEOUT` on a pending open is
  `METHOD_TIMEOUT` and the server-side drop signal fires; `grpc-timeout`
  ends a stream with `DEADLINE_EXCEEDED` after at least the deadline; the
  server's lifetime ends it with `STREAM_EXPIRED`; a 5 MiB item ends it with
  `OUT_OF_RANGE`; wrong token at the open is `UNAUTHENTICATED` with no
  handler run; hops above the server's limit; nothing listening is
  `COMPONENT_UNREACHABLE` at the open; **interop**: a plain
  `tonic::client::Grpc::server_streaming` with `tonic_prost::ProstCodec`
  reads the items and a typed terminal error from the standard details
  (`tonic-types`).
- `tests/grpc_web_stream.rs`: the runtime server with `.prefix("/api")`,
  CORS and gRPC-Web, `SERVE_AUTH=bearer`; a browser-shaped HTTP/1.1 client
  (`application/grpc-web+proto`, `x-grpc-web: 1`, reading body frames as
  they arrive): the end user reaches the handler; the first item is read
  before the producer is allowed to produce the second (incremental
  delivery); the end is a `0x80` trailer frame with `grpc-status: 0`, and an
  error end carries the status and its details; a missing token is a
  trailers-only `UNAUTHENTICATED` before the handler runs; an `anonymous`
  streaming method works without a token; a client that disconnects lets an
  endless producer be dropped.
- `ComponentService` called directly as a tower service: a streaming
  response carries a `MessageCount` equal to the items once the body is
  consumed; open errors are trailers-only without one.

**`sekvent-telemetry`:** `messages` logged from a `MessageCount` in the
response, the value read at completion (count after the headers), absent
without one.

**`sekvent-proto-build`:** the render tests with both flags (`(…, false,
true)` for server streaming, `(…, true, false)` for client streaming);
`tests/compile.rs` expectations in the 5-tuple.

**`sekvent-macros`** (excluded from coverage): parse tests for `stream`,
`max_lifetime` and both return-shape errors; snapshots
`component__stream` and `component__local_only_stream`; trybuild pass
`stream.rs` (standard; runs under `local` and `local-serialized`) and
`stream_local_only.rs`; trybuild fail `m25_stream_without_call_stream`,
`m26_call_stream_without_stream`, `m27_max_lifetime_without_stream`,
`m28_bad_max_lifetime`, `p5_streaming_rpc` (new message),
`p13_stream_method_on_unary_rpc`, `p14_client_streaming_rpc`; every
existing case's constant in the 5-tuple. `.snap` and `.stderr` files are
generated on the Mac.

**`sekvent-facade-check`:** its `Echo` component gains
`#[call(stream)] pings` and its `notes` component a `local_only` stream,
both consumed through the facade under `local` and `local-serialized`;
the constant in the 5-tuple.

**`examples/shop`:** section 13 (excluded from coverage, part of every
gate).

## 15. Docs and skills to update

Owned by the implementation's docs agent (section 16); listed here because
these files are being edited elsewhere right now.

- `docs/component-model.md`: status line (streams, link to this note);
  method rules (`#[call(stream)]` returns
  `Result<CallStream<Item, E>, E>`); method kinds (a note that a stream is a
  `#[call]` variant); attributes (`stream`, `max_lifetime`); contracts
  (`returns (stream …)` maps to `#[call(stream)]`, client and bidirectional
  streaming are rejected); bindings (streams under each); resilience (opens
  only; bulkheads count open streams); **non-goals: "Streaming RPCs"
  becomes "Client-streaming and bidirectional RPCs (server streaming is
  `#[call(stream)]`; browsers over gRPC-Web can only do server streaming)"**.
- `docs/modules/components.md`: Concepts (contract and binding tables say
  unary or server-streaming); trait rules; `#[call(...)]` options (`stream`,
  `max_lifetime`); a new "How to stream replies (`#[call(stream)]`)"
  section (declare, implement with `CallStream`, consume, reopen and
  resume, lifetime, drain and shutdown, browsers); resilience (open-only
  semantics); configuration keys (`STREAM_MAX_LIFETIME`); error table
  (`STREAM_EXPIRED`, `COMPONENT_DRAINING` for streams); testing (paused
  clock and the lifetime); pitfalls (unpolled streams, HTTP/1.1 connection
  slots, proxy idle timeouts, credentials not re-checked, `close_streams`
  outside the runtime).
- `docs/modules/proto-build.md`: the 5-tuple constant and its example.
- `docs/modules/telemetry.md` and `docs/modules/server.md`: `messages` in
  the access log and `MessageCount`; server.md also: long-lived streams and
  graceful shutdown, `max_connections` and HTTP/1.1.
- `docs/modules/runtime.md`: where the shutdown sequence is described,
  that `App::register` closes component streams when shutdown begins.
- `docs/features.md`: the `component` and `component-grpc` rows mention
  server-streaming methods.
- `README.md`: the component bullet mentions server streaming.
- `AGENTS.md`, section Components: a bullet "Methods are unary or
  server-streaming (`#[call(stream)]`, `Result<CallStream<Item, E>, E>`);
  client and bidirectional streaming are rejected at compile time. A
  stream's open is a call (`TIMEOUT`, open retries, breaker, hop limit,
  auth); after it, the caller's deadline, `STREAM_MAX_LIFETIME` (15 min),
  the consumer and the gate end it; it holds its admission and bulkhead
  permit for its life; `App::register` closes streams when shutdown
  begins"; the `TIMEOUT` bullet says "for a stream, the open"; the serving
  bullet says items are capped at 4 MiB when served; the crate-map row of
  `sekvent-component` adds `telemetry (optional)`.
- `skills/sekvent/SKILL.md`: a streaming recipe (proto, trait,
  implementation, consumer loop, keys); `skills/sekvent-api/SKILL.md`: a
  task-map row "stream live updates to a browser or service";
  `skills/sekvent-migrate/SKILL.md`: a mapping row "hand-written
  server-streaming tonic service → `#[call(stream)]` method".

Design notes (C1, C2, end user) stay as written; this note records the
change.

## 16. Work split

Phase 1: four agents write code and tests and **do not build, lint or run
tests**. Phase 2 (parent, on the Mac): `cargo metadata --format-version 1 >
/dev/null` (new dependencies of `sekvent-component` and `inventory`);
`INSTA_UPDATE=always cargo test -p sekvent-macros --lib`;
`TRYBUILD=overwrite cargo test -p sekvent-macros --test ui`;
`(cd examples/shop && cargo run -p cargo-sekvent -- sekvent contract emit)`;
review every regenerated file; then `rrb run gate` and `rrb run coverage`
once over the merged tree. Phase 3: fixes by file owner.

| Agent | Writes (disjoint) | May assume |
|---|---|---|
| **A — component runtime** | `crates/sekvent-component/**` (manifest; `src/stream.rs` new; `lib.rs`, `descriptor.rs`, `reasons.rs`, `contract.rs`, `__private.rs`, `server.rs`, `link.rs`, `app.rs`, `runtime.rs`, `config.rs`, `policy.rs`, `wire.rs`, `grpc/client.rs`, `grpc/service.rs`; `tests/**`: `support/ticker.rs` new, fakes, `stream.rs`, `grpc_stream.rs`, `grpc_web_stream.rs`, and the 5-tuple in `support/inventory.rs` and `grpc_end_user.rs`); `crates/sekvent/src/lib.rs` (prelude `CallStream`) | `MessageCount` exactly as 8.3 (C); the 5-tuple constants (C); the macro emits exactly 11.3 (B) |
| **B — macros and facade check** | `crates/sekvent-macros/**` except generated `.snap` / `.stderr` (parse, expand, unit tests, snapshot tests, trybuild cases incl. the 5-tuple in every existing case); `crates/sekvent-facade-check/**` | the `__private` items and public API of 8.1–8.2 (A) |
| **C — proto-build, telemetry, server keep-alive** | `crates/sekvent-proto-build/**` (5-tuple emission, tests, `tests/compile.rs`); `crates/sekvent-telemetry/**` (`MessageCount`, the `messages` field, tests); `crates/sekvent-runtime/src/server.rs` and a new `crates/sekvent-runtime/tests/server_keep_alive.rs` (7.1) | nothing from the others |
| **D — example and docs** | `examples/**` except `examples/shop/contracts/*.json`; `docs/**` except `docs/design/**`; `README.md`; `AGENTS.md`; `skills/**` | sections 3–13 exactly |

Couplings to watch: A's `tests/support/ticker.rs` must match B's
`component__stream` snapshot token for token (apart from the `proto` path);
every hand-written `__sekvent_service_*` constant changes shape in the same
merge (A, B, C each in their files; D in `docs/modules/proto-build.md`);
A's gRPC service uses C's `MessageCount`. Every agent reports, besides its
summary, **anything in this note that contradicted the tree or could not be
implemented as written**, with file and reason, and stays inside its write
set.

## 17. Decisions the user might override, risks, open questions

### Decisions the user might override

1. **`#[call(stream)]` + `Result<CallStream<Item, ItemError>, Error>`**:
   a boxed named stream type and a flag on `#[call]` (3.5), with separate
   open and item error types allowed. Alternative: `impl Stream` in the
   trait, or a new `#[stream]` kind.
2. **`TIMEOUT` bounds the open only**, and the open is the open reply
   (response headers), not the first item. Alternative: time to first item.
3. **The bulkhead counts open streams for their whole life.** Alternative:
   count opens only (unbounded concurrent streams).
4. **Shutdown closes streams at its start** (`App::register` on
   `ShutdownTrigger::triggered`), refusing new streams during
   `shutdown_delay`; `App::stop` closes a component's streams when it
   drains. Alternative: close at the end of the delay (needs a runtime hook,
   open question 2).
5. **No re-check of credentials during a stream**; a finite default
   lifetime of 15 minutes, jittered 90–100 %, ending with `UNAVAILABLE` /
   `STREAM_EXPIRED` so clients reopen at once and re-authenticate.
6. **No idle timeout.** Quiet streams are the norm for live updates, so an
   idle timeout would end healthy streams; dead peers are bounded by the
   lifetime; liveness through proxies needs heartbeats or HTTP/2
   keep-alive, which an idle timeout does not give.
7. **Item errors are never converted to `DOWNSTREAM_FAILURE`**, open errors
   are (5.6).
8. **`stream` is allowed on `local_only`** components (plain Rust items),
   keeping the on-ramp complete.
9. **Item counts in the access log** through a `MessageCount` response
   extension (`sekvent-component` gains an optional dependency on
   `sekvent-telemetry`), rather than parsing gRPC frames in the access log.
10. **The service constant becomes a 5-tuple** (4.2) rather than a second
    per-RPC constant, at the cost of touching every hand-written constant.

### Risks

- **Undetected dead peers.** Without `http2_keep_alive` (7.1, off by
  default) and over HTTP/1.1, hyper notices a closed connection only when it
  writes, so a quiet stream to a vanished client lives until its lifetime or
  the next heartbeat item. The default lifetime of 15 minutes bounds it.
- **Proxy idle timeouts** cut quiet gRPC-Web streams; applications need
  heartbeats or reopen logic, which the docs must say plainly.
- **Browser connection limits** over HTTP/1.1 (one connection per stream).
- **tonic behaviour this design relies on**, verified in 0.14.6 and pinned
  by tests: the client's `server_streaming` resolves at the response
  headers and turns a trailers-only answer into `Err`; `GrpcTimeout` bounds
  only the response future; the server's `EncodeBody` ends the body with
  trailers at the first `Err` item; hyper drops a response body when the
  client resets its HTTP/2 stream.
- **Churn of the 5-tuple** across about 25 files and every trybuild case,
  in one merge.
- **Paused-clock tests** can auto-advance into a stream's lifetime when
  every task waits; tests set long lifetimes or expect the expiry.
- **Unpolled streams** hold a drain for the grace (5.4).

### Answered by the user (October 2026)

1. Default lifetime: **15 minutes** (jittered 90–100 %).
2. Streams close **when shutdown begins**; no runtime hook for the end of
   `shutdown_delay`.
3. Credential expiry during a stream: **rely on the lifetime** for now;
   ending at `EndUser` expiry may come later.
4. **HTTP/2 keep-alive** becomes an opt-in `ServerBuilder` setting (7.1);
   applications send **heartbeat items** for HTTP/1.1 proxies.
5. **No idle timeout** (decision 6 stands).
