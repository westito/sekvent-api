# Component model — milestone C1 implementation spec

> Status: approved design for implementation. Follows
> [`docs/component-model.md`](../component-model.md); where this document is
> more specific, it wins for C1. Three agents implement it in parallel
> (section 7); every cross-agent contract is spelled out here, so nobody
> needs to ask anybody.

## 1. Scope

### In C1

- New crate **`sekvent-component`** (runtime side) and two new macros in
  **`sekvent-macros`**: the trait attribute `#[component(...)]` and
  `#[derive(ComponentError)]`, re-exported as `sekvent_component::component`,
  `sekvent_component::ComponentError` and, through the facade,
  `sekvent::component` / `sekvent::ComponentError`.
- **Bindings:** `local` (direct call, no encoding) and `local-serialized`
  (prost-encode the request, carry the `CallContext` through the header
  codec, run the call on a separate task, encode the reply or error, decode
  it back; the task is aborted when the caller drops the future). The value
  `grpc` parses, and `AppBuilder::build` then fails with
  `binding grpc (set by <KEY>) is not available in this build`.
- **Method kind:** `#[call]` only. `#[async_call]` and `#[deferred]` are
  compile errors saying the kind is planned (section 3.3).
- **Method attributes** inside `#[call(...)]`: `idempotent` (recorded in the
  descriptor; no runtime effect until C2 retries), `timeout = "<humantime>"`,
  `bulkhead = <N>`.
- **Component modes:** `#[component(..., local_only)]` accepts plain Rust
  request/reply types (`Send + 'static`), needs no `package`, and can only be
  bound `local`. `#[component(..., remote_only)]` never runs in this binary:
  it has no factory (`install_remote`), a local binding is a build error, and
  because C1 has no remote transport every C1 build with a `remote_only`
  component fails (with a message naming the key). It exists in C1 so API
  crates can be written ahead of C2.
- **Fail-closed App builder** with constructor injection:
  `XHandle::install(&mut app, |deps| Ok(X::new(deps.handle::<YHandle>()?)))`.
  Factories run in `build()`, in install order, and only for local bindings.
  Build fails, naming the key and never a value, on: installing a component
  (by handle type or by name) twice; unknown `SEKVENT_COMPONENT_*` keys;
  malformed values; an unavailable binding; a mode/binding mismatch;
  colliding configuration keys; a factory error (including a dependency that
  is not installed before it). All configuration errors are reported
  together; factories run only when configuration is clean. Remote bindings
  will additionally need link authentication in C2.
- **Lifecycle:** optional `Lifecycle` hooks; components start in install
  order and stop in reverse order. Stopping a component first drains it: new
  calls are rejected with `UNAVAILABLE` (`COMPONENT_DRAINING`) while
  in-flight calls finish, bounded by a grace period. The App runs either
  standalone (`App::start` / `App::stop`) or as one `sekvent-runtime` unit in
  `Stage::Components` that sequences all components itself (units of one
  stage start concurrently, so per-component units would lose the order).
- **Resilience per method:** deadline (attribute timeout capped by the
  caller's deadline, overridable by configuration), bulkhead without a queue
  (a full bulkhead sheds immediately with `RESOURCE_EXHAUSTED` /
  `BULKHEAD_FULL`), and load shedding of dead calls (cancelled or expired
  contexts are rejected before any work; calls to a component that is not
  serving are rejected with `UNAVAILABLE`).
- **Typed errors:** a variant travels as an `AppError` with its code, a
  stable reason, an optional domain and its fields as metadata; a known
  reason decodes back into the variant; an unknown reason, or a field that
  does not parse, decodes into the `#[other]` variant as a plain `AppError`.
  Decoding never fails.
- **`local-serialized` wire path:** request and reply via
  `prost::Message`; `CallContext` → `http::HeaderMap` with
  `sekvent_context::headers::inject` → `headers::from_headers` with the
  caller `ServiceIdentity::trusted(LOCAL_CALLER)` (an in-process trusted
  link, so subject and tenant survive); errors as
  `AppError` → `WireError` → private prost message → `WireError` →
  `AppError` → `E::from_app_error`.
- **`examples/shop`**: seven crates, one test suite that runs every scenario
  under the profiles `monolith-local` and `monolith-serialized` in one
  `cargo test`.

### Deferred (not in C1)

| Item | Where |
|---|---|
| `grpc` transport, link auth (`auth = none` escape), retry for `idempotent` methods, circuit breaker, endpoint/link keys | C2 |
| Named policies (`SEKVENT_POLICY_*`), bulkhead queue keys, a shared component-wide bulkhead | C2 |
| Cross-component **call-hop limit**: `CallContext` has no hop counter and the header codec carries none; C2 adds both | C2 |
| `contract emit` / `check`, `split-grpc` profile, request/reply full names in descriptors | C2 |
| `#[async_call]`, `#[deferred]`, topics, bus | C3 |
| DB pools per component (`deps.pool(name)`), database roles, distinct-target check across component pools. **C1 recommendation:** each component builds or receives its own pool through its factory (a newtype per component via `AppBuilder::provide`) and never shares one with another component | later |
| Telemetry: spans and metrics per call. C1 only propagates the caller's tracing span into the serialized task and logs build decisions at `debug` and drain timeouts at `warn` | later |

## 2. Public API of `sekvent-component`

### 2.1 Files

```text
crates/sekvent-component/
  Cargo.toml
  src/lib.rs          crate docs, re-exports, constants, `extern crate self as sekvent_component;`
  src/binding.rs      Binding, ComponentMode
  src/descriptor.rs   ComponentDescriptor, MethodDescriptor, MethodKind, ComponentHandle
  src/error.rs        ComponentError (+ impl for AppError), BuildError
  src/reasons.rs      `pub mod reasons` constants
  src/lifecycle.rs    Lifecycle, private LifecycleDyn (boxed futures, blanket impl)
  src/config.rs       key grammar, known-key set, collisions, binding + policy resolution
  src/app.rs          App, AppBuilder, Deps, ComponentState, build, start/stop
  src/server.rs       serving side: gate (state + in-flight count), per-method bulkheads
  src/link.rs         Endpoint<D>, Route, the local and serialized call pipelines
  src/wire.rs         private prost WireErrorPb, encode/decode helpers, AbortOnDrop
  src/runtime.rs      #[cfg(feature = "runtime")] App::register
  src/__private.rs    macro plumbing (section 2.4), `#[doc(hidden)] pub mod __private`
  tests/support/inventory.rs   the hand-written expansion of section 3.4, verbatim
  tests/*.rs          integration tests (section 6)
```

### 2.2 Manifest

```toml
[package]
name = "sekvent-component"
description = "Components with local and serialized bindings for sekvent."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
publish.workspace = true

[features]
default = ["macros"]
# `component` attribute and `ComponentError` derive, re-exported from sekvent-macros.
macros = ["dep:sekvent-macros"]
# `App::register`: run all components as one sekvent-runtime unit.
runtime = ["dep:sekvent-runtime"]

[dependencies]
sekvent-config = { workspace = true }
sekvent-error = { workspace = true }
sekvent-context = { workspace = true }
sekvent-resilience = { workspace = true }
sekvent-macros = { workspace = true, optional = true }
# Path, not workspace: keeps sekvent-runtime's default grpc-web feature off.
sekvent-runtime = { path = "../sekvent-runtime", default-features = false, optional = true }
bytes = { workspace = true }
http = { workspace = true }
prost = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
sekvent-component = { path = ".", features = ["runtime"] }
tokio = { workspace = true, features = ["test-util"] }

[lints]
workspace = true
```

`runtime` is off by default so `-api` crates (which only declare traits and
errors) do not pull tonic and axum; the facade turns it on together with its
own `runtime` feature (section 4). Crate map row: `sekvent-component` —
"components, App builder, local and serialized bindings" — depends on
config, error, context, resilience, macros, runtime (optional).

### 2.3 Public API (exact signatures)

```rust
// lib.rs
#![forbid(unsafe_code)]
extern crate self as sekvent_component; // generated code says ::sekvent_component inside this crate too

pub use sekvent_context::CallContext;
pub use sekvent_error::{AppError, ErrorCode};
#[cfg(feature = "macros")]
pub use sekvent_macros::{component, ComponentError}; // attribute + derive (macro namespace)
pub use app::{App, AppBuilder, ComponentState, Deps};
pub use binding::{Binding, ComponentMode};
pub use descriptor::{ComponentDescriptor, ComponentHandle, MethodDescriptor, MethodKind};
pub use error::{BuildError, ComponentError};           // trait (type namespace)
pub use lifecycle::Lifecycle;
pub mod reasons;
#[doc(hidden)]
pub mod __private;

/// Caller name a component sees for calls made in this process, under both local bindings.
pub const LOCAL_CALLER: &str = "local";
/// Prefix of every component configuration key.
pub const CONFIG_PREFIX: &str = "SEKVENT_COMPONENT_";
/// Default binding for every standard component without its own binding key.
pub const DEFAULT_BINDING_KEY: &str = "SEKVENT_COMPONENT_BINDING";

// binding.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Binding { Local, LocalSerialized, Grpc }
impl Binding {
    pub const ALL: [Binding; 3] = [Self::Local, Self::LocalSerialized, Self::Grpc];
    pub const fn as_str(self) -> &'static str;      // "local", "local-serialized", "grpc"
    pub fn parse(value: &str) -> Option<Binding>;   // exact match on as_str, nothing else
    pub const fn is_local(self) -> bool;            // Local | LocalSerialized
}
impl fmt::Display for Binding;                      // as_str

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComponentMode { Standard, LocalOnly, RemoteOnly }

// descriptor.rs — fields private; const builders so generated code survives new fields
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MethodKind { Call }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodDescriptor { /* name, rpc, kind, idempotent, timeout, bulkhead */ }
impl MethodDescriptor {
    pub const fn call(name: &'static str, rpc: &'static str) -> Self; // kind Call, rest unset
    pub const fn with_idempotent(self) -> Self;
    pub const fn with_timeout(self, timeout: Duration) -> Self;
    pub const fn with_bulkhead(self, max_concurrent: u32) -> Self;
    pub const fn name(&self) -> &'static str;       // "reserve"
    pub const fn rpc(&self) -> &'static str;        // "Reserve"
    pub const fn kind(&self) -> MethodKind;
    pub const fn is_idempotent(&self) -> bool;
    pub const fn timeout(&self) -> Option<Duration>;
    pub const fn bulkhead(&self) -> Option<u32>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentDescriptor { /* name, service, package, mode, methods */ }
impl ComponentDescriptor {
    pub const fn new(name: &'static str, service: &'static str,
                     methods: &'static [MethodDescriptor]) -> Self; // Standard, no package
    pub const fn with_package(self, package: &'static str) -> Self;
    pub const fn with_mode(self, mode: ComponentMode) -> Self;
    pub const fn name(&self) -> &'static str;       // "inventory"
    pub const fn service(&self) -> &'static str;    // "Inventory" (the trait name)
    pub const fn package(&self) -> Option<&'static str>;
    pub const fn mode(&self) -> ComponentMode;
    pub const fn methods(&self) -> &'static [MethodDescriptor];
    pub fn full_service_name(&self) -> Option<String>; // "shop.inventory.v1.Inventory"
}

/// Implemented by every generated handle.
pub trait ComponentHandle: Clone + fmt::Debug + Send + Sync + 'static {
    const DESCRIPTOR: &'static ComponentDescriptor;
}

// error.rs
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a component error",
    note = "derive it with `#[derive(ComponentError)]`, or use `AppError`")]
pub trait ComponentError: Sized + Send + 'static {
    /// Code, reason, optional domain and fields as metadata.
    fn into_app_error(self) -> AppError;
    /// Known reason (and domain, when declared) with parseable fields → that
    /// variant; anything else → the catch-all variant. Never fails.
    fn from_app_error(error: AppError) -> Self;
}
impl ComponentError for AppError { /* both directions are the identity */ }

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    #[error("component {component} is installed twice")]
    DuplicateInstall { component: String },
    #[error("a resource of type {type_name} is provided twice")]
    DuplicateResource { type_name: &'static str },
    #[error("configuration key {key} would configure both {first} and {second}; rename one of them")]
    KeyCollision { key: String, first: String, second: String },
    #[error(transparent)]
    Config(#[from] sekvent_config::ConfigError),
    #[error("component {component}: binding {binding} (set by {key}) is not available in this build")]
    BindingUnavailable { component: String, binding: Binding, key: String },
    #[error("component {component} is local_only and can only be bound local; {key} selects {binding}")]
    LocalOnly { component: String, binding: Binding, key: String },
    #[error("component {component} is remote_only and cannot be bound {binding} ({key})")]
    RemoteOnly { component: String, binding: Binding, key: String },
    #[error("component {component} is remote_only; set {key} to a remote binding")]
    RemoteOnlyUnbound { component: String, key: String },
    #[error("component {component} failed to build: {source}")]
    Factory { component: String, source: AppError },
    /// Never nested; a single error is returned bare.
    #[error("{} component build errors: {}", .0.len(), /* items joined with "; " */)]
    Multiple(Vec<BuildError>),
}
// `first` / `second` in KeyCollision read "component inventory" or
// "method inventory.reserve".

// lifecycle.rs
pub trait Lifecycle: Send + Sync + 'static {
    fn on_start(&self) -> impl Future<Output = Result<(), AppError>> + Send { async { Ok(()) } }
    fn on_stop(&self) -> impl Future<Output = Result<(), AppError>> + Send { async { Ok(()) } }
}

// app.rs
#[derive(Clone)]
pub struct App { /* Arc<Inner> */ }
impl App {
    pub fn builder(source: &dyn ConfigSource) -> AppBuilder<'_>;
    /// A handle for code outside the components (ingress, tests).
    pub fn handle<H: ComponentHandle>(&self) -> Result<H, AppError>; // FAILED_PRECONDITION if not installed
    pub fn binding(&self, component: &str) -> Option<Binding>;
    pub fn state(&self, component: &str) -> Option<ComponentState>;
    pub fn components(&self) -> Vec<&'static str>;                  // install order
    pub async fn start(&self) -> Result<(), AppError>;
    pub async fn stop(&self, grace: Duration) -> Result<(), AppError>;
    #[cfg(feature = "runtime")]
    pub fn register(&self, runtime: sekvent_runtime::RuntimeBuilder) -> sekvent_runtime::RuntimeBuilder;
}
impl fmt::Debug for App; // names, bindings, states

pub struct AppBuilder<'a> { /* &'a dyn ConfigSource, installs, resources */ }
impl AppBuilder<'_> {
    pub fn provide<T: Clone + Send + Sync + 'static>(&mut self, value: T) -> Result<(), BuildError>;
    pub fn build(self) -> Result<App, BuildError>;
}
impl fmt::Debug for AppBuilder<'_>;

pub struct Deps<'b> { /* view of the build in progress */ }
impl Deps<'_> {
    pub fn component(&self) -> &'static str;              // the component being built
    pub fn binding(&self) -> Binding;                     // its resolved binding
    pub fn handle<H: ComponentHandle>(&self) -> Result<H, AppError>;
    pub fn resource<T: Clone + Send + Sync + 'static>(&self) -> Result<T, AppError>;
    pub fn config(&self) -> &dyn ConfigSource;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComponentState { NotStarted, Serving, Draining, Stopped }

// reasons.rs
pub mod reasons {
    pub const NOT_STARTED: &str = "COMPONENT_NOT_STARTED";
    pub const DRAINING: &str = "COMPONENT_DRAINING";
    pub const STOPPED: &str = "COMPONENT_STOPPED";
    pub const BULKHEAD_FULL: &str = "BULKHEAD_FULL";       // set by sekvent-resilience
    pub const MALFORMED_REQUEST: &str = "MALFORMED_REQUEST";
    pub const MALFORMED_REPLY: &str = "MALFORMED_REPLY";
}
```

`Deps::handle` errors (`FAILED_PRECONDITION`, message names both
components): the component is not installed; it is installed at or after
the current one ("install inventory before orders"); it failed to build.
`Deps::resource` / `App::handle` errors are `FAILED_PRECONDITION` naming the
type or component. `App::start` on an App that was already started, and
after `stop`, is `FAILED_PRECONDITION`; `stop` is idempotent and on a
never-started App only marks components `Stopped`.

### 2.4 `__private` (the contract with the macro; semver-exempt, `#[doc(hidden)]`)

```rust
pub use bytes::Bytes;
pub use prost;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a protobuf message",
    note = "component requests and replies must be prost messages; declare the component `local_only` to use plain Rust types")]
pub trait WireMessage: prost::Message + Default + Send + 'static {}
impl<T: prost::Message + Default + Send + 'static> WireMessage for T {}

#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a local_only request or reply",
    note = "local_only requests and replies must be `Send + 'static`")]
pub trait LocalMessage: Send + 'static {}
impl<T: Send + 'static> LocalMessage for T {}

#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be carried as error metadata",
    note = "ComponentError fields must implement Display and FromStr, or be an Option of such a type")]
pub trait MetadataValue: fmt::Display + FromStr {}
impl<T: fmt::Display + FromStr> MetadataValue for T {}

// Compile-time assertions used in generated `const _: () = { ... };` blocks.
pub const fn assert_wire<T: WireMessage>() {}
pub const fn assert_local<T: LocalMessage>() {}
pub const fn assert_error<T: ComponentError>() {}
pub const fn assert_metadata<T: MetadataValue>() {}

/// Byte-level dispatcher of one component (generated per component; C2's gRPC server reuses it).
pub trait Dispatch: Send + Sync + 'static {
    fn dispatch(&self, method: usize, cx: CallContext, body: Bytes)
        -> BoxFuture<'static, Result<Bytes, AppError>>;
}
/// Decode `Req` (INVALID_ARGUMENT / MALFORMED_REQUEST on failure), run `call`,
/// map `E` with `into_app_error`, encode `Rep`.
pub fn serve<Req, Rep, E, F, Fut>(cx: CallContext, body: Bytes, call: F)
    -> BoxFuture<'static, Result<Bytes, AppError>>
where
    Req: WireMessage, Rep: WireMessage, E: ComponentError,
    F: FnOnce(CallContext, Req) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Rep, E>> + Send + 'static;
/// UNIMPLEMENTED naming the component and the index.
pub fn unknown_method(descriptor: &'static ComponentDescriptor, method: usize)
    -> BoxFuture<'static, Result<Bytes, AppError>>;

/// What a generated handle wraps. Built only by the App builder.
pub struct Endpoint<D: ?Sized> { /* Arc<Link>, Route<D> */ }
impl<D: ?Sized> Clone for Endpoint<D>;          // manual impls: no `D: Clone`/`D: Debug` bound
impl<D: ?Sized> fmt::Debug for Endpoint<D>;     // Endpoint { component, binding }
impl<D: ?Sized + Send + Sync + 'static> Endpoint<D> {
    pub fn binding(&self) -> Binding;
    /// Standard components: route by binding (local → `local(imp, callee_cx, req)`,
    /// local-serialized → encode + task + decode).
    pub fn call<'a, Req, Rep, E, L, Fut>(&'a self, method: usize, cx: &'a CallContext,
        req: Req, local: L) -> impl Future<Output = Result<Rep, E>> + Send + 'a
    where
        Req: WireMessage, Rep: WireMessage, E: ComponentError,
        L: FnOnce(Arc<D>, CallContext, Req) -> Fut + Send + 'a,
        Fut: Future<Output = Result<Rep, E>> + Send + 'a;
    /// local_only components: always the local pipeline (a serialized route is
    /// impossible after a successful build; it yields INTERNAL, never a panic).
    pub fn call_local<'a, Req, Rep, E, L, Fut>(&'a self, method: usize, cx: &'a CallContext,
        req: Req, local: L) -> impl Future<Output = Result<Rep, E>> + Send + 'a
    where
        Req: LocalMessage, Rep: LocalMessage, E: ComponentError,
        L: FnOnce(Arc<D>, CallContext, Req) -> Fut + Send + 'a,
        Fut: Future<Output = Result<Rep, E>> + Send + 'a;
}

/// What a factory produced, assembled by generated code.
pub struct Local<D: ?Sized> { /* Arc<D>, Option<Arc<dyn Dispatch>>, Option<Arc<dyn LifecycleDyn>> */ }
impl<D: ?Sized + Send + Sync + 'static> Local<D> {
    pub fn new(imp: Arc<D>) -> Self;
    pub fn with_dispatch(self, dispatch: Arc<dyn Dispatch>) -> Self;
    pub fn with_lifecycle<L: Lifecycle>(self, lifecycle: Arc<L>) -> Self;
}
pub fn install_local<H, D, F>(app: &mut AppBuilder<'_>, make_handle: fn(Endpoint<D>) -> H,
    factory: F) -> Result<(), BuildError>
where
    H: ComponentHandle, D: ?Sized + Send + Sync + 'static,
    F: FnOnce(&mut Deps<'_>) -> Result<Local<D>, AppError> + Send + 'static;
pub fn install_remote<H, D>(app: &mut AppBuilder<'_>, make_handle: fn(Endpoint<D>) -> H)
    -> Result<(), BuildError>
where H: ComponentHandle, D: ?Sized + Send + Sync + 'static;

// Used by #[derive(ComponentError)].
pub fn matches(error: &AppError, reason: &str, domain: Option<&str>) -> bool; // reason ==, and domain == when Some
pub fn field<T: FromStr>(error: &AppError, key: &str) -> Option<T>;            // absent or unparseable → None
pub fn optional_field<T: FromStr>(error: &AppError, key: &str) -> Option<Option<T>>; // absent → Some(None), unparseable → None
```

`install_local` / `install_remote` detect duplicates immediately (same
`TypeId::of::<H>()` or same `H::DESCRIPTOR.name()`) and return
`DuplicateInstall`; everything else is checked in `build`. If a
`make_handle`/`factory` pair is installed for a `RemoteOnly` descriptor, or
`install_remote` for a non-`RemoteOnly` one, `build` reports the mode error of
section 2.6 (the macro never generates these combinations).

### 2.5 Call pipeline

All deadline arithmetic uses the tokio clock so that `tokio::time::pause`
tests are exact: deadlines are set as
`(tokio::time::Instant::now() + t).into_std()` through
`CallContext::with_deadline`, and time left is read with
`sekvent_resilience::remaining(cx)`. Never use `CallContext::with_timeout`,
`remaining` or `is_expired` inside this crate (they read the std clock).

Caller side, identical for both bindings (`Link`):

1. **Dead call shed.** `cx` cancelled → `CANCELLED`; `remaining(cx) == 0` →
   `DEADLINE_EXCEEDED`. Nothing is encoded, admitted or spawned.
2. **Callee context.** `callee = cx.child().with_caller(ServiceIdentity::trusted(LOCAL_CALLER))`,
   then, if the resolved method timeout is `Some(t)`, narrow its deadline to
   now + `t` (`with_deadline` keeps the earlier of the two). The idempotency
   key, request id, subject, tenant and traceparent are kept.
3. **Transport**, raced with `cx.cancelled()` (biased toward cancellation,
   which returns `CANCELLED` and drops the transport future) and wrapped in
   `Timeout::deadline_only().call(&callee, …)` (expiry → `DEADLINE_EXCEEDED`):
   - `local`: run the serving side (below) around `local(imp, callee, req)` in
     the caller's task. A typed error is normalized with
     `E::from_app_error(e.into_app_error())` so both bindings return identical
     values. A panic propagates to the caller, as a direct call would.
   - `local-serialized`: `body = req.encode_to_vec()`; `headers::inject(&callee, &mut HeaderMap)`;
     `tokio::spawn` (instrumented with the caller's current tracing span) a
     task that rebuilds `cx = headers::from_headers(&headers, Some(ServiceIdentity::trusted(LOCAL_CALLER)))`,
     runs the serving side around `dispatch.dispatch(method, cx, body)` and
     returns `Result<Bytes, Bytes>` (reply bytes, or a prost `WireErrorPb` of
     `AppError::to_wire()`). The caller awaits the `JoinHandle` through an
     `AbortOnDrop` guard, so dropping the caller's future (or cancellation,
     or the caller-side deadline) aborts the task. Reply bytes decode to
     `Rep`; error bytes decode to `WireError` → `AppError::from_wire` →
     `E::from_app_error`. A decode failure is `INTERNAL` / `MALFORMED_REPLY`;
     a panicked task is `INTERNAL` ("component … method … panicked", no
     payload).
4. Metadata `component` and `method` (names, never values) is added only by
   the side that creates the error: the caller side to its own errors (dead
   call shed, cancellation, caller-side deadline, reply decode failure,
   panic), the serving side to its own (admission, bulkhead, server-side
   deadline). Errors returned by the implementation are never touched. Every
   error then goes through `E::from_app_error`.

Serving side (`Server`, one per installed local component, shared by both
paths; C2's gRPC server reuses it):

1. **Admission.** Increment the in-flight counter first, then read the gate
   state; if it is not `Serving`, decrement and reject with `UNAVAILABLE` and
   reason `COMPONENT_NOT_STARTED` / `COMPONENT_DRAINING` /
   `COMPONENT_STOPPED`. The guard decrements on drop and wakes a drain
   waiter when the count reaches zero (increment-then-check closes the race
   with a concurrent drain).
2. **Dead call shed** again on the (possibly re-anchored) context.
3. **Bulkhead**, if the method has one: `Bulkhead::new(n)` without a queue;
   `acquire(&cx)` fails immediately with `RESOURCE_EXHAUSTED` /
   `BULKHEAD_FULL` when full.
4. Run the call under `Timeout::deadline_only().call(&cx, …)`, holding the
   admission guard and the bulkhead permit until it completes or is dropped.

### 2.6 Configuration

`<C>` is the component name upper-cased (`order_history` → `ORDER_HISTORY`),
`<M>` the method name upper-cased. All keys are optional.

| Key | Values | Applies to |
|---|---|---|
| `SEKVENT_COMPONENT_BINDING` | `local` (default), `local-serialized`, `grpc` | every standard component without its own binding key |
| `SEKVENT_COMPONENT_<C>_BINDING` | same | component `C` |
| `SEKVENT_COMPONENT_<C>_TIMEOUT` | humantime (`250ms`, `2s`) or whole seconds, > 0 | default for every method of `C` |
| `SEKVENT_COMPONENT_<C>_BULKHEAD_MAX_CONCURRENT` | integer ≥ 1 | default for every method of `C` (each method gets its own bulkhead) |
| `SEKVENT_COMPONENT_<C>_<M>_TIMEOUT` | as above | method `M` |
| `SEKVENT_COMPONENT_<C>_<M>_BULKHEAD_MAX_CONCURRENT` | as above | method `M` |

Precedence per field, highest first: method key, component key, `#[call]`
attribute, none. (C2 inserts named policies between the attribute and the
component key, using `sekvent_resilience::PolicySpec` layering.) Values are
read with `sekvent_config::__private::opt_duration_value(source, key)` and
`sekvent_config::__private::opt_value::<u32>(source, key)` (the helpers the
`EnvConfig` derive uses); malformed → `ConfigError::Malformed`,
zero → `ConfigError::Invalid`, both naming the key and never the value.
Binding values are exact (`Local` or ` local` are malformed; expected text
`one of local, local-serialized, grpc`).

Binding resolution:

| Mode | Own key set | Own key unset |
|---|---|---|
| Standard | that binding; `grpc` → `BindingUnavailable { key: own }` | `SEKVENT_COMPONENT_BINDING` or `local`; `grpc` → `BindingUnavailable { key: SEKVENT_COMPONENT_BINDING }` |
| LocalOnly | `local` only; otherwise `LocalOnly { key: own }` | `local` (the default key is ignored) |
| RemoteOnly | `grpc` → `BindingUnavailable`; `local`/`local-serialized` → `RemoteOnly` | `RemoteOnlyUnbound { key: own }` |

**Unknown keys.** The known set is `SEKVENT_COMPONENT_BINDING` plus, per
installed component, `<C>_BINDING`, `<C>_TIMEOUT`,
`<C>_BULKHEAD_MAX_CONCURRENT` and, per method, `<C>_<M>_TIMEOUT` and
`<C>_<M>_BULKHEAD_MAX_CONCURRENT` (all with the prefix). The check is
`sekvent_config::check_reserved(&Prefixed::new(source, CONFIG_PREFIX), &known)`:
it only sees keys under the prefix, reports them sorted and suggests the
closest known key. No other key under the prefix is accepted in C1 (C2 adds
its own).

**Collisions.** While building the known set, a key produced by two owners
is a `KeyCollision` naming the key and both owners — for example component
`a` with method `b_c` and component `a_b` with method `c` both produce
`SEKVENT_COMPONENT_A_B_C_TIMEOUT`. The check is exact string equality.

**`crates/sekvent-config/src/lib.rs`:** add `"SEKVENT_COMPONENT_"` as the
first entry of `FRAMEWORK_PREFIXES` (the list stays sorted), add the doc
bullet "`SEKVENT_COMPONENT_`: component bindings and policy overrides
(`sekvent-component` validates the names against the installed
components)", and extend `framework_keys_are_recognised` with
`assert!(is_framework_key("SEKVENT_COMPONENT_BINDING"))` and
`assert!(is_framework_key("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT"))`
plus a check that `FRAMEWORK_PREFIXES` is sorted if there is none yet.
Without this, a service's own `sekvent_config::load` rejects component keys.

### 2.7 Build and lifecycle algorithms

`AppBuilder::build`:

1. Known-key set and collisions (2.6), in install order.
2. Unknown-key check.
3. Read `SEKVENT_COMPONENT_BINDING`, then per component its binding and the
   policy keys; resolve bindings (2.6) and per-method `timeout: Option<Duration>`
   and `bulkhead: Option<u32>`.
4. Errors from steps 1–3 are collected (config errors from one component do
   not stop the others); if any, return them (bare or `Multiple`) without
   running a factory.
5. In install order: for a local binding, call the factory with a `Deps`
   that sees the components built so far and the provided resources; an
   error stops the build with `Factory { component, source }`. Create the
   `Server` (gate `NotStarted`, bulkheads) and `Link`; route `Local(imp)` for
   `local`, `Serialized(dispatch)` for `local-serialized`; build the handle
   with `make_handle` and store it as `Box<dyn Any + Send + Sync>` (cloned on
   every `handle::<H>()`). `debug!` the component, binding and resolved
   policies.

`App::start`: state `Built` → `Starting`. For each component in install
order: `on_start` (if any); on success its gate becomes `Serving`. On the
first failure, stop the already-started components in reverse (as below,
with zero grace), mark the App stopped and return the error with metadata
`component`. Otherwise the App is `Running`.

`App::stop(grace)`: `deadline = now + grace` (tokio clock). For each
component in reverse install order: gate → `Draining`; wait until its
in-flight count is zero or `deadline` passes (`warn!` naming the component
and the count when it passes); `on_stop` (if any; an error is remembered and
stopping continues); gate → `Stopped`. Returns the first remembered error.

`App::register` (feature `runtime`) adds one unit named `components` in
`Stage::Components` with `UnitPolicy::Critical`:
`start().await?; ctx.ready(); ctx.shutdown().cancelled().await; stop(ctx.stage_grace()).await`.
Ingress stops before components and infrastructure after them, so the
stage order complements the per-component order.

## 3. Macro contract

### 3.1 `#[component]` input grammar

```text
#[component( name = "<name>" [, package = "<package>"] [, local_only | , remote_only] [, crate = "<path>"] )]
<vis> trait <Trait> [: <Send | Sync | 'static> (+ ...)*] {
    ( <doc comments>*
      #[call] | #[call( idempotent | timeout = "<humantime>" | bulkhead = <int> ,* )]
      async fn <method>(&self, <ident|_>: &CallContext, <ident|_>: <Request>) -> Result<<Reply>, <Error>>; )+
}
```

- `name`: required; `[a-z][a-z0-9_]*`, no `__`, no trailing `_`, ≤ 48 chars.
- `package`: required unless `local_only`; dot-separated segments each
  `[a-z][a-z0-9_]*` (`shop.inventory.v1`).
- `crate = "::path::to::component"`: explicit runtime path (3.6).
- Missing `Send`, `Sync`, `'static` supertraits are added; any other
  supertrait is an error.
- `<method>`: `[a-z][a-z0-9_]*`, no `__`, no trailing `_`; not one of the
  handle's own names `install`, `install_with_lifecycle`, `install_remote`,
  `binding`, `clone`. RPC name = UpperCamelCase (`get_invoice` → `GetInvoice`).
- The context type is a shared reference without lifetime to a path whose
  last segment is `CallContext`. Request, reply and error are owned types
  with no lifetime and no `impl Trait` anywhere inside (token scan). The
  return type is a path whose last segment is `Result` with exactly two type
  arguments. Error types may differ per method; each must implement
  `ComponentError`.
- `timeout`: parsed with `humantime` at compile time, must be > 0, emitted as
  `Duration::new(secs, nanos)`. `bulkhead`: integer literal in `1..=u32::MAX`.

### 3.2 Compile errors (exact messages)

Every error is a `syn::Error` spanned on the offending tokens; independent
errors are combined so one compile shows all of them.

| # | Violation | Message |
|---|---|---|
| C1 | attribute on a non-trait item | `#[component] applies to a trait` |
| C2 | `name` missing | ``missing `name = "..."` `` |
| C3 | bad name | ``component name must match [a-z][a-z0-9_]*, without `__` or a trailing `_`, at most 48 characters`` |
| C4 | `package` missing (not local_only) | ``missing `package = "..."`; only a local_only component may omit it`` |
| C5 | bad package | ``package must be dot-separated lowercase segments such as `shop.inventory.v1` `` |
| C6 | both mode flags | `` `local_only` and `remote_only` exclude each other `` |
| C7 | unknown / duplicate argument | ``unknown component argument `x`; expected name, package, local_only, remote_only or crate`` / ``duplicate component argument `x` `` |
| C8 | generics or where clause on the trait | `a component trait cannot have generic parameters or a where clause` |
| C9 | `unsafe` / `auto` trait | `a component trait cannot be unsafe or auto` |
| C10 | other supertraits | ``a component trait may only have `Send`, `Sync` and `'static` as supertraits`` |
| C11 | associated type/const/macro | `a component trait may only contain methods` |
| C12 | no methods | `a component needs at least one method` |
| M1 | no kind attribute | ``method `x` needs a kind attribute: #[call]`` |
| M2 | `#[async_call]` / `#[deferred]` | ``#[async_call] is planned for milestone C3 and not available yet`` (same text with `#[deferred]`) |
| M3 | two kind attributes | `a method has exactly one kind attribute` |
| M4 | unknown / duplicate `#[call]` argument | ``unknown #[call] argument `x`; expected idempotent, timeout or bulkhead`` / ``duplicate #[call] argument `x` `` |
| M5 | bad timeout | `timeout must be a positive duration such as "2s" or "250ms"` |
| M6 | bad bulkhead | `bulkhead must be an integer from 1 to 4294967295` |
| M7 | other attribute on a method | `only doc comments and the kind attribute are allowed on a component method` |
| M8 | not `async` | `` component methods must be `async fn` `` |
| M9 | `const` / `unsafe` / `extern` | `component methods cannot be const, unsafe or extern` |
| M10 | default body | `component methods cannot have a default body` |
| M11 | generics, lifetimes, where clause | `component methods cannot have generic parameters, lifetimes or a where clause` |
| M12 | receiver other than `&self` | `` component methods take `&self` `` |
| M13 | wrong arity | `` component methods take `&self`, `cx: &CallContext` and one request `` |
| M14 | bad context argument | `` the first argument must be `cx: &CallContext` `` |
| M15 | argument pattern not an identifier or `_` | `arguments must be plain identifiers` |
| M16 | bad request type | `` the request must be an owned type without lifetimes or `impl Trait` `` |
| M17 | bad return type | `the return type must be Result<Reply, Error>` |
| M18 | reply is `()` | `a #[call] method returns a reply message, not ()` |
| M19 | reply/error with lifetime or `impl Trait` | `` reply and error types cannot contain lifetimes or `impl Trait` `` |
| M20 | bad method name | `` method names must be snake_case ([a-z][a-z0-9_]*, without `__` or a trailing `_`) `` |
| M21 | reserved method name | ``method name `install` is reserved on the generated handle`` |

Type-level rules the macro cannot see are enforced by rustc through the
`__private::assert_*` calls (non-prost request/reply, error without
`ComponentError`, non-`Send` types); the exact rustc wording is whatever the
toolchain prints and is recorded in the trybuild `.stderr` files.

### 3.3 Generated items (summary)

For trait `X` with visibility `V`: the rewritten trait `X`; `#[doc(hidden)] V
trait __XDyn` (object-safe, boxed futures, methods renamed `__<method>` so a
glob import of the API crate never makes method calls ambiguous) with a
blanket impl for `T: X`; `V struct XHandle` (`Clone`, `Debug`) with one
method per trait method plus `binding`, `install`,
`install_with_lifecycle` (or `install_remote`) and the hidden
`__METHODS`; `impl ComponentHandle for XHandle`; `#[doc(hidden)] V struct
__XDispatcher` with `impl Dispatch` (not for `local_only`); one
`const _: () = { … };` block of type assertions. All paths are absolute
(`::core`, `::std`, `#krate`); generated impls carry
`#[allow(clippy::all, clippy::pedantic)]`; the handle and its methods have
docs (trait method docs are copied), hidden items do not need any. The
generated code must compile in edition 2021 crates too: no let-chains, and
explicit `'a` on the handle methods' `impl Future` return.

### 3.4 Complete expansion of the reference component

Input (in `inventory-api`, `K` below is the resolved runtime path; shown as
`::sekvent_component`):

```rust
#[sekvent::component(name = "inventory", package = "shop.inventory.v1")]
pub trait Inventory: Send + Sync + 'static {
    /// Reserve stock for an order.
    #[call(idempotent, timeout = "2s", bulkhead = 16)]
    async fn reserve(&self, cx: &CallContext, req: ReserveRequest)
        -> Result<ReserveReply, InventoryError>;

    /// Release a reservation.
    #[call(timeout = "500ms")]
    async fn release(&self, cx: &CallContext, req: ReleaseRequest)
        -> Result<ReleaseReply, InventoryError>;
}
```

Output (this exact text, formatted, is the insta snapshot
`component__standard.snap`, and `crates/sekvent-component/tests/support/inventory.rs`
contains it verbatim, with hand-written prost messages and a hand-expanded
`InventoryError`, so the framework's tests do not depend on the macro):

```rust
pub trait Inventory: Send + Sync + 'static {
    /// Reserve stock for an order.
    fn reserve(&self, cx: &CallContext, req: ReserveRequest)
        -> impl ::core::future::Future<Output = Result<ReserveReply, InventoryError>>
        + ::core::marker::Send;
    /// Release a reservation.
    fn release(&self, cx: &CallContext, req: ReleaseRequest)
        -> impl ::core::future::Future<Output = Result<ReleaseReply, InventoryError>>
        + ::core::marker::Send;
}

#[doc(hidden)]
pub trait __InventoryDyn: ::core::marker::Send + ::core::marker::Sync + 'static {
    fn __reserve<'a>(&'a self, cx: &'a CallContext, req: ReserveRequest)
        -> ::sekvent_component::__private::BoxFuture<'a, Result<ReserveReply, InventoryError>>;
    fn __release<'a>(&'a self, cx: &'a CallContext, req: ReleaseRequest)
        -> ::sekvent_component::__private::BoxFuture<'a, Result<ReleaseReply, InventoryError>>;
}

#[allow(clippy::all, clippy::pedantic)]
impl<T: Inventory> __InventoryDyn for T {
    fn __reserve<'a>(&'a self, cx: &'a CallContext, req: ReserveRequest)
        -> ::sekvent_component::__private::BoxFuture<'a, Result<ReserveReply, InventoryError>>
    {
        ::std::boxed::Box::pin(<T as Inventory>::reserve(self, cx, req))
    }
    fn __release<'a>(&'a self, cx: &'a CallContext, req: ReleaseRequest)
        -> ::sekvent_component::__private::BoxFuture<'a, Result<ReleaseReply, InventoryError>>
    {
        ::std::boxed::Box::pin(<T as Inventory>::release(self, cx, req))
    }
}

/// Cloneable handle to the [`Inventory`] component (`inventory`).
///
/// Get it from `Deps::handle` in a factory or from `App::handle`; the App
/// builder decides whether its calls run in-process or across a
/// serialization boundary.
#[derive(Clone, Debug)]
pub struct InventoryHandle(::sekvent_component::__private::Endpoint<dyn __InventoryDyn>);

#[allow(clippy::all, clippy::pedantic)]
impl InventoryHandle {
    #[doc(hidden)]
    pub const __METHODS: &'static [::sekvent_component::MethodDescriptor] = &[
        ::sekvent_component::MethodDescriptor::call("reserve", "Reserve")
            .with_idempotent()
            .with_timeout(::core::time::Duration::new(2u64, 0u32))
            .with_bulkhead(16u32),
        ::sekvent_component::MethodDescriptor::call("release", "Release")
            .with_timeout(::core::time::Duration::new(0u64, 500000000u32)),
    ];

    /// Reserve stock for an order.
    pub fn reserve<'a>(&'a self, cx: &'a CallContext, req: ReserveRequest)
        -> impl ::core::future::Future<Output = Result<ReserveReply, InventoryError>>
        + ::core::marker::Send + 'a
    {
        self.0.call(0usize, cx, req,
            |imp: ::std::sync::Arc<dyn __InventoryDyn>,
             cx: ::sekvent_component::CallContext,
             req: ReserveRequest| async move { __InventoryDyn::__reserve(&*imp, &cx, req).await })
    }

    /// Release a reservation.
    pub fn release<'a>(&'a self, cx: &'a CallContext, req: ReleaseRequest)
        -> impl ::core::future::Future<Output = Result<ReleaseReply, InventoryError>>
        + ::core::marker::Send + 'a
    {
        self.0.call(1usize, cx, req,
            |imp: ::std::sync::Arc<dyn __InventoryDyn>,
             cx: ::sekvent_component::CallContext,
             req: ReleaseRequest| async move { __InventoryDyn::__release(&*imp, &cx, req).await })
    }

    /// The binding this handle's calls use.
    pub fn binding(&self) -> ::sekvent_component::Binding {
        self.0.binding()
    }

    /// Install the implementation `factory` builds. The factory runs during
    /// `AppBuilder::build`, in install order, and only when the component is
    /// bound `local` or `local-serialized`.
    pub fn install<T, F>(app: &mut ::sekvent_component::AppBuilder<'_>, factory: F)
        -> ::core::result::Result<(), ::sekvent_component::BuildError>
    where
        T: Inventory,
        F: ::core::ops::FnOnce(&mut ::sekvent_component::Deps<'_>)
            -> ::core::result::Result<T, ::sekvent_component::AppError>
            + ::core::marker::Send + 'static,
    {
        ::sekvent_component::__private::install_local(app, InventoryHandle, move |deps| {
            let imp: ::std::sync::Arc<dyn __InventoryDyn> = ::std::sync::Arc::new(factory(deps)?);
            let dispatch = ::std::sync::Arc::new(__InventoryDispatcher(::std::sync::Arc::clone(&imp)));
            ::core::result::Result::Ok(
                ::sekvent_component::__private::Local::new(imp).with_dispatch(dispatch))
        })
    }

    /// Like [`InventoryHandle::install`], and also run the implementation's
    /// `Lifecycle` hooks when the App starts and stops.
    pub fn install_with_lifecycle<T, F>(app: &mut ::sekvent_component::AppBuilder<'_>, factory: F)
        -> ::core::result::Result<(), ::sekvent_component::BuildError>
    where
        T: Inventory + ::sekvent_component::Lifecycle,
        F: ::core::ops::FnOnce(&mut ::sekvent_component::Deps<'_>)
            -> ::core::result::Result<T, ::sekvent_component::AppError>
            + ::core::marker::Send + 'static,
    {
        ::sekvent_component::__private::install_local(app, InventoryHandle, move |deps| {
            let concrete = ::std::sync::Arc::new(factory(deps)?);
            let imp: ::std::sync::Arc<dyn __InventoryDyn> = ::std::sync::Arc::<T>::clone(&concrete);
            let dispatch = ::std::sync::Arc::new(__InventoryDispatcher(::std::sync::Arc::clone(&imp)));
            ::core::result::Result::Ok(::sekvent_component::__private::Local::new(imp)
                .with_dispatch(dispatch)
                .with_lifecycle(concrete))
        })
    }
}

#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::ComponentHandle for InventoryHandle {
    const DESCRIPTOR: &'static ::sekvent_component::ComponentDescriptor =
        &::sekvent_component::ComponentDescriptor::new("inventory", "Inventory", InventoryHandle::__METHODS)
            .with_package("shop.inventory.v1");
}

#[doc(hidden)]
pub struct __InventoryDispatcher(::std::sync::Arc<dyn __InventoryDyn>);

#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::__private::Dispatch for __InventoryDispatcher {
    fn dispatch(&self, method: usize, cx: ::sekvent_component::CallContext,
                body: ::sekvent_component::__private::Bytes)
        -> ::sekvent_component::__private::BoxFuture<'static,
            ::core::result::Result<::sekvent_component::__private::Bytes, ::sekvent_component::AppError>>
    {
        let imp = ::std::sync::Arc::clone(&self.0);
        match method {
            0usize => ::sekvent_component::__private::serve(cx, body,
                move |cx: ::sekvent_component::CallContext, req: ReserveRequest| async move {
                    __InventoryDyn::__reserve(&*imp, &cx, req).await
                }),
            1usize => ::sekvent_component::__private::serve(cx, body,
                move |cx: ::sekvent_component::CallContext, req: ReleaseRequest| async move {
                    __InventoryDyn::__release(&*imp, &cx, req).await
                }),
            _ => ::sekvent_component::__private::unknown_method(
                <InventoryHandle as ::sekvent_component::ComponentHandle>::DESCRIPTOR, method),
        }
    }
}

const _: () = {
    ::sekvent_component::__private::assert_wire::<ReserveRequest>();
    ::sekvent_component::__private::assert_wire::<ReserveReply>();
    ::sekvent_component::__private::assert_error::<InventoryError>();
    ::sekvent_component::__private::assert_wire::<ReleaseRequest>();
    ::sekvent_component::__private::assert_wire::<ReleaseReply>();
    ::sekvent_component::__private::assert_error::<InventoryError>();
};
```

### 3.5 Mode differences

- **`local_only`** (`#[component(name = "notes", local_only)]`): handle
  methods call `self.0.call_local(…)` instead of `self.0.call(…)`; no
  `__XDispatcher` and no `Dispatch` impl; the install functions build
  `Local::new(imp)` without `.with_dispatch(…)`; assertions use
  `assert_local::<Req>()` / `assert_local::<Rep>()` (plus `assert_error`);
  the descriptor adds `.with_mode(::sekvent_component::ComponentMode::LocalOnly)`
  and `.with_package(…)` only when a package was given.
- **`remote_only`**: everything as for a standard component except that
  `install` and `install_with_lifecycle` are replaced by

  ```rust
  /// Declare this remote-only component; it must be bound to a remote transport.
  pub fn install_remote(app: &mut ::sekvent_component::AppBuilder<'_>)
      -> ::core::result::Result<(), ::sekvent_component::BuildError>
  {
      ::sekvent_component::__private::install_remote(app, InventoryHandle)
  }
  ```

  and the descriptor adds `.with_mode(::sekvent_component::ComponentMode::RemoteOnly)`.
  The dispatcher is still generated (C2 test harnesses serve fakes with it).

### 3.6 Runtime path

Generalize `runtime_path` of `src/env_config.rs` into `src/paths.rs`:

```rust
/// `direct` = crate_name(<package>), `facade` = crate_name("sekvent").
pub(crate) fn runtime_path(direct: Option<FoundCrate>, facade: impl FnOnce() -> Option<FoundCrate>,
                           lib: &str, module: &str) -> TokenStream;
pub(crate) fn resolve(package: &str, lib: &str, module: &str) -> TokenStream;
```

Order: explicit `crate = "…"` argument; direct dependency (`Name(n)` →
`::n`, `Itself` → `::<lib>`); the facade (`Name(n)` → `::n::<module>`,
`Itself` → `crate::<module>`); fallback `::<lib>`. `EnvConfig` uses
`("sekvent-config", "sekvent_config", "config")` and keeps its behaviour and
tests; `component` and `ComponentError` use
`("sekvent-component", "sekvent_component", "component")`. Snapshot tests
call the expansion functions with an explicit path, never through
`proc_macro_crate`.

### 3.7 `#[derive(ComponentError)]`

```text
#[derive(ComponentError)]
[#[component_error( domain = "<domain>" , crate = "<path>" )]]    // both optional
<vis> enum <Error> {
    #[reason("<REASON>", code = <ErrorCode variant> [, message = "<format string>"])]
    <Variant> [ { <field>: <Type>, ... } ],
    ...
    #[other]
    <Other>(<AppError>),
}
```

Rules and errors (spanned like 3.2):

| # | Violation | Message |
|---|---|---|
| D1 | not an enum | `ComponentError can only be derived for an enum` |
| D2 | generic enum | `ComponentError cannot be derived for a generic enum` |
| D3 | no `#[other]` | `exactly one variant needs #[other] to catch unknown reasons` |
| D4 | two `#[other]` | `only one variant can be #[other]` |
| D5 | `#[other]` not a one-field tuple | ``the #[other] variant must be a tuple variant holding one AppError, e.g. `Other(AppError)` `` |
| D6 | variant without either attribute | ``variant `X` needs #[reason("REASON", code = ...)] or #[other]`` |
| D7 | both, or repeated | `a variant has either one #[reason(...)] or #[other]` |
| D8 | bad reason | `reason must be UPPER_SNAKE_CASE ([A-Z][A-Z0-9_]*) and at most 63 characters` |
| D9 | duplicate reason | ``reason `X` is used by more than one variant`` |
| D10 | `code` missing | ``missing `code = ...` (an ErrorCode variant such as NotFound)`` |
| D11 | unknown code | ``unknown error code `X`; expected one of Cancelled, Unknown, InvalidArgument, DeadlineExceeded, NotFound, AlreadyExists, PermissionDenied, ResourceExhausted, FailedPrecondition, Aborted, OutOfRange, Unimplemented, Internal, Unavailable, DataLoss, Unauthenticated`` |
| D12 | `code = Ok` | `` `Ok` is not an error code `` |
| D13 | tuple variant with `#[reason]` | `use named fields (they become error metadata keys) or no fields` |
| D14 | unknown argument | ``unknown #[reason] argument `x`; expected code or message`` / ``unknown #[component_error] argument `x`; expected domain or crate`` |
| D15 | empty domain or whitespace in it | `domain must be non-empty and contain no whitespace` |

Semantics: metadata key = field name; value = `ToString`; decoding parses
with `FromStr` (`__private::field`). A field whose type is syntactically
`Option<U>` is written only when `Some` and decodes absent as `None`. The
message is the `message` format string (it may name the variant's fields,
`"only {available} of {sku} left"`), otherwise the reason in lower case with
spaces (`OUT_OF_STOCK` → `out of stock`). With a `domain`, encoding sets it
and decoding requires it to match. The `#[other]` variant encodes as its
`AppError` unchanged. The derive also implements `From<AppError> for E`
(via `from_app_error`) and `From<E> for AppError` (via `into_app_error`), so
do not add `#[from] AppError` elsewhere.

Expansion for

```rust
#[derive(Debug, ComponentError)]
#[component_error(domain = "shop.inventory.v1")]
pub enum InventoryError {
    #[reason("OUT_OF_STOCK", code = FailedPrecondition, message = "only {available} of {sku} left")]
    OutOfStock { sku: String, available: u32 },
    #[reason("RESERVATION_NOT_FOUND", code = NotFound)]
    ReservationNotFound { reservation_id: String, hint: Option<String> },
    #[reason("INVENTORY_CLOSED", code = Unavailable)]
    Closed,
    #[other]
    Other(AppError),
}
```

is (snapshot `component_error__inventory.snap`; `K` = `::sekvent_component`):

```rust
const _: () = {
    ::sekvent_component::__private::assert_metadata::<String>();
    ::sekvent_component::__private::assert_metadata::<u32>();
    ::sekvent_component::__private::assert_metadata::<String>();
    ::sekvent_component::__private::assert_metadata::<String>();
};

#[automatically_derived]
#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::ComponentError for InventoryError {
    fn into_app_error(self) -> ::sekvent_component::AppError {
        match self {
            Self::OutOfStock { sku, available } => ::sekvent_component::AppError::new(
                    ::sekvent_component::ErrorCode::FailedPrecondition,
                    ::std::format!("only {available} of {sku} left"))
                .with_reason("OUT_OF_STOCK")
                .with_domain("shop.inventory.v1")
                .with_metadata("sku", ::std::string::ToString::to_string(&sku))
                .with_metadata("available", ::std::string::ToString::to_string(&available)),
            Self::ReservationNotFound { reservation_id, hint } => {
                let error = ::sekvent_component::AppError::new(
                        ::sekvent_component::ErrorCode::NotFound,
                        ::std::string::String::from("reservation not found"))
                    .with_reason("RESERVATION_NOT_FOUND")
                    .with_domain("shop.inventory.v1")
                    .with_metadata("reservation_id", ::std::string::ToString::to_string(&reservation_id));
                match hint {
                    ::core::option::Option::Some(value) =>
                        error.with_metadata("hint", ::std::string::ToString::to_string(&value)),
                    ::core::option::Option::None => error,
                }
            }
            Self::Closed => ::sekvent_component::AppError::new(
                    ::sekvent_component::ErrorCode::Unavailable,
                    ::std::string::String::from("inventory closed"))
                .with_reason("INVENTORY_CLOSED")
                .with_domain("shop.inventory.v1"),
            Self::Other(error) => error,
        }
    }

    fn from_app_error(error: ::sekvent_component::AppError) -> Self {
        if ::sekvent_component::__private::matches(&error, "OUT_OF_STOCK",
                ::core::option::Option::Some("shop.inventory.v1")) {
            match (::sekvent_component::__private::field::<String>(&error, "sku"),
                   ::sekvent_component::__private::field::<u32>(&error, "available")) {
                (::core::option::Option::Some(sku), ::core::option::Option::Some(available)) =>
                    return Self::OutOfStock { sku, available },
                _ => {}
            }
        }
        if ::sekvent_component::__private::matches(&error, "RESERVATION_NOT_FOUND",
                ::core::option::Option::Some("shop.inventory.v1")) {
            match (::sekvent_component::__private::field::<String>(&error, "reservation_id"),
                   ::sekvent_component::__private::optional_field::<String>(&error, "hint")) {
                (::core::option::Option::Some(reservation_id), ::core::option::Option::Some(hint)) =>
                    return Self::ReservationNotFound { reservation_id, hint },
                _ => {}
            }
        }
        if ::sekvent_component::__private::matches(&error, "INVENTORY_CLOSED",
                ::core::option::Option::Some("shop.inventory.v1")) {
            return Self::Closed;
        }
        Self::Other(error)
    }
}

#[automatically_derived]
impl ::core::convert::From<::sekvent_component::AppError> for InventoryError {
    fn from(error: ::sekvent_component::AppError) -> Self {
        <Self as ::sekvent_component::ComponentError>::from_app_error(error)
    }
}

#[automatically_derived]
impl ::core::convert::From<InventoryError> for ::sekvent_component::AppError {
    fn from(error: InventoryError) -> Self {
        <InventoryError as ::sekvent_component::ComponentError>::into_app_error(error)
    }
}
```

A variant with one field uses a one-element tuple match `(… ,)`. Without a
`domain`, `matches` gets `::core::option::Option::None` and `.with_domain`
is omitted.

## 4. Facade

`crates/sekvent/Cargo.toml`:

```toml
[features]
component = ["dep:sekvent-component", "config", "error", "context"]
runtime = ["dep:sekvent-runtime", "sekvent-component?/runtime"]   # was ["dep:sekvent-runtime"]
full = [ ..., "component" ]                                         # add to the list

[dependencies]
sekvent-component = { workspace = true, optional = true }
```

`crates/sekvent/src/lib.rs` (plus a `component` row in the crate docs'
feature table):

```rust
#[cfg(feature = "component")]
pub use sekvent_component as component;                        // sekvent::component::App, …
#[cfg(feature = "component")]
pub use sekvent_component::{component, App, ComponentError};  // #[sekvent::component], derive + trait

pub mod prelude {
    // existing entries unchanged
    #[cfg(feature = "component")]
    pub use sekvent_component::{App, ComponentError, Lifecycle};
}
```

The module alias and the attribute share the name `component` in different
namespaces (like serde's trait and derive), so `sekvent::component::Binding`
and `#[sekvent::component(...)]` both resolve. The macro finds the runtime
as `::sekvent::component` when a crate depends only on the facade (3.6).

`crates/sekvent-facade-check`: dependencies become
`sekvent = { path = "../sekvent", default-features = false, features = ["config", "component"] }`
and `prost = { workspace = true }`; dev-dependency
`tokio = { workspace = true }`. New `src/component.rs` (declared from
`lib.rs`) with, through the facade only: two hand-written
`#[derive(prost::Message)]` messages `PingRequest { text }` /
`PingReply { text }`; `#[derive(Debug, sekvent::ComponentError)] enum EchoError { #[reason("EMPTY_TEXT", code = InvalidArgument)] EmptyText, #[other] Other(AppError) }`;
`#[sekvent::component(name = "echo", package = "check.echo.v1")] trait Echo { #[call(timeout = "1s")] async fn ping(...) }`;
and a `local_only` component `notes` whose request is a `String` and reply a
`usize`. Tests: build an App under `local` and under `local-serialized`
(via `MapSource` with `SEKVENT_COMPONENT_BINDING`), start it, call `ping`,
get `EchoError::EmptyText` back for an empty text under both bindings, and
call the `notes` component.

## 5. `examples/shop`

### 5.1 Layout and root change

```text
examples/shop/
  README.md                     what the example shows, how to run the tests
  inventory-api/                Cargo.toml, build.rs, proto/shop/inventory/v1/inventory.proto, src/lib.rs
  inventory/                    InventoryService (in-memory stock and reservations)
  notifications-api/            ... proto/shop/notifications/v1/notifications.proto
  notifications/                NotificationsService (in-memory outbox, blocklist, Lifecycle)
  orders-api/                   ... proto/shop/orders/v1/orders.proto
  orders/                       OrdersService (in-memory orders; calls inventory and notifications)
  shop/                         lib (wiring), thin main.rs, tests/
```

Root `Cargo.toml` (edited by agent A):

```toml
members = [
    "crates/*",
    "examples/shop/inventory-api",
    "examples/shop/inventory",
    "examples/shop/notifications-api",
    "examples/shop/notifications",
    "examples/shop/orders-api",
    "examples/shop/orders",
    "examples/shop/shop",
]
```

Explicit paths, not a glob, so `examples/shop/README.md` is never taken for
a member. Example crates are never added to `[workspace.dependencies]`.
Every example manifest uses the workspace `package` fields and
`[lints] workspace = true`; framework crates by relative path
(`sekvent = { path = "../../../crates/sekvent", default-features = false, features = ["component"] }`,
build-dependency `sekvent-proto-build = { path = "../../../crates/sekvent-proto-build" }`),
sibling example crates by relative path (`inventory-api = { path = "../inventory-api" }`),
third-party crates with `{ workspace = true }` (`prost`, `tokio`, `tracing`,
dev: `rstest`, `tokio` with `test-util`). `shop` enables
`["component", "config", "runtime"]`.

### 5.2 API crates

`build.rs` (all three):

```rust
fn main() {
    sekvent_proto_build::ProtoBuild::new("proto")
        .messages_only()
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
```

`src/lib.rs` shape (inventory shown; docs on every public item):

```rust
//! Contract of the inventory component.
/// Messages of the `shop.inventory.v1` package.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
}
use proto::shop::inventory::v1::{ReleaseReply, ReleaseRequest, ReserveReply, ReserveRequest, StockReply, StockRequest};
use sekvent::prelude::*;
// InventoryError: as in 3.7 but with the variants below.
// trait Inventory: as in 3.4 plus `stock`.
```

| Package | Messages | Component (methods, attributes) | Error variants (reason, code) |
|---|---|---|---|
| `shop.inventory.v1` | `ReserveRequest{order_id, sku, quantity:uint32}`, `ReserveReply{reservation_id, remaining:uint32}`, `ReleaseRequest{reservation_id}`, `ReleaseReply{released:bool}`, `StockRequest{sku}`, `StockReply{sku, available:uint32}` | `inventory`: `reserve` (idempotent, 2s, bulkhead 16), `release` (500ms), `stock` (idempotent, 500ms) | `OutOfStock{sku, available:u32}` (`OUT_OF_STOCK`, FailedPrecondition, message `only {available} of {sku} left`), `UnknownSku{sku}` (`UNKNOWN_SKU`, NotFound), `ReservationNotFound{reservation_id}` (`RESERVATION_NOT_FOUND`, NotFound); domain `shop.inventory.v1` |
| `shop.notifications.v1` | `NotifyRequest{customer_id, order_id, template}`, `NotifyReply{notification_id}`, `SentRequest{customer_id}`, `SentReply{repeated Notification notifications}`, `Notification{notification_id, customer_id, order_id, template, tenant}` | `notifications`: `notify` (1s), `sent` (idempotent) | `RecipientBlocked{customer_id}` (`RECIPIENT_BLOCKED`, FailedPrecondition) |
| `shop.orders.v1` | `PlaceOrderRequest{customer_id, sku, quantity:uint32}`, `PlaceOrderReply{order_id, reservation_id}`, `GetOrderRequest{order_id}`, `Order{order_id, customer_id, sku, quantity:uint32, status:OrderStatus}`, `enum OrderStatus{ORDER_STATUS_UNSPECIFIED, ORDER_STATUS_PLACED}` | `orders`: `place_order` (5s), `get_order` (idempotent, 1s) | `InvalidQuantity{quantity:u32}` (`INVALID_QUANTITY`, InvalidArgument), `OrderNotFound{order_id}` (`ORDER_NOT_FOUND`, NotFound), `OutOfStock{sku, available:u32}` (`OUT_OF_STOCK`, FailedPrecondition), `UnknownSku{sku}` (`UNKNOWN_SKU`, NotFound), `CustomerBlocked{customer_id}` (`CUSTOMER_BLOCKED`, FailedPrecondition); domain `shop.orders.v1` |

Unlisted string fields are `string`; every error enum also has
`#[other] Other(AppError)`. No proto imports another package.

### 5.3 Implementations and flow

State is a `std::sync::Mutex<…>` never held across `.await`; ids come from
per-instance counters (`ord-1`, `res-1`, `ntf-1`) so tests can assert them.

- `InventoryService::new(stock: impl IntoIterator<Item = (String, u32)>)`:
  `reserve` fails `UnknownSku` / `OutOfStock { available }` or decrements
  and records the reservation; `release` restores it (unknown id →
  `ReservationNotFound`); `stock` reads.
- `NotificationsService::new(blocked: impl IntoIterator<Item = String>)`
  implements `Lifecycle` (opens on `on_start`, closes on `on_stop`) and is
  installed with `install_with_lifecycle`. `notify` fails
  `RecipientBlocked` for a blocked customer, otherwise stores a
  `Notification` whose `tenant` is `cx.tenant()` (so tests see the context
  crossing the binding); `sent` lists a customer's notifications.
- `OrdersService::new(inventory: InventoryHandle, notifications: NotificationsHandle)`,
  built with `deps.handle::<…>()?`. `place_order`: quantity 0 →
  `InvalidQuantity` (no call made); reserve (map `OutOfStock` and
  `UnknownSku` to the orders variants, anything else to `Other(error.into())`);
  notify; if notify fails, release the reservation (a release failure is
  logged with `tracing::warn!` and otherwise ignored) and return
  `CustomerBlocked` for `RecipientBlocked`, `Other` otherwise; store the
  order as `ORDER_STATUS_PLACED`. `get_order` → `OrderNotFound` if unknown.

`shop` lib:

```rust
pub struct ShopOptions { pub stock: Vec<(String, u32)>, pub blocked_customers: Vec<String> }
impl ShopOptions { pub fn demo() -> Self; }
pub fn install_inventory(app: &mut AppBuilder<'_>, stock: Vec<(String, u32)>) -> Result<(), BuildError>;
pub fn install_notifications(app: &mut AppBuilder<'_>, blocked: Vec<String>) -> Result<(), BuildError>;
pub fn install_orders(app: &mut AppBuilder<'_>) -> Result<(), BuildError>;
pub fn install(app: &mut AppBuilder<'_>, options: ShopOptions) -> Result<(), BuildError>; // the three, in this order
```

`main.rs`: `App::builder(&EnvSource)`, `shop::install(.., ShopOptions::demo())`,
`build()`, `app.register(Runtime::builder()).build()?.run().await`.

### 5.4 Tests (`examples/shop/shop/tests/`)

`support/mod.rs`:

```rust
#[derive(Debug, Clone, Copy)]
pub enum Profile { MonolithLocal, MonolithSerialized }
impl Profile {
    /// monolith-local: no keys (local is the default);
    /// monolith-serialized: SEKVENT_COMPONENT_BINDING=local-serialized.
    pub fn source(self) -> MapSource;
    pub fn binding(self) -> Binding;
}
/// Build with `install`, assert every component's binding equals `profile.binding()`, start.
pub async fn started(source: &MapSource, profile: Profile,
                     install: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>) -> App;
```

plus fakes implementing `Inventory` (installed through
`InventoryHandle::install` next to the real notifications and orders):
`GatedInventory` (reserve reports entry on an `mpsc` channel, then waits for
a `tokio::sync::Semaphore` permit), `PendingInventory` (reserve reports entry,
holds a guard whose `Drop` fires a `oneshot`, then never completes) and
`FutureReasonInventory` (reserve returns
`Other(AppError::failed_precondition("…").with_reason("FROM_THE_FUTURE"))`).

Every test runs under both profiles in one `cargo test`:

```rust
#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]            // `(start_paused = true)` for the deadline tests
async fn name(#[case] profile: Profile) { ... }
```

`flows.rs`: an order reserves stock, notifies and is readable; over-stock is
`OrdersError::OutOfStock { available }` with stock unchanged; unknown SKU is
`OrdersError::UnknownSku`; quantity 0 is `InvalidQuantity`; a blocked
customer gets `CustomerBlocked` and the reservation is released (stock
restored); an unknown order is `OrderNotFound`; the tenant set on the
caller's context is recorded on the notification.

`faults.rs` (C1 fault injection):

| Test | Setup | Expectation |
|---|---|---|
| method deadline | `PendingInventory`, paused clock | `InventoryError::Other(e)`, `DeadlineExceeded`, virtual elapsed ≥ 2 s and < 3 s |
| caller deadline wins | same, caller deadline 100 ms | `DeadlineExceeded` after ≥ 100 ms and < 2 s |
| config overrides timeout | `SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT=50ms` | `DeadlineExceeded` after ≥ 50 ms and < 100 ms |
| bulkhead full | `…_INVENTORY_RESERVE_BULKHEAD_MAX_CONCURRENT=2`, `GatedInventory`; two spawned calls entered | third call `ResourceExhausted` / `BULKHEAD_FULL` at once; after two permits both spawned calls succeed |
| dead context | cancelled context; expired context | `Cancelled` / `DeadlineExceeded`; the fake saw no entry |
| typed error round trip | real services | `InventoryError::OutOfStock { sku, available }` from `inventory.reserve` directly |
| unknown reason | `FutureReasonInventory` | `InventoryError::Other(e)`, reason `FROM_THE_FUTURE`, code `FailedPrecondition` |
| caller drop aborts | `PendingInventory`; spawn the call, await entry, abort the caller task | the guard's `oneshot` resolves (the call's future was dropped; under `local-serialized`, the task was aborted) |
| drain rejects | `GatedInventory`; one call in flight; spawn `app.stop(30 s)`; `yield_now` until `app.state("inventory") == Some(Draining)` | new call `Unavailable` / `COMPONENT_DRAINING`; after a permit the in-flight call succeeds, `stop` returns `Ok`, state `Stopped`, next call `Unavailable` / `COMPONENT_STOPPED` |

`runtime.rs`: the App registered into `Runtime::builder().without_signals()`,
`start()`, one order placed, `shutdown()`, `wait()` is `Ok`, every component
is `Stopped`.

## 6. Test and coverage plan

All tests are deterministic: `#[tokio::test(start_paused = true)]` for
anything timed, channels / `Notify` / `Semaphore` / `oneshot` as completion
signals, `yield_now` loops only to let a spawned task reach a state; no
sleeps.

**`sekvent-component` (≥ 95 % lines, `rrb run coverage`).** Unit tests next
to each module plus integration tests that use
`tests/support/inventory.rs` (the 3.4 expansion by hand), never the macro:

- `binding`, `descriptor`: parse/display of every binding, malformed
  values; const builders and getters; `full_service_name`.
- `config`: every key and the precedence method > component > attribute;
  malformed and zero values name the key and never contain the value; the
  nine cells of the binding table; `SEKVENT_COMPONENT_BINDING`; unknown keys
  sorted with a suggestion for a typo; a collision.
- `app`: duplicate install by type and by name; duplicate resource; all
  configuration errors reported together and no factory run; factory error;
  `Deps::handle` for a missing, later-installed and self dependency;
  `Deps::resource`; `App::handle` of an uninstalled component; `Debug`.
- pipeline, each under both bindings: success; typed error round trip
  (identical values under both bindings); unknown reason → `Other`; a field
  that fails to parse → `Other`; method, caller and configured deadlines;
  bulkhead shed and permit release; cancelled and expired contexts rejected
  without admission; caller cancellation mid-call; dropping the caller
  aborts the serialized task; a panicking method is `INTERNAL` under
  `local-serialized`; garbage request bytes through `Dispatch` are
  `INVALID_ARGUMENT` / `MALFORMED_REQUEST`; an unknown method index is
  `UNIMPLEMENTED`; the callee sees the request id, subject, tenant,
  idempotency key, `caller == trusted("local")` and a deadline no later
  than the caller's.
- lifecycle: start in install order, stop in reverse (a shared recorder);
  start failure rolls back; stop hook error reported after all stopped;
  drain waits for in-flight calls; grace expiry still stops; `NotStarted`,
  `Draining`, `Stopped` rejections; `start` twice; `stop` idempotent.
- `wire`: `WireErrorPb` round trip of every `WireError` field; `AbortOnDrop`.
- `runtime` (self dev-dependency enables the feature): the `components`
  unit starts, reports ready and stops through `RuntimeHandle::shutdown`.

**`sekvent-macros`** (excluded from coverage):

- unit tests for name/package validation, humantime parsing and nanosecond
  emission, UpperCamelCase RPC names, the lifetime/`impl` token scan,
  `paths::runtime_path` (the existing EnvConfig cases plus the component
  cases);
- insta snapshots of `prettyplease::unparse` of the expansion, in
  `src/component/snapshots/`: `component__standard` (3.4 input),
  `component__local_only`, `component__remote_only`,
  `component_error__inventory` (3.7 input), `component_error__no_domain`;
- trybuild (`tests/ui.rs`): `tests/ui/pass/*.rs` — standard component
  called under both bindings, `local_only`, `remote_only` (build fails with
  `RemoteOnlyUnbound`), derive round trip with an `Option` field and a unit
  variant, `crate = "…"` override; `tests/ui/fail/*.rs` — one file per row
  of 3.2 and 3.7 (e.g. `c3_bad_name.rs`, `m2_async_call.rs`,
  `d11_unknown_code.rs`) plus rustc-level failures: non-prost request,
  error type without `ComponentError`, field type without `FromStr`.

insta `.snap` and trybuild `.stderr` files are written into the source tree,
so they are generated on the Mac (`INSTA_UPDATE=always cargo test -p
sekvent-macros --lib` and `TRYBUILD=overwrite cargo test -p sekvent-macros
--test ui`), reviewed, and then verified on rtx like everything else (see
decision 8). Pass cases compile and run.

**Elsewhere:** the `sekvent-config` prefix test (2.6); `sekvent-facade-check`
tests (section 4); the example suite (section 5, excluded from coverage).
Verification after merge: `rrb run gate`, then `rrb run coverage`.

## 7. Work split

Phase 1: three agents write code and tests and **do not build, lint or run
tests**. Phase 2 (parent): update `Cargo.lock` on the Mac with
`cargo metadata --format-version 1 > /dev/null` (resolves, does not compile),
generate snapshots and `.stderr` files (section 6), then `rrb run gate` once
over the merged tree. Phase 3: fixes dispatched by file owner.

| Agent | Writes (disjoint) | May assume |
|---|---|---|
| **A — framework** | `crates/sekvent-component/**` (new); `crates/sekvent/Cargo.toml`, `crates/sekvent/src/lib.rs`; `crates/sekvent-facade-check/**`; `crates/sekvent-config/src/lib.rs` (2.6 change only); **all of root `Cargo.toml`** | sekvent-macros exports `component` (attribute) and `ComponentError` (derive, helpers `reason`, `other`, `component_error`) whose output is exactly section 3; facade-check is the only place A's code uses the macros |
| **B — macros** | `crates/sekvent-macros/**` only: `Cargo.toml`, `src/lib.rs`, `src/paths.rs`, `src/env_config.rs` (switch to `paths.rs`, behaviour and tests kept), `src/component/{mod,parse,expand,tests}.rs` + `snapshots/`, `src/component_error.rs`, `tests/ui.rs`, `tests/ui/**` | the `sekvent_component` names and signatures of 2.3 and 2.4 exist exactly as written |
| **C — examples and docs** | `examples/**`; `README.md` (replace "Component model (planned)" with a short C1 example and link, add the crate-table row); `AGENTS.md` (crate map row for `sekvent-component`, `sekvent-macros` row becomes "proc macros (`EnvConfig`, `component`, `ComponentError`)"); `docs/component-model.md` (status: C1 implemented, link to this spec; keep the rest); `skills/sekvent/SKILL.md` (component section: declare, implement, install, bindings, keys, tests under both profiles) | sections 2–5 exactly; A adds the members and the facade feature |

Root `Cargo.toml` edits (agent A): the `members` list of 5.1;
`sekvent-component = { path = "crates/sekvent-component" }` after
`sekvent-proto-build` in the internal block; `prettyplease = "0.3.0"` in the
proc-macro block. Nothing else; `syn` already has `full` and
`extra-traits`, and the macro needs no `visit` (lifetimes and `impl` are
found by a token scan).

`crates/sekvent-macros/Cargo.toml` (agent B): add
`humantime = { workspace = true }`; dev-dependencies `trybuild`, `insta`,
`prettyplease`, `prost`, `tokio` (all `{ workspace = true }`) and
`sekvent-component = { path = "../sekvent-component" }` (a dev-dependency
cycle, which Cargo allows; trybuild cases need the runtime crate).

Every agent reports, besides its summary, **anything in this document that
contradicted the tree or could not be implemented as written**, with the
file and the reason, and keeps to its write set even when a fix elsewhere
looks obvious.

## 8. Decisions the user might override, and risks

### Decisions the user might override

1. **Synchronous, infallible entry points.** `App::builder(&source)` cannot
   fail and `build()` is sync (factories are sync; C2 channels connect
   lazily). The model document showed `builder(..)?` and `.build().await?`.
2. **Lifecycle hooks live outside the contract:** a separate `Lifecycle`
   trait, opted into with `install_with_lifecycle`, rather than methods on
   the component trait or a mandatory `impl Lifecycle for X {}`.
3. **Component-level overrides are per-method defaults.**
   `SEKVENT_COMPONENT_<C>_BULKHEAD_MAX_CONCURRENT` gives every method its own
   bulkhead; there is no shared component-wide bulkhead, no component-level
   `timeout`/`bulkhead` attribute, no bulkhead queue and no named policies
   in C1 (all C2).
4. **Key grammar without a `METHOD_` infix**
   (`SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT`) with build-time collision
   detection, and `SEKVENT_COMPONENT_BINDING` as the one "profile" switch for
   monoliths; the named profiles `monolith-local` / `monolith-serialized`
   live in the example's test support, not in the framework.
5. **Identical results across local bindings:** the `local` binding also
   normalizes typed errors through `into_app_error` / `from_app_error`, and
   the callee always sees `caller = trusted("local")` with the idempotency key
   forwarded (`inject`, not `propagate`). Panics still differ: `local`
   propagates, `local-serialized` returns `INTERNAL`.
6. **Per-method error types** are allowed (each must implement
   `ComponentError`), and `AppError` itself is a `ComponentError`, so
   `local_only` components can start without an error enum.
7. **`remote_only` is accepted but unbuildable in C1**, so API crates can be
   written ahead of C2; the alternative is a compile error until C2.
8. **Expected-output files are generated on the Mac.** insta snapshots and
   trybuild `.stderr` files are in-tree codegen under the rtx rule, so
   `cargo test -p sekvent-macros` runs locally for that step only; the
   alternative is transcribing them from rtx failure logs.

### Risks

- **RPITIT rewrite.** Implementors write `async fn` against a trait method
  declared `-> impl Future + Send`; non-`Send` state held across an await
  in an implementation fails at the impl with rustc's wording, not ours.
- **`#[diagnostic::on_unimplemented]` through blanket impls** may report the
  leaf bound (`prost::Message`) instead of our message; only the `.stderr`
  files record what rustc actually prints.
- **Clock mixing.** `CallContext` stores std `Instant`s while tests pause the
  tokio clock; the pipeline must follow 2.5 (tokio-derived deadlines,
  `sekvent_resilience::remaining`), or deadline tests become flaky.
- **Dev-dependency cycle** between sekvent-macros and sekvent-component
  compiles sekvent-macros twice in test builds; harmless for a proc macro,
  but a surprise when reading build logs.
- **Build time.** Seven example crates, three `protoc` runs and trybuild
  cases join every gate run.
- **Generated code under the workspace lints.** Missing docs on user trait
  methods propagate to the handle; the blanket `#[allow(clippy::all,
  clippy::pedantic)]` on generated impls could hide a real problem in
  generated code, which the snapshots are the check for.
- **Cargo.lock** must be refreshed on the Mac after the new members and
  dependencies land; a remote build's lockfile update is discarded.
