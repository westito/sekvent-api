# Component model — milestone C2 implementation spec

> Status: design for review. Builds on
> [`component-c1.md`](component-c1.md) (implemented, commit 6f8c7bd) and
> [`docs/component-model.md`](../component-model.md). Where this document is
> more specific, it wins for C2; everything C1 specifies and this document
> does not change stays as it is.

## 1. Scope

### In C2

- **`grpc` binding** behind a new `grpc` feature of `sekvent-component`
  (facade feature `component-grpc`). One generic, byte-level tonic client and
  one generic, byte-level tonic service carry every component: both reuse
  C1's `Dispatch` (`dispatch(method, cx, body) -> bytes`) and a pass-through
  `Bytes` codec, routed by the standard gRPC path
  `/<package>.<Trait>/<Rpc>`. **No per-component tonic code is generated.**
  The wire is plain gRPC, so a client generated from the component's
  `.proto` by any toolchain can call a sekvent server (section 2).
- **Serving:** a locally bound component is exposed over gRPC by
  configuration (`SEKVENT_COMPONENT_<C>_SERVE=grpc`); `App::grpc_routes()`
  returns tonic `Routes` for every exposed component, which join the
  process's `sekvent-runtime` server next to any other tonic service.
  Per-component `grpc.health.v1` status follows the component's lifecycle.
- **Link authentication**, fail-closed in both directions: a `grpc` binding
  presents the outbound token of its link (`SEKVENT_LINK_OUTBOUND_<LINK>`)
  unless `SEKVENT_COMPONENT_<C>_AUTH=none`; an exposed component accepts
  only the process's inbound tokens (`SEKVENT_LINK_INBOUND_*`) unless
  `SEKVENT_COMPONENT_<C>_SERVE_AUTH=none`. The callee's caller is the
  authenticated link, trusted only when `SEKVENT_LINK_TRUSTED` names it;
  subject and tenant are dropped otherwise. No token may serve two purposes
  in one process.
- **`remote_only`** components become buildable (bound `grpc`).
- **Resilience on remote bindings:** budgeted retry of `idempotent` methods
  only, deadline-aware and capped by `Retry-After`; one circuit breaker per
  remote component (its endpoint); retries on by default for remote
  bindings. **Named policies** (`SEKVENT_POLICY_<NAME>_*`) and **bulkhead
  queues** land; precedence is framework default → attribute → named policy
  → component key → method key, using `PolicySpec` layering.
- **Call-hop limit** (deferred in C1): `CallContext` carries a hop count,
  the header codec carries `x-sekvent-hops`, and every binding rejects a
  call deeper than `SEKVENT_COMPONENT_MAX_HOPS` (default 16).
- **Telemetry spans** (deferred in C1): one `debug`-level span per call on
  the caller side and per served gRPC call; no metrics.
- **Contracts:** the `-api` crate's `.proto` file declares the component's
  `service`; the `#[component]` macro gains `proto = "<module path>"` and
  checks at compile time that the trait and the proto service agree (names,
  RPCs, request and reply types); `sekvent-proto-build` emits the constant
  the check reads. `cargo sekvent contract emit | check` compiles the protos
  in-process (pure-Rust `protox`, no `protoc`, no Rust build), writes
  canonical JSON baselines and applies an in-house wire-compatibility
  checker; `[contract]` in `sekvent.toml` enables it and adds a gate step.
- **`examples/shop` split-grpc profile:** a new `inventory-svc` crate (lib +
  thin binary) hosts inventory; the `shop` binary binds it `grpc`. The C1
  suite runs under `monolith-local`, `monolith-serialized` and
  `split-grpc`, plus remote fault injection (section 7).

### Out of scope (later)

| Item | Note |
|---|---|
| TLS on component endpoints | `https://` endpoints are a build error naming the key; C2 assumes a private network or a TLS-terminating mesh (decision 3) |
| Per-component caller allow-lists | every configured inbound link may call every exposed component |
| Compression, message-size keys, keep-alive keys | a fixed 4 MiB request-message limit when serving, fixed keep-alive |
| Per-attempt timeouts, rate limits for components | `TIMEOUT` stays the whole call's deadline; `RATE_LIMIT_*` keys are unknown keys |
| Shared component-wide bulkhead | component keys remain per-method defaults |
| Several endpoints per component, client-side balancing | one URL per component; a load balancer or DNS in front |
| Error reasons in contracts | reasons live in Rust (`ComponentError`); `contract check` sees only protos (decision 6) |
| Multi-version tooling | `billing.v2` alongside `billing.v1` is two components; nothing new is needed (6.6) |
| Proto editions | `contract` rejects editions files (extensions are recorded and checked, 6.3/6.4) |
| Metrics, streaming, `#[async_call]`, `#[deferred]` | C3 and later |

## 2. Wire contract

### 2.1 Decision: one generic byte-level service and client

The generic approach wins over per-component tonic codegen in the macro:

- C1 already produces, per component, a byte-level `Dispatch` and prost
  encoding on the caller side; gRPC only adds framing, metadata and status
  mapping, which are the same for every component.
- tonic's `Codec` trait makes a pass-through codec trivial
  (`Encode = Decode = Bytes`), and `tonic::server::Grpc::unary` /
  `tonic::client::Grpc::unary` then provide standard framing, trailers and
  `grpc-status` handling. A dynamic service name is mounted with
  `axum::Router::route_service("/<service>/{*rest}", …)` wrapped in
  `tonic::service::Routes::from(router)` — `NamedService` (a `const NAME`)
  is not needed.
- The macro output does not grow, generated code never names tonic, and
  `-api` crates stay free of tonic (C1 decision). The cost — one `Bytes`
  copy per message and a path lookup per call — is negligible next to the
  network hop.

Private codec (in `src/grpc/codec.rs`): `BytesCodec` implements
`tonic::codec::{Codec, Encoder, Decoder}`; `encode` puts the bytes into the
`EncodeBuf`, `decode` returns `src.copy_to_bytes(src.remaining())`.

### 2.2 Requests

`POST /<package>.<Trait>/<Rpc>` over HTTP/2 (prior knowledge, plaintext),
for example `/shop.inventory.v1.Inventory/Reserve`, where `<package>` is the
component's `package`, `<Trait>` the trait name and `<Rpc>` the method's
UpperCamelCase RPC name (C1 `MethodDescriptor::rpc`). Body: one gRPC
length-prefixed, uncompressed message holding the prost encoding of the
request. Headers, written by the caller for **every attempt**:

| Header | Value | Serving side |
|---|---|---|
| `content-type`, `te` | `application/grpc`, `trailers` (tonic) | tonic |
| `authorization` | `Bearer <token>` of `SEKVENT_LINK_OUTBOUND_<LINK>` (`BearerInjector::apply`); absent with `AUTH=none` | `sekvent_link::authenticate_headers` against the process's inbound `TokenMap` (2.4) |
| `x-request-id`, `traceparent`, `x-sekvent-subject`, `x-sekvent-tenant`, `idempotency-key` | `sekvent_context::headers::inject(&callee_cx, …)` (C1 semantics: the key set for this call is forwarded, identical on every retry; a key the caller only received is not) | `headers::from_headers(&headers, caller)`; subject and tenant survive only for a trusted caller |
| `grpc-timeout` | the callee context's time left at the start of the attempt, truncated (`inject`) | the context deadline, anchored at receipt, then narrowed by the serving side's method timeout |
| `x-sekvent-hops` (new) | the callee's hop count, 1 for a first-level call | rejected when above the server's `SEKVENT_COMPONENT_MAX_HOPS` |

No other metadata is sent. The caller never sends `x-sekvent-*` identity it
does not have, and the server never trusts a `CallContext` found in the
request extensions (the runtime server's own authenticator may be about end
users); it always builds the context itself.

### 2.3 Responses and errors

Success: `grpc-status: 0` with one message holding the prost encoding of the
reply. Failure: `sekvent_error::grpc::to_status(&AppError)` — the code, the
message as `grpc-message`, and a standard `google.rpc.Status` in
`grpc-status-details-bin` carrying `ErrorInfo { reason, domain, metadata }`,
`RetryInfo { retry_delay }` and `BadRequest` field violations. The internal
source never leaves the process. The caller decodes with
`sekvent_error::grpc::from_status` → `AppError` → `E::from_app_error`, so a
typed error (C1 3.7) survives the hop exactly as under `local-serialized`.

Caller-side mapping of the `Status` returned by `tonic::client::Grpc::unary`:

| Status | Becomes |
|---|---|
| has `tonic::TimeoutExpired` in its source chain: tonic's channel enforcing the injected `grpc-timeout` | `DEADLINE_EXCEEDED`, message `the call did not complete in time`, the status as internal source, tagged `component`/`method` — the same code the context deadline yields under `local` |
| otherwise has an underlying error (`std::error::Error::source` is `Some`): produced by the client's own transport (connect refused, reset, GOAWAY, h2 error) | `UNAVAILABLE`, reason `COMPONENT_UNREACHABLE`, message `component <c> is unreachable`, the status as internal source, tagged `component`/`method` |
| anything else (decoded from trailers, or inferred from a non-gRPC HTTP status) | `from_status(&status)`, untouched |

The method's own `TIMEOUT` is tracked apart from the caller's deadline:
when it expires while the caller still has time, the call is
`DEADLINE_EXCEEDED` with reason `METHOD_TIMEOUT` and counts as a failure of
the callee in its circuit breaker, which `RemoteClient` drives itself. The
caller's own deadline or cancellation says nothing about the callee: the
breaker permit is returned unrecorded.

A transient failure (`UNAVAILABLE`, `RESOURCE_EXHAUSTED`, `METHOD_TIMEOUT`)
of a call made from inside a handler (hop > 0) is marked with metadata
`downstream = <callee>`; a marker set further down is kept. The serving
side (2.4) turns such an error into `INTERNAL` / `DOWNSTREAM_FAILURE`
(keeping `downstream`, with the original code as `downstream_code` and
reason as `downstream_reason`), so callers above neither retry the
healthy component in between nor count the failure against it.

Only errors the caller side creates are tagged (C1 2.5 rule 4); errors from
the server keep exactly the metadata the server sent, so `local`,
`local-serialized` and `grpc` return identical values for the same
server-side outcome. A reply that does not decode as `Rep` is `INTERNAL` /
`MALFORMED_REPLY` (C1).

### 2.4 Serving side, in order

The generic service (`src/grpc/service.rs`) receives
`http::Request<B>` for any body `B: http_body::Body<Data = Bytes>`.
Steps 1–3 read only the request headers, so a call is refused before a
byte of its body is read; request trailers never reach the context.

1. **Route.** The router only reaches the service for an exposed
   component's path prefix; an unknown service path gets tonic's
   `UNIMPLEMENTED`. The path must be exactly `/<service>/<Rpc>`, with
   `<Rpc>` one of the component's methods by `rpc()`; anything else →
   `UNIMPLEMENTED`, reason `UNKNOWN_METHOD`, tagged `component`, before
   authentication (the path is public anyway).
2. **Authenticate** (unless `SERVE_AUTH=none`):
   `authenticate_headers(&inbound_map, request.headers())`; `None` →
   `UNAUTHENTICATED` with `sekvent_link::REJECTED_MESSAGE`, no reason, no
   metadata — the same answer for a missing, malformed or unknown token.
   With `SERVE_AUTH=none` the caller is `None` (anonymous, untrusted).
3. **Context.** `cx = headers::from_headers(&headers, identity)`, which keeps
   subject and tenant only for a trusted link; hop check; the deadline is
   narrowed by the served method's resolved timeout (tokio clock, as C1
   `callee_context`); a drop guard cancels `cx`'s token when the response
   future is dropped (the client reset the stream or went away).
4. **Serve** through the component's existing `Server::run(method, cx, …)`
   around `dispatch.dispatch(method, cx, body)` — admission gate (draining →
   `UNAVAILABLE` / `COMPONENT_DRAINING`), dead-call shedding, bulkhead,
   deadline — exactly the path `local-serialized` uses. Request bytes that do
   not decode are `INVALID_ARGUMENT` / `MALFORMED_REQUEST` (C1 `serve`).
5. **Answer** `Ok(bytes)` as the reply, `Err(e)` as `to_status(&e)`. A
   panicking method is `INTERNAL` / `HANDLER_PANICKED` (payload never
   shown); an error carrying `downstream` metadata becomes `INTERNAL` /
   `DOWNSTREAM_FAILURE` (2.3); every other error crosses unchanged.

The body is decoded inside `tonic::server::Grpc::new(BytesCodec).unary(…)`
with a 4 MiB message limit, so malformed framing, unsupported compression
and oversized messages get tonic's standard answers.

### 2.5 Interoperability

A client generated from the component's `.proto` (whose `service` block the
macro checks against the trait, section 5) calls a sekvent server with the
same paths and messages; it must send `authorization: Bearer <token>` for a
configured inbound link unless the component is served with
`SERVE_AUTH=none`, and may send `grpc-timeout`, `x-request-id`,
`traceparent` and `idempotency-key`. It reads errors from the standard
`google.rpc.Status` details. Conversely, a sekvent `grpc` binding can call a
non-sekvent server implementing the same service. Server reflection is not
offered in C2.

## 3. Public API changes

Everything is additive; C1 signatures are unchanged unless stated.

### 3.1 `sekvent-component`

Manifest:

```toml
[features]
default = ["macros"]
macros = ["dep:sekvent-macros"]
runtime = ["dep:sekvent-runtime"]
# The `grpc` binding and serving components over gRPC.
grpc = ["dep:sekvent-link", "dep:tonic", "dep:axum", "dep:http-body", "dep:tower", "sekvent-error/grpc"]

[dependencies]            # additions; all optional, all { workspace = true }
sekvent-link, tonic, axum, http-body, tower

[dev-dependencies]
sekvent-component = { path = ".", features = ["runtime", "grpc"] }
tonic-prost = { workspace = true }    # the interop client (section 8)
tonic-types = { workspace = true }    # reading standard error details in the interop test
tonic-health = { workspace = true }   # health client in the serving tests
```

Crate map row (AGENTS.md): "components, App builder, local, serialized and
gRPC bindings" — depends on config, error, context, resilience, link
(optional), macros, runtime (optional). `link` sits below `component`, so
the DAG gains no cycle.

New and changed items:

```rust
// lib.rs
pub const MAX_HOPS_KEY: &str = "SEKVENT_COMPONENT_MAX_HOPS";
pub const DEFAULT_MAX_HOPS: u32 = 16;
/// Prefix of named policy keys, `SEKVENT_POLICY_<NAME>_<FIELD>`.
pub const POLICY_PREFIX: &str = "SEKVENT_POLICY_";

// app.rs
impl App {
    /// Full gRPC service names of the components exposed over gRPC
    /// (`SEKVENT_COMPONENT_<C>_SERVE=grpc`), in install order. Empty without
    /// the `grpc` feature.
    pub fn grpc_services(&self) -> Vec<String>;
    /// tonic routes serving every exposed component, for
    /// `sekvent_runtime::ServerBuilder::grpc_routes` (or tonic's own server).
    /// Empty when nothing is exposed. Calling it marks the routes mounted.
    #[cfg(feature = "grpc")]
    pub fn grpc_routes(&self) -> tonic::service::Routes;
}
// App::start: fails with FAILED_PRECONDITION / GRPC_NOT_MOUNTED (naming the
// component and its SERVE key) when a component is exposed and grpc_routes()
// was never called, before any component starts.
// App::register (feature runtime): additionally keeps runtime.health() in step
// for every name in grpc_services(): NotServing until start() succeeds,
// Serving while running, NotServing before stop().

// error.rs — BuildError gains one variant
#[error("component {component} cannot be served over gRPC ({key}): {reason}")]
NotServable { component: String, key: String, reason: &'static str },
// reasons: "it is local_only", "it is not bound locally",
// "this build has no gRPC support; enable the grpc feature".
// BindingUnavailable (unchanged text) now only fires without the feature.

// reasons.rs — additions
pub const CIRCUIT_OPEN: &str = "CIRCUIT_OPEN";               // set by sekvent-resilience
pub const UNREACHABLE: &str = "COMPONENT_UNREACHABLE";
pub const CALL_DEPTH_EXCEEDED: &str = "CALL_DEPTH_EXCEEDED";
pub const UNKNOWN_METHOD: &str = "UNKNOWN_METHOD";
pub const GRPC_NOT_MOUNTED: &str = "GRPC_NOT_MOUNTED";
```

`__private` additions (the contract with the macro, section 5):

```rust
/// One RPC as sekvent-proto-build emits it: (RPC name, request full name,
/// reply full name, streaming).
pub type ProtoRpc = (&'static str, &'static str, &'static str, bool);
/// A proto service as sekvent-proto-build emits it: (full name, RPCs).
pub type ProtoService = (&'static str, &'static [ProtoRpc]);

#[diagnostic::on_unimplemented(
    message = "`{Self}` has no protobuf type name",
    note = "compile the messages with sekvent-proto-build, which enables prost's type names, or implement `prost::Name`")]
pub trait ContractMessage: WireMessage + prost::Name {}
impl<T: WireMessage + prost::Name> ContractMessage for T {}

/// Const-panics unless `proto.0 == service`, the trait's RPC names `rpcs`
/// are distinct, and the proto declares each of its RPCs once and only RPCs
/// in `rpcs`.
pub const fn assert_service(proto: ProtoService, service: &str, rpcs: &[&str]);
/// Const-panics unless `proto` has a non-streaming RPC `rpc` whose request is
/// `Req` and whose reply is `Rep` (full names from `prost::Name`; an empty
/// `PACKAGE` means the bare `NAME`; `()` is `google.protobuf.Empty`).
pub const fn assert_rpc<Req: ContractMessage, Rep: ContractMessage>(proto: ProtoService, rpc: &str);
/// Fails to compile unless `Req` and `Rep` are the very types of `Rpc`
/// (`__sekvent_rpc_<Service>__<Rpc> = (Req, Rep)`, emitted by
/// sekvent-proto-build): names alone cannot tell a nested message from a
/// top-level one of the same leaf name.
pub const fn assert_rpc_types<Req, Rep, Rpc>();
```

Const-panic messages (exact; rustc shows them as
`evaluation of constant value failed`, spanned on the generated `const`):

| Check | Message |
|---|---|
| service name | `component contract: the proto service is not <package>.<Trait>; check package and proto` |
| RPC count | `component contract: the proto service and the trait declare different sets of RPCs` |
| missing RPC | `component contract: the proto service has no RPC named after this method` |
| streaming | `component contract: the RPC named after this method is streaming; component methods are unary` |
| request | `component contract: this method's request type is not the RPC's input type` |
| reply | `component contract: this method's reply type is not the RPC's output type` |

Internal layout (new files; `grpc/**` is `#[cfg(feature = "grpc")]`):

```text
src/policy.rs        PolicySpec layering for components (4.3), per-method and per-component results
src/contract.rs      assert_service / assert_rpc, const byte-wise string comparison
src/grpc/mod.rs      Remote route type, channel cache keyed by endpoint URL
src/grpc/codec.rs    BytesCodec (2.1)
src/grpc/endpoint.rs ENDPOINT validation → tonic::transport::Endpoint (4.2)
src/grpc/client.rs   RemoteClient: lazy channel, per-method path + Policy, auth, attempt()
src/grpc/service.rs  ComponentService (2.4) and App::grpc_routes
tests/grpc_*.rs      loopback tests (section 8)
```

Changes to C1 internals, fixed here so the macro and tests can rely on them:

- `Route<D>` gains `#[cfg(feature = "grpc")] Remote(Arc<grpc::RemoteClient>)`.
  Every installed component keeps a `Server` (its gate): for a remote
  binding it has no bulkheads, admits outbound calls only while the App runs,
  and `App::stop` drains in-flight outbound calls within the grace, so
  `App::state`, `NOT_STARTED` / `DRAINING` / `STOPPED` rejections and drain
  behave the same under every binding. A remote component has no factory and
  no lifecycle hooks (C1).
- `invoke` (C1 2.5) adds, after dead-call shedding: `hops = cx.hops() + 1`;
  `hops > max_hops` → `FAILED_PRECONDITION` / `CALL_DEPTH_EXCEEDED` (tagged,
  nothing sent); the callee context carries `with_hops(hops)`. The whole
  call runs in `tracing::debug_span!("component_call", component, method,
  binding)`.
- Remote pipeline: `server.run(method, callee, |cx| client.call(method, cx, body))`
  then `Rep::decode`, where `RemoteClient::call` is
  `policies[method].call(&cx, descriptor.methods()[method].is_idempotent(), || attempt(…))`
  with `Policy::new("<component>.<method>")`, `Timeout::deadline_only()`, the
  method's `RetryPolicy` (if any) and the component's shared
  `Arc<CircuitBreaker>` (if any). Each attempt builds a fresh `HeaderMap`
  (`headers::inject`, then the bearer), awaits `Grpc::ready()` (an error is
  `COMPONENT_UNREACHABLE`) and calls `unary(…, path, BytesCodec)`; the whole
  pipeline stays raced with `cx.cancelled()` inside C1's `invoke`, and
  dropping it resets the HTTP/2 stream, which drops the server's handler.
- Channels: `tonic::transport::Endpoint::from_shared(url)` with
  `connect_timeout(5 s)`, `tcp_nodelay(true)`,
  `http2_keep_alive_interval(30 s)`, `keep_alive_timeout(10 s)`, built in
  `build()` (sync, no I/O); `connect_lazy()` runs on the first call (it
  needs a tokio runtime, `build()` does not). Components with the same
  endpoint URL share one channel.
- Breaker state changes are logged: `warn!` when a component's breaker
  opens, `info!` when it closes (names only).
- Serving-side bulkheads come from `build_bulkhead()` of the method's
  resolved spec, queue settings included (C1 built them with
  `Bulkhead::new(n)` directly).
- Errors made by the remote pipeline itself (breaker open,
  `COMPONENT_UNREACHABLE`) are tagged like every caller-made error; a retry
  that gives up returns the last server error untouched.

### 3.2 Other crates (additive only)

**`sekvent-context`**

```rust
impl CallContext {
    /// How many component calls led to this one (0 at the edge).
    pub fn hops(&self) -> u32;
    #[must_use] pub fn with_hops(self, hops: u32) -> Self;
}   // new() starts at 0; child() and detached() keep the count
pub mod headers {
    /// Component call depth.
    pub const HOPS: &str = "x-sekvent-hops";
}
```

`from_headers` reads `HOPS` as 1–4 ASCII digits (anything else is ignored,
leaving 0) from any caller — it is a safety net, not identity; `inject`
writes it when the count is above 0 and removes a stale header otherwise;
`propagate` writes it only when above 0 and absent.

**`sekvent-link`**

```rust
impl LinkConfig {
    /// Fail when two link keys (inbound or outbound) hold the same token;
    /// `LinkError::DuplicateToken { first, second }` names them as
    /// `inbound/<link>` or `outbound/<link>`, never the token. Constant-time
    /// comparisons, like `validate_unique`.
    pub fn check_distinct_tokens(&self) -> Result<(), LinkError>;
}
/// `SEKVENT_LINK_INBOUND_<LINK>` (upper-cased).
pub fn inbound_key(link: &str) -> String;
/// `SEKVENT_LINK_OUTBOUND_<LINK>` (upper-cased).
pub fn outbound_key(link: &str) -> String;
```

`BearerInjector` keeps its token privately (a `Secret`) for the comparison.

**`sekvent-resilience`**

```rust
impl PolicySpec {
    /// Retries alone; `None` unless `retry_max_attempts` is above 1.
    pub fn build_retry(&self) -> Result<Option<RetryPolicy>, PolicyError>;
    /// The bulkhead alone; `None` without `bulkhead_max_concurrent`.
    pub fn build_bulkhead(&self) -> Result<Option<Bulkhead>, PolicyError>;
    /// The breaker alone, named `name`; `None` unless enabled.
    pub fn build_breaker(&self, name: impl Into<String>) -> Result<Option<CircuitBreaker>, PolicyError>;
}
```

`build` is rewritten on top of the three with identical behaviour and
unchanged tests.

**`sekvent-config`:** `"SEKVENT_POLICY_"` joins `FRAMEWORK_PREFIXES`
(between the link prefixes and `SEKVENT_TEST_`; the list stays sorted), with
the doc bullet "`SEKVENT_POLICY_`: named resilience policies
(`sekvent-component` validates them against the policies components
reference)" and an `is_framework_key("SEKVENT_POLICY_REMOTE_TIMEOUT")`
assertion.

**`sekvent-proto-build`:** a built-in service generator, first in the hook
chain, active in `messages_only()` and `both()` (not `services_only()`, whose
messages crate already has it):

```rust
pub const SERVICE_CONTRACT_PREFIX: &str = "__sekvent_service_";
impl ProtoBuild {
    /// Emit the service constants `#[component(proto = …)]` checks (default on).
    pub fn service_contracts(self, enable: bool) -> Self;
}
```

For every proto `service` it appends to the package's generated file, from
`prost_build::Service` (`proto_name`, `package`, and per method `proto_name`,
`input_proto_type` / `output_proto_type` without the leading dot,
`client_streaming || server_streaming`):

```rust
/// Contract of `shop.inventory.v1.Inventory`, checked by `#[component(proto = …)]`.
#[doc(hidden)]
#[allow(non_upper_case_globals, dead_code)]
pub const __sekvent_service_Inventory: (&str, &[(&str, &str, &str, bool)]) = (
    "shop.inventory.v1.Inventory",
    &[
        ("Reserve", "shop.inventory.v1.ReserveRequest", "shop.inventory.v1.ReserveReply", false),
        ("Release", "shop.inventory.v1.ReleaseRequest", "shop.inventory.v1.ReleaseReply", false),
        ("Stock", "shop.inventory.v1.StockRequest", "shop.inventory.v1.StockReply", false),
    ],
);
```

The generated code names no sekvent crate. The names `__sekvent_*` are
reserved in proto packages (lib docs).

**`sekvent` facade:** `component-grpc = ["component", "sekvent-component/grpc"]`,
added to `full`. **`sekvent-runtime`, `sekvent-error`:** no change — the
runtime's `ServerBuilder::grpc_routes` / `add_service` and `HealthRegistry`
already cover mounting and health, and `sekvent_error::grpc` covers status
mapping.

## 4. Configuration

### 4.1 Keys

`<C>` / `<M>` as in C1 2.6; `<N>` a policy name upper-cased. All keys are
optional unless a rule in 4.4 requires them. **The known-key set does not
depend on the binding**: a key that only acts under one binding is accepted
under the others, so one environment works for every profile.

| Key | Values | Acts on |
|---|---|---|
| `SEKVENT_COMPONENT_BINDING`, `…_<C>_BINDING` | C1 | C1 |
| `SEKVENT_COMPONENT_MAX_HOPS` | integer 1–1000, default 16 | every call through a handle; inbound gRPC |
| `…_<C>_ENDPOINT` | `http://<host>:<port>` (4.2) | `grpc` binding (required there) |
| `…_<C>_LINK` | `[A-Za-z0-9_]+`, lower-cased; default the component name | `grpc` binding: presents `SEKVENT_LINK_OUTBOUND_<LINK>` |
| `…_<C>_AUTH` | `link` (default) or `none` | `grpc` binding |
| `…_<C>_SERVE` | `none` (default) or `grpc` | exposing a locally bound component |
| `…_<C>_SERVE_AUTH` | `link` (default) or `none` | exposed component |
| `…_<C>_POLICY`, `…_<C>_<M>_POLICY` | policy name `[A-Za-z0-9_]+` | named layer (4.3) |
| `…_<C>_<FIELD>` | component fields below | defaults for every method; per-component state |
| `…_<C>_<M>_<FIELD>` | method fields below | one method |
| `SEKVENT_POLICY_<N>_<FIELD>` | component fields below | every component or method referencing `<N>` |

Fields use exactly `PolicySpec::from_config` (value grammar, minimums, zero
rules; errors name the key, never the value), read with the prefix of their
level (`SEKVENT_COMPONENT_INVENTORY_RESERVE_`, `SEKVENT_POLICY_REMOTE_`, …).
This replaces C1's two readers; C1 tests that assert exact message text
move to `from_config`'s wording.

| Field | Level | Where it acts |
|---|---|---|
| `TIMEOUT` | method, component | caller side under every binding (narrows the callee deadline, C1); serving side for gRPC-served calls (2.4) |
| `BULKHEAD_MAX_CONCURRENT`, `BULKHEAD_MAX_QUEUE`, `BULKHEAD_QUEUE_TIMEOUT` | method, component | serving side only (local bindings), per method; a full queue or an expired wait sheds with `RESOURCE_EXHAUSTED` / `BULKHEAD_FULL` |
| `RETRY_MAX_ATTEMPTS`, `RETRY_INITIAL_BACKOFF`, `RETRY_MAX_BACKOFF`, `RETRY_MULTIPLIER`, `RETRY_JITTER`, `RETRY_MAX_RETRY_AFTER` | method, component | `grpc` caller, `idempotent` methods only |
| `RETRY_BUDGET_RATIO`, `RETRY_BUDGET_MIN_PER_SEC` | component only | one `RetryBudget` per remote component, shared by its methods |
| `BREAKER_ENABLED`, `BREAKER_FAILURE_RATE`, `BREAKER_WINDOW`, `BREAKER_MIN_CALLS`, `BREAKER_WAIT_IN_OPEN`, `BREAKER_PERMITTED_IN_HALF_OPEN` | component only | one `CircuitBreaker` per remote component (its endpoint) |

`RATE_LIMIT_*` and every other suffix are unknown keys. Budget and breaker
fields are component-only because their state belongs to the endpoint;
`idempotent` is code-only — configuration never makes a method retryable.

### 4.2 `ENDPOINT`

`http://` + host (DNS name, IPv4, or bracketed IPv6) + `:` + port 1–65535,
optionally followed by a single `/`. Userinfo, a path, a query or a fragment
→ `ConfigError::Invalid { key, reason }`. `https://…` →
`Invalid { reason: "TLS endpoints are not supported yet; use http:// on a
private network or behind a TLS-terminating proxy" }`. Anything else →
`Malformed { expected: "http://host:port" }`. Messages never echo the value.

### 4.3 Policy layering and precedence

Per method `m` of component `c`, `PolicySpec::resolve` over, lowest first:

0. **Framework default** — remote bindings only:
   `retry_max_attempts = 3`, `breaker_enabled = true`; everything else from
   the `sekvent-resilience` defaults (exponential backoff defaults, budget
   20 % + 10/s, breaker over the last 20 calls at 50 % with at least 10
   calls, 30 s open, 3 probes, `Retry-After` honoured up to 30 s).
   `RETRY_MAX_ATTEMPTS=1` and `BREAKER_ENABLED=false` switch them off.
1. **Attribute** — `timeout` and `bulkhead` of `#[call(…)]`.
2. **Named policy** — `SEKVENT_POLICY_<N>_*`, where `<N>` is
   `…_<C>_<M>_POLICY` when set, else `…_<C>_POLICY`.
3. **Component keys** — `…_<C>_<FIELD>`.
4. **Method keys** — `…_<C>_<M>_<FIELD>`.

The component-level result (breaker, budget) uses layers 0, the named
policy of `…_<C>_POLICY`, and 3. The breaker and budget are built once per
component with `build_breaker("component:<c>")` / the budget fields, and
shared by the per-method `RetryPolicy` values (`build_retry` followed by
`with_budget(shared)`).

### 4.4 Fail-closed rules

Checked in `AppBuilder::build` step 3 (C1 2.7), collected with every other
configuration error; no factory runs while any exists.

| Condition | Error (keys named, never values) |
|---|---|
| `grpc` binding or `SERVE=grpc` without the `grpc` feature | `BindingUnavailable` / `NotServable` |
| bound `grpc`, `ENDPOINT` unset / invalid | `Missing { key: …_ENDPOINT }` / 4.2 |
| bound `grpc`, `AUTH=link`, no outbound token | `Missing { key: SEKVENT_LINK_OUTBOUND_<LINK> }` |
| `SERVE=grpc` on a `local_only` component, or one not bound locally | `NotServable` |
| `SERVE=grpc`, `SERVE_AUTH=link`, no inbound token at all | `Missing { key: SEKVENT_LINK_INBOUND_<CALLER> }` |
| link keys needed (any row above with `link`) and `LinkConfig::from_source` fails | its `ConfigError` |
| two link keys hold one token | `Invalid { key: <second key>, reason }` from `check_distinct_tokens` |
| a `POLICY` reference with no `SEKVENT_POLICY_<N>_*` key | `Invalid { key: <the POLICY key>, reason: "no SEKVENT_POLICY_<N>_* key is set" }` |
| a method-level reference to a policy that sets component-only fields | `Invalid { key: …_<C>_<M>_POLICY }` |
| a `SEKVENT_POLICY_*` key outside every referenced policy's fields | unknown key, sorted, with a suggestion (`check_reserved` on `Prefixed(source, POLICY_PREFIX)`) |
| a resolved spec that `build_*` rejects (`PolicyError`) | `Invalid { key: "<level prefix>*", reason: "<parameter>: <reason>" }` |
| `MAX_HOPS` malformed / outside 1–1000 | `Malformed` / `Invalid` |

`AUTH=none` and `SERVE_AUTH=none` are accepted and logged once at `warn!`
naming the key. Link configuration is read only when some row needs it, so
an App without remote bindings or exposure keeps C1's behaviour even with
link keys it does not use. At start, an exposed component whose routes were
never mounted fails `App::start` (3.1).

Known-key set: C1's, plus `MAX_HOPS`; per component `ENDPOINT`, `LINK`,
`AUTH`, `SERVE`, `SERVE_AUTH`, `POLICY` and the component fields; per
method `POLICY` and the method fields. C1's exact-equality collision check
covers the new keys (the only new clash is a method named `bulkhead_queue`
against the component key `BULKHEAD_QUEUE_TIMEOUT`).

### 4.5 Framework key lists

`FRAMEWORK_PREFIXES` gains `SEKVENT_POLICY_` (3.2). `SEKVENT_LINK_INBOUND_`,
`SEKVENT_LINK_OUTBOUND_` and `SEKVENT_LINK_TRUSTED` are already framework
keys; no other list changes.

## 5. Macro changes

The only macro change is the contract link. The handle, dispatcher,
descriptor and `#[call]` grammar are unchanged; transport choice never
reaches generated code.

### 5.1 Grammar and errors

```text
#[component( name = "<name>" [, package = "<package>"] [, proto = "<module path>"]
             [, local_only | , remote_only] [, crate = "<path>"] )]
```

`proto` is the path of the module that `sekvent-proto-build` generated for
the component's proto package (`"crate::proto::shop::inventory::v1"` in an
`-api` crate following C1 5.2). It is **required** for standard and
`remote_only` components and forbidden for `local_only` ones: a component
that may cross a process boundary always has a checked contract (decision
1). `package` stays required and is cross-checked by `assert_service`.

| # | Violation | Message |
|---|---|---|
| C7 (changed) | unknown argument | ``unknown component argument `x`; expected name, package, proto, local_only, remote_only or crate`` |
| C13 | `proto` missing (not `local_only`) | ``missing `proto = "..."`: the module generated for the component's proto package, such as "crate::proto::shop::inventory::v1"; only a local_only component may omit it`` |
| C14 | `proto` on `local_only` | ``a local_only component has no contract; remove `proto` `` |
| C15 | not a path | `proto must be a module path such as "crate::proto::shop::inventory::v1"` |

Type-level contract failures are the const panics of 3.1 plus rustc's own
errors (a missing `__sekvent_service_<Trait>` constant, a message without
`prost::Name` through the `ContractMessage` diagnostic).

### 5.2 Expansion diff against C1 3.4

Input: C1's reference component with
`#[sekvent::component(name = "inventory", package = "shop.inventory.v1", proto = "crate::proto::shop::inventory::v1")]`.
Output: C1 3.4 verbatim, followed by (the first `const` spanned on the trait
name, each `assert_rpc` on its method's signature):

```rust
const _: () = ::sekvent_component::__private::assert_service(
    crate::proto::shop::inventory::v1::__sekvent_service_Inventory,
    "shop.inventory.v1.Inventory",
    &["Reserve", "Release"],
);
const _: () = ::sekvent_component::__private::assert_rpc::<ReserveRequest, ReserveReply>(
    crate::proto::shop::inventory::v1::__sekvent_service_Inventory,
    "Reserve",
);
const _: () = ::sekvent_component::__private::assert_rpc::<ReleaseRequest, ReleaseReply>(
    crate::proto::shop::inventory::v1::__sekvent_service_Inventory,
    "Release",
);
```

plus, per method, `assert_rpc_types::<Req, Rep, <path>::__sekvent_rpc_Inventory__Reserve>()`,
which compares the types themselves. Two methods mapping to one RPC name
(`get_v2`, `get_v_2`) are a macro error. The path is emitted exactly as
parsed, followed by `::__sekvent_service_<Trait>`; types are the ones
written in the trait.
`remote_only` gets the same three items; `local_only` gets none (its
snapshot is unchanged).

`crates/sekvent-component/tests/support/inventory.rs` (the hand-written
expansion) adds the same three items, a `pub mod proto` holding a
hand-written `__sekvent_service_Inventory` for its two RPCs, and
`impl prost::Name` for its four messages (`PACKAGE = "shop.inventory.v1"`).

### 5.3 Tests (sekvent-macros, excluded from coverage as in C1)

- Snapshots `component__standard` and `component__remote_only` change as
  above; unit tests for the `proto` argument parsing (C13–C15).
- trybuild pass cases: every standard and `remote_only` case declares a
  module with the constant and `impl prost::Name` for its messages; a new
  pass case uses a real `sekvent-proto-build`-shaped constant with a message
  from another package.
- trybuild fail cases: `c13_missing_proto`, `c14_proto_on_local_only`,
  `c15_bad_proto_path`, `p1_method_without_rpc`, `p2_extra_rpc`,
  `p3_wrong_request`, `p4_wrong_reply`, `p5_streaming_rpc`,
  `p6_package_mismatch`, `p7_message_without_name`, `p8_missing_constant`.
  `.stderr` files are generated on the Mac (C1 decision 8).

## 6. Contract emit and check

### 6.1 Mechanism: the proto is the contract, the macro ties the trait to it

Three pieces, each checkable on its own:

1. The `-api` crate's `.proto` declares the component's `service` next to
   its messages (`service Inventory { rpc Reserve(ReserveRequest) returns
   (ReserveReply); … }`). This is standard gRPC practice: any toolchain,
   `buf` included, can consume it.
2. `sekvent-proto-build` emits `__sekvent_service_<Service>` per service
   (3.2), and `#[component(proto = …)]` asserts at compile time that the
   trait and that service agree exactly (5.2). A trait and its proto can
   therefore never drift: removing a method without touching the proto, or
   the other way round, does not compile.
3. `cargo sekvent contract emit | check` works on the protos alone: it
   compiles them in-process with **`protox`** (pure Rust; no `protoc`, no
   cargo build), so it runs anywhere — `emit` on the Mac (it writes into the
   tree), `check` as an in-process gate step.

Rejected alternatives: dumping descriptors by building and running a
generated test binary needs a Rust build per check and brings no artifact
back from rtx; parsing Rust sources with `syn` to recover the service would
need import resolution to map types to proto names.

### 6.2 `[contract]` in `sekvent.toml`

```toml
[contract]
roots = ["crates/billing-api/proto"]  # every .proto below each root is compiled; every service in them is a contract
includes = []                         # extra import directories for every root
baseline = "contracts"                # committed baselines, relative to sekvent.toml
gate = true                           # run `contract check` in `cargo sekvent gate` when roots is not empty
```

`deny_unknown_fields`; defaults: no roots (the feature is off), no
includes, `contracts`, `true`. Each root is compiled with the include path
`[root] + includes` (protox bundles the `google/protobuf` well-known types),
its `.proto` files sorted, `include_imports(true)`,
`include_source_info(false)`. Two roots defining the same service full name
is an error. A file with `syntax = "editions"`, or a field of type `group`,
is an error naming the file (not supported in C2).

### 6.3 Canonical contract

One file per service, `<baseline>/<full service name>.json`, holding the
service and the transitive closure of the messages and enums its RPCs reach
(through fields and map values). `google.protobuf.*` types are referenced by
name and never expanded. Serialized with `serde_json::to_string_pretty`
from `BTreeMap`s (numeric keys sort numerically), with a trailing newline,
so emitting twice yields identical bytes:

```json
{
  "format": 2,
  "service": "shop.inventory.v1.Inventory",
  "file": "shop/inventory/v1/inventory.proto",
  "rpcs": {
    "Reserve": { "request": "shop.inventory.v1.ReserveRequest", "reply": "shop.inventory.v1.ReserveReply",
                 "client_streaming": false, "server_streaming": false }
  },
  "messages": {
    "shop.inventory.v1.ReserveRequest": {
      "fields": {
        "1": { "name": "order_id", "type": "string", "cardinality": "singular", "oneof": null, "default": null },
        "3": { "name": "quantity", "type": "uint32", "cardinality": "singular", "oneof": null, "default": null }
      },
      "reserved": [[2, 2]],
      "reserved_names": ["sku_code"],
      "extension_ranges": [],
      "extensions": {}
    }
  },
  "enums": {
    "shop.orders.v1.OrderStatus": {
      "values": { "0": ["ORDER_STATUS_UNSPECIFIED"], "1": ["ORDER_STATUS_PLACED"] },
      "reserved": [], "reserved_names": [], "closed": false
    }
  }
}
```

- `type`: a scalar name (`double` … `bytes`), `message:<full name>`,
  `enum:<full name>`, or for maps `map<K, V>` with `V` in the same grammar.
- `cardinality`: `singular` (proto3 implicit or `optional`, proto2
  `optional`), `repeated`, `map`, `required`.
- `oneof`: the real oneof's name; proto3 `optional`'s synthetic oneof is
  `null`.
- `reserved`: inclusive `[start, end]` pairs, merged and sorted (message
  ranges converted from the descriptor's exclusive ends); enum `values` list
  every name of a number (aliases); `closed` is true for proto2 enums.
- `default`: a proto2 `[default = …]` as written (an enum default as its
  number), else `null`.
- `extension_ranges` (inclusive pairs, merged) and `extensions` (by number:
  `name`, `type`, `cardinality`, `default`) of every extension of the
  message declared in the compiled files.
- Options, JSON names and comments are not recorded. Types from the
  bundled well-known files are exempt by source file, not by package, so a
  user file in package `google.protobuf` is still expanded.
- A baseline of an older format is rejected with "outdated contract
  format …; re-emit the baselines"; a newer one is unsupported.
- Roots and includes must resolve to directories inside the project, and a
  file whose real path leaves it is refused; symlinked directories are not
  walked. A root that declares no service is an error.

### 6.4 Compatibility rules

`check` compares each baseline `B` with the current contract `C` of the
same service. A change is **breaking** when an existing binary peer could
misread or fail on the wire:

| # | Breaking change |
|---|---|
| R1 | the service is gone (renamed, moved to another package, deleted) |
| R2 | an RPC in `B` is missing from `C` |
| R3 | an RPC's request or reply type (full name) changed |
| R4 | an RPC's streaming flags changed |
| R5 | a field number of a `B` message is missing in `C` and not reserved there |
| R6 | a field's type changed (scalar, message/enum full name, map key or value) |
| R7 | a field's cardinality changed |
| R8 | a field moved into or out of a oneof, or the pre-existing fields sharing its oneof changed |
| R9 | a number reserved in a `B` message is no longer reserved (dropped, or used by a field) |
| R10 | an enum value number in `B` is missing in `C` and not reserved |
| R11 | a number reserved in a `B` enum is no longer reserved |
| R12 | an enum changed between open and closed |
| R13 | a `required` field was removed (even with its number reserved) |
| R14 | a `required` field was added |
| R15 | a field's or extension's explicit default changed |
| R16 | an extension of a `B` message is gone, or changed type or cardinality |
| R17 | a number of a `B` extension range is neither an extension range nor reserved in `C` |

Rules R5–R17 apply to every message and enum of `B`'s closure that `C`
still defines under the same name; a type no longer reachable was replaced
somewhere, which R3 or R6 already reports. **Compatible:** new services,
RPCs, messages, non-required fields, extensions, extension ranges, enum
values and reservations; renamed fields, enum
values and oneofs (the binary wire carries numbers only — Rust callers in
the same workspace are caught by the compiler instead); options and
comments. A current service without a baseline is an error ("no baseline
for `<service>`; run `cargo sekvent contract emit <service>` and commit
`<path>`"). Retiring a service is an explicit act: delete its baseline.

### 6.5 CLI and gate

```text
cargo sekvent contract emit  [<service>...]   write baselines (all services, or those named)
cargo sekvent contract check [<service>...]   compare with the baselines
```

- Both exit 1 with "no [contract] roots in sekvent.toml" when unconfigured,
  and exit 1 on a proto that does not compile (protox's message, which
  names file and line).
- `emit` overwrites the named services' files, prints `wrote <path>` per
  file, never deletes one, exit 0.
- `check` prints one line per finding, sorted —
  `breaking: shop.inventory.v1.Inventory: field 3 (quantity) of shop.inventory.v1.ReserveRequest was removed without reserving its number`
  — then `contract: <n> breaking changes in <m> services` or
  `contract: <k> services compatible`; exit 1 on any finding or missing
  baseline, else 0.
- `contract` leaves `dispatch::RESERVED` (now `component`, `extract`,
  `queue`, `schedule`).
- Gate: a new `Step::Contract`, run in-process after `Step::Boundaries`
  (no compile, no rrb) when `roots` is not empty and `gate = true`.

Library surface in `sekvent-tasks` (`src/contract/{mod,compile,model,rules}.rs`,
dependencies `protox = "0.9.1"`, `prost-types`, `serde_json`):

```rust
pub struct ContractConfig { pub roots: Vec<PathBuf>, pub includes: Vec<PathBuf>, pub baseline: PathBuf, pub gate: bool }
pub struct Contract { /* 6.3, Serialize + Deserialize + PartialEq */ }
pub struct Finding { pub service: String, pub message: String }       // Display: "breaking: <service>: <message>"
pub fn load(root: &Path, config: &ContractConfig) -> anyhow::Result<BTreeMap<String, Contract>>;
pub fn compare(baseline: &Contract, current: Option<&Contract>) -> Vec<Finding>;
pub fn emit(root: &Path, config: &ContractConfig, only: &[String]) -> anyhow::Result<Vec<PathBuf>>;
pub fn check(root: &Path, config: &ContractConfig, only: &[String]) -> anyhow::Result<Vec<Finding>>;
```

### 6.6 Breaking changes: a new package alongside

A breaking change goes into `shop/inventory/v2/inventory.proto` (package
`shop.inventory.v2`) with a new trait
`#[component(name = "inventory_v2", package = "shop.inventory.v2", proto = "…::v2")]`.
Both components are installed and exposed from the same binary — two
services on one port, no new machinery; the v1 implementation may delegate
to v2. Callers move handle by handle; once none uses v1, remove it and
delete its baseline.

## 7. `examples/shop`: the split-grpc profile

### 7.1 Changes and the new crate

- **Protos** gain their services: `service Inventory { Reserve, Release,
  Stock }`, `service Notifications { Notify, Sent }`, `service Orders {
  PlaceOrder, GetOrder }` (`GetOrder` returns `Order`), each RPC with the
  request/reply of the trait method. `-api` traits add
  `proto = "crate::proto::shop::<name>::v1"`; `build.rs` is unchanged
  (service constants are on by default).
- **`examples/shop/inventory-svc`** (new member; lib + thin `main.rs`;
  depends on `inventory`, `inventory-api`, `sekvent-api` with
  `["component", "component-grpc", "config", "runtime"]`, `tokio`):

  ```rust
  /// Install the inventory component with `stock`.
  pub fn install(app: &mut AppBuilder<'_>, stock: Vec<(String, u32)>) -> Result<(), BuildError>;
  /// A server on `addr` serving the App's exposed components and the health endpoints.
  pub async fn bind(app: &App, addr: SocketAddr) -> Result<Server, AppError>;
  /// The App's `components` unit plus `server` as the ingress unit `grpc`.
  pub fn runtime(app: &App, server: Server, runtime: RuntimeBuilder) -> RuntimeBuilder;
  ```

  `main.rs`: `EnvSource`, `install` with the demo stock, `build()`,
  `bind` on `INVENTORY_SVC_ADDR` (default `127.0.0.1:50051`),
  `runtime(…, Runtime::builder()).build()?.run().await`.
- **`shop`**: `main.rs` also mounts `app.grpc_routes()` on `SHOP_GRPC_ADDR`
  (default `127.0.0.1:50050`, health included), so any component the
  environment exposes is reachable; with the split environment it binds
  inventory remotely without any code change (`shop::install` already
  installs inventory; its factory does not run under `grpc`). Manifest
  features gain `component-grpc`; dev-dependencies `inventory-svc` and
  `sekvent-testing` (for `await_until!`), both by path.
- **`examples/shop/sekvent.toml`**: `[project] name = "shop"` and
  `[contract] roots = ["inventory-api/proto", "notifications-api/proto",
  "orders-api/proto"]`, `baseline = "contracts"`; the three baselines in
  `examples/shop/contracts/` are emitted by the parent on the Mac (section 9).
- **README**: the split topology, its environment and how the loopback
  suite runs it.

### 7.2 Profiles (`tests/support/mod.rs`)

```rust
pub(crate) enum Profile { MonolithLocal, MonolithSerialized, SplitGrpc }
impl Profile {
    pub(crate) fn binding_of(self, component: &str) -> Binding; // SplitGrpc: inventory Grpc, others Local
    pub(crate) fn paused(self) -> bool;                       // true for the monolith profiles
}
/// A started topology: the caller-side App and, under SplitGrpc, the inventory service.
pub(crate) struct Shop { pub(crate) app: App, service: Option<InventoryService> }
pub(crate) struct InventoryService { pub(crate) app: App, pub(crate) runtime: RuntimeHandle, pub(crate) addr: SocketAddr }
impl Shop {
    pub(crate) fn inventory_app(&self) -> &App;               // where inventory runs
    pub(crate) fn service(&self) -> Option<&InventoryService>;
}
/// `extra` keys go to every process; `inventory` installs the (real or fake)
/// inventory where it runs; notifications and orders are installed on the caller side.
pub(crate) async fn start(profile: Profile, extra: &[(&str, &str)],
    inventory: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>) -> Shop;
```

`SplitGrpc` starts the service first: an App from `extra` plus
`SEKVENT_COMPONENT_INVENTORY_SERVE=grpc`, `SEKVENT_LINK_INBOUND_SHOP=<TOKEN>`,
`SEKVENT_LINK_TRUSTED=shop`; `inventory_svc::bind` on `127.0.0.1:0`;
`inventory_svc::runtime(…, Runtime::builder().without_signals().shutdown_delay(Duration::ZERO)).start()`.
Then the caller App from `extra` plus `…_INVENTORY_BINDING=grpc`,
`…_INVENTORY_ENDPOINT=http://<addr>`, `SEKVENT_LINK_OUTBOUND_INVENTORY=<TOKEN>`,
`…_INVENTORY_RETRY_INITIAL_BACKOFF=1ms`, `…_RETRY_MAX_BACKOFF=1ms`,
`…_RETRY_JITTER=none`, installing `shop::install_inventory(app, vec![])`,
notifications and orders. `<TOKEN>` is a fixed canonical 40-character test
token. The caller App asserts `app.binding(c) == Some(profile.binding_of(c))`
for every component; the service App asserts inventory is `Local`.
C1's `started_shop` / `shop_with_inventory` become thin wrappers over
`start` returning `Shop`.

### 7.3 The shared suite under three profiles

Every test of `flows.rs`, `faults.rs` and `runtime.rs` gains
`#[case::split_grpc(Profile::SplitGrpc)]`. Tests that need
`start_paused` (C1: the three deadline tests, bulkhead, caller drop, drain)
keep the scenario in one `async fn` taking the profile and get two entry
points: the monolith cases with `#[tokio::test(start_paused = true)]`, and a
`split_grpc` twin on the real clock wrapped in a 30 s
`tokio::time::timeout` guard against hangs. Time assertions go through
`support::assert_elapsed(profile, elapsed, at_least, below)`: both bounds
when paused, only the lower bound on the real clock (loopback timing is
not exact; the structural assertions carry the test). Under `SplitGrpc` a
server-side key (bulkhead size) takes effect because `extra` reaches the
service; the retried `reserve` of the bulkhead and drain tests still ends
with the same code and reason, because the test holds the gate until the
call returns. `faults.rs` also gains, for all three profiles, **hop limit**:
`SEKVENT_COMPONENT_MAX_HOPS=1` → `place_order` fails `FAILED_PRECONDITION` /
`CALL_DEPTH_EXCEEDED` and inventory sees no call.

### 7.4 Remote fault injection (`tests/remote.rs`, split-grpc only)

New fakes in `support/fakes.rs`: `FlakyInventory` (fails the first N calls
of a chosen method with a chosen `AppError`, then succeeds; records every
entry's request id and idempotency key), `RecordingInventory` (records
`cx.caller()`, `cx.tenant()`, `sekvent::resilience::remaining(cx)` of each
call), and `PendingInventory` extended to record the remaining time.

| Test | Setup | Expectation |
|---|---|---|
| breaker opens | `FlakyInventory` failing every `release` with `UNAVAILABLE`; `…_INVENTORY_BREAKER_WINDOW=4`, `…_BREAKER_MIN_CALLS=4`, `…_BREAKER_WAIT_IN_OPEN=1h` | 4 calls reach the service and fail `UNAVAILABLE`; the 5th fails `UNAVAILABLE` / `CIRCUIT_OPEN` with `retry_after` set and metadata `component = inventory`; the service still saw 4 |
| business errors never open it | real inventory, same keys; 10 over-stock `reserve` calls | each `OutOfStock`; then a valid `reserve` succeeds |
| idempotent retry | first `stock` fails `UNAVAILABLE` | success; 2 entries with the same request id and idempotency key |
| no retry otherwise | first `release` fails `UNAVAILABLE` | `UNAVAILABLE`; 1 entry |
| `Retry-After` cap | first `stock` fails `UNAVAILABLE` with `retry_after = 60 s` | that error, `retry_after` 60 s; 1 entry |
| deadline across the hop | `PendingInventory`; caller deadline 300 ms | `DeadlineExceeded` after ≥ 300 ms; recorded remaining in `(0, 300 ms]`; the drop signal fires |
| wrong token | service expects another token | `UNAUTHENTICATED`, message `service authentication required`; no entry |
| missing token | no `SEKVENT_LINK_OUTBOUND_INVENTORY` | build fails: `Missing { key: "SEKVENT_LINK_OUTBOUND_INVENTORY" }` |
| `AUTH=none` against an authenticated server | `…_INVENTORY_AUTH=none` | `UNAUTHENTICATED` |
| trusted vs untrusted link | `RecordingInventory`; caller context with tenant `t-1`; with and without `SEKVENT_LINK_TRUSTED=shop` | `trusted("shop")` and `Some("t-1")` / `untrusted("shop")` and `None` |
| graceful shutdown | `GatedInventory`, one `reserve` in flight; `service.runtime.shutdown()` | health of `shop.inventory.v1.Inventory` becomes `NotServing` (polled with `await_until!`); after a permit the in-flight call succeeds; `wait()` is `Ok`; the next call fails `UNAVAILABLE` / `COMPONENT_UNREACHABLE` |
| not mounted | service App with `SERVE=grpc` whose routes are never taken | `start()` fails `FAILED_PRECONDITION` / `GRPC_NOT_MOUNTED` |

## 8. Test and coverage plan

Rules (C1 section 6 applies): completion signals (`oneshot`, `Notify`,
`Semaphore`, channels), `tokio::time::pause` for in-process timing,
`await_until!` only to observe external state (a closed listener, health),
no fixed sleeps. Anything with a socket binds `127.0.0.1:0` and runs on the
real clock (tokio's auto-advance would fire timers while loopback I/O is
pending); timing there is asserted by lower bounds and structure, with a
30 s `timeout` guard against hangs. Every library crate stays at ≥ 95 %
lines (`rrb run coverage`); code under `cfg(not(feature = "grpc"))` is
compiled out under `--all-features` and not counted.

**`sekvent-component`** (dev-dependencies of 3.1):

- `config` / `policy`: every new key and value grammar; the ENDPOINT table
  (DNS, IPv4, `[::1]`, trailing `/`; `https`, userinfo, path, query,
  fragment, missing or out-of-range port, other schemes) with messages that
  never contain the value; each layer of 4.3 winning over the one below,
  per field, including a method `POLICY` replacing the component's;
  component-only fields at method level (unknown key) and through a
  method-referenced policy; unreferenced and misspelt `SEKVENT_POLICY_*`
  keys with a suggestion; an empty referenced policy; a `PolicyError`
  mapped to `<prefix>*`; the `bulkhead_queue` collision; binding
  independence (the same source builds under all three bindings).
- Build rules: every row of 4.4, reported together with C1's errors and
  without running factories; `AUTH=none` / `SERVE_AUTH=none` accepted.
- `contract`: `assert_service` / `assert_rpc` called at run time under
  `#[should_panic(expected = …)]` for each message of 3.1, and in `const`
  items for the passing cases (empty package, cross-package types).
- Loopback (`tests/grpc_*.rs`, the hand-written inventory of 5.2 served by
  `sekvent_runtime::Server` from `App::grpc_routes()`): success and typed
  errors identical to the local bindings; unknown reason → `Other`; server
  metadata untouched, caller-made errors tagged; `remote_only` through
  `install_remote`; missing, wrong and unauthenticated-by-design tokens;
  trusted versus untrusted caller (subject, tenant); request id,
  traceparent and idempotency key forwarded; `grpc-timeout` shortening the
  server's deadline and the server's own method timeout for a caller
  without one; hops above the server's limit; unknown RPC; garbage body;
  caller drop resets the stream (server-side drop signal and cancelled
  token); nothing listening → `COMPONENT_UNREACHABLE`; retry of idempotent
  methods only, budget exhaustion, the `Retry-After` cap; the breaker
  opening and `CIRCUIT_OPEN` failing fast; two components on one endpoint
  sharing a channel; **interop**: a plain `tonic::client::Grpc` with
  `tonic_prost::ProstCodec` and a bearer token calls
  `/shop.inventory.v1.Inventory/Reserve` and reads a typed error from the
  standard details with `tonic-types`.
- Lifecycle: a remote component's gate (`NOT_STARTED` before start, drain of
  in-flight outbound calls on stop); `GRPC_NOT_MOUNTED`; `App::register`
  moving `grpc_services()` through `NotServing` → `Serving` → `NotServing`
  in the runtime's `HealthRegistry`.

**`sekvent-context`:** `hops` defaults, builder, `child` / `detached`; the
header codec (1–4 digits accepted; 5 digits, signs and letters ignored;
`inject` writes and removes; `propagate` keeps a caller's value and skips 0).
**`sekvent-link`:** `check_distinct_tokens` for inbound/outbound and
outbound/outbound duplicates and the clean case, errors free of tokens;
`inbound_key` / `outbound_key`. **`sekvent-resilience`:** each `build_*`
for `None`, `Some` and a `PolicyError`; existing `build` tests unchanged.
**`sekvent-config`:** the prefix test (3.2). **`sekvent-proto-build`:** the
generated constant for a fixture package with two services, a streaming
RPC and a cross-package type, compared as text; `service_contracts(false)`
and `services_only` omit it.

**`sekvent-tasks`:**

- `compile`: roots and includes, sorted files, a service in two roots,
  an editions file, a group field, a syntax error surfacing protox's file
  and line.
- `model`: the canonical JSON of a fixture, compared as text (nested types,
  maps, imports from another package, well-known types not expanded,
  reserved ranges converted and merged, synthetic oneofs, enum aliases,
  closed enums); re-serialization is byte-identical.
- `rules`: one breaking fixture per R1–R12, each compatible change of 6.4
  producing no finding, a missing baseline, sorted output and exact
  messages.
- `emit` / `check` on a temporary project; `only` filters; the CLI no
  longer treats `contract` as reserved; exit codes; `[contract]` defaults
  and unknown-key rejection; the gate plan contains `Step::Contract` exactly
  when roots are set and `gate` is true.
- Dogfood `tests/contract_shop.rs`: `check` on `examples/shop` reports
  nothing, and `emit` into a temporary copy reproduces the committed
  baselines byte for byte.

**`sekvent-macros`:** 5.3. **`examples/shop`:** section 7 (excluded from
coverage, part of every gate).

## 9. Work split

Phase 1: four agents write code and tests and **do not build, lint or run
tests**. Phase 2 (parent, on the Mac): `cargo metadata --format-version 1 >
/dev/null` to refresh `Cargo.lock`; insta snapshots and trybuild `.stderr`
files (C1 decision 8); the shop baselines with
`cargo run -q -p cargo-sekvent -- sekvent contract emit` from
`examples/shop` (in-tree codegen, protox needs no `protoc`); then
`rrb run gate` and `rrb run coverage` once over the merged tree. Phase 3:
fixes dispatched by file owner.

| Agent | Writes (disjoint) | May assume |
|---|---|---|
| **A — component runtime** | `crates/sekvent-component/**` (manifest, `src/**` incl. `policy.rs`, `contract.rs`, `grpc/**`, `tests/**` incl. the updated `tests/support/inventory.rs`); `crates/sekvent/Cargo.toml`, `crates/sekvent/src/lib.rs` (feature `component-grpc`, feature table in the crate docs); **all of root `Cargo.toml`**: member `examples/shop/inventory-svc`, `[workspace.dependencies]` `protox = "0.9.1"` | the 3.2 APIs of context, link, resilience and config exist exactly as written (agent B); the macro emits exactly 5.2 |
| **B — macros, proto-build, supporting crates** | `crates/sekvent-macros/**`; `crates/sekvent-proto-build/**`; `crates/sekvent-context/**`; `crates/sekvent-link/**`; `crates/sekvent-resilience/**`; `crates/sekvent-config/src/lib.rs` (prefix only); `crates/sekvent-facade-check/**` (its component gains `proto` and a hand-written constant; a `component-grpc` compile check) | the `__private` items of 3.1 and the `sekvent-component` `grpc` feature exist (agent A) |
| **C — contract tooling** | `crates/sekvent-tasks/**` (`src/contract/**`, `config.rs` `[contract]`, `plan.rs` / `gate.rs` step, `dispatch.rs` reserved list, `Cargo.toml` adding `protox`, `prost-types`, `serde_json` as `{ workspace = true }` where missing, `tests/contract_shop.rs`); `crates/cargo-sekvent/**` (`contract emit|check`); `templates/workspace/sekvent.toml.tmpl` (a commented `[contract]` example) | root `protox` entry (A); `examples/shop/sekvent.toml`, the service blocks of 7.1 and the baselines generated in phase 2 (D, parent) |
| **D — example and docs** | `examples/**` except `examples/shop/contracts/*.json`; `README.md` (C2 status, split topology pointer); `AGENTS.md` (crate map row for `sekvent-component`, the gRPC and contract notes); `docs/component-model.md` (status C2 implemented with a link here, roadmap row, "Contracts and the wire" now proto-first, the resilience defaults); `skills/**` (bindings, serving, link keys, named policies, `contract emit/check`) | sections 2–7 exactly; A adds the member, the facade feature and `protox` |

Every agent reports, besides its summary, **anything in this document that
contradicted the tree or could not be implemented as written**, with file
and reason, and stays inside its write set even when a fix elsewhere looks
obvious. Two couplings to watch: A's `tests/support/inventory.rs` must match
B's snapshot of 5.2 token for token, and C's dogfood test reads D's
`examples/shop/sekvent.toml` and protos.

## 10. Decisions the user might override, and risks

### Decisions the user might override

1. **Proto-first contracts with a required `proto = "…"` argument.** The
   `.proto` `service` block is the contract; the macro checks the trait
   against it at compile time, and `contract` tooling reads protos only.
   This changes every standard and `remote_only` component written for C1
   (in-repo only today). Alternatives: an optional argument (unchecked
   components could drift from their proto), or a Rust-first contract
   dumped by building and running code.
2. **One generic byte-level gRPC service and client** instead of
   macro-generated tonic stubs per component (2.1).
3. **Plaintext HTTP/2 with link tokens in C2**; `https://` endpoints are a
   build error. Tokens cross the network in clear, so this assumes a
   private network or a mesh with mTLS. Alternative: rustls-based TLS keys
   (`…_<C>_TLS_*`) now, with one more feature of tonic.
4. **Exposure by configuration** (`SERVE=grpc`) through
   `App::grpc_routes()`, with `App::start` failing when an exposed
   component's routes were never mounted. Alternatives: expose every local
   component whenever the routes are mounted, or choose in code.
5. **Remote defaults on:** three attempts for `idempotent` methods and a
   breaker per remote component unless configured off; budget and breaker
   settings are component-level only; `TIMEOUT` stays the deadline of the
   whole call (no per-attempt timeout); no transparent retry of
   non-idempotent calls even when the request provably never left.
6. **Wire-only compatibility rules.** Field, value and oneof renames are
   compatible; error reasons (`ComponentError`) are outside `contract check`
   because they live in Rust. Alternative: also flag renames (JSON-level
   rules) and move reasons into a proto enum the derive checks.
7. **One key set for every binding, and a gate for remote handles.** Policy
   and link keys are accepted under any binding, so one environment serves
   all profiles; remote components keep a caller-side gate, so start, stop,
   drain and `App::state` behave the same under every binding.
8. **Hop limit** of 16 by default, `FAILED_PRECONDITION` /
   `CALL_DEPTH_EXCEEDED` (not retried, never trips a breaker), with
   `x-sekvent-hops` accepted from any caller as a safety net against cycles
   rather than a security control. Debug spans land; metrics do not.

### Risks

- **Transport-error classification** relies on tonic leaving
  `Status::source()` empty for statuses decoded from trailers (true in
  0.14.6: `from_header_map` sets no source). A tonic upgrade could move a
  server error into `COMPONENT_UNREACHABLE`; the loopback tests pin both
  sides of the rule.
- **Real-clock loopback tests** can only assert lower time bounds; a slow
  CI host lengthens them but cannot make them fail, except through the 30 s
  hang guard.
- **Const-panic diagnostics** are rustc's `evaluation of constant value
  failed` with our message; wording and spans are whatever the toolchain
  prints, recorded in `.stderr` files and exposed to toolchain changes.
- **protox versus protoc:** builds compiled protos with `protoc`, contracts
  with `protox`, so a proto one accepts and the other rejects would split
  build and gate. Resolved since: `sekvent-proto-build` uses protox too, and
  `protoc` is needed nowhere.
- **Retry amplification across hops:** each remote hop may retry three
  times, so a chain of n hops can multiply load by 3ⁿ under failure; budgets
  and deadlines bound it, and inner hops can set `RETRY_MAX_ATTEMPTS=1`.
- **Deadline anchoring:** `headers::from_headers` anchors on the std clock,
  so paused-clock tests cannot span the network; the profile split of 7.3
  is the answer, not a clock abstraction in the codec.
- **Churn from decision 1:** every trybuild pass case, the facade check,
  the hand-written expansion and the example change in lockstep with the
  macro; section 9 names who owns which copy.
- **Build time:** a new example crate, the loopback suites and protox in
  the CLI join every gate run.
