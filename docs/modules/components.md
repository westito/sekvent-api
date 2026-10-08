# Components

A component is a unit of business logic behind a typed contract: a Rust
trait checked against a protobuf `service`. Other code calls it through a
generated handle, and configuration decides how each call travels. It can
stay in-process (`local`), cross a serialization boundary on a task of its
own (`local-serialized`), or go to another process over gRPC (`grpc`). The
code is the same under all three.

This page is the practical guide. For the ideas behind the model and its
roadmap, see [the component model](../component-model.md). The full design
notes are [component-c1.md](../design/component-c1.md) (local bindings and
lifecycle), [component-c2.md](../design/component-c2.md) (gRPC and
contracts) and [component-end-user.md](../design/component-end-user.md)
(serving components to end users). Most examples here come from
[`examples/shop`](../../examples/shop), a complete project with three
components: inventory, notifications and orders.

## Enable it

| Facade feature | What it adds |
|---|---|
| `component` | `#[sekvent::component]`, `#[derive(sekvent::ComponentError)]`, the `App` builder, the `local` and `local-serialized` bindings. Turns on `config`, `error` and `context`. |
| `component-grpc` | The `grpc` binding and serving components over gRPC (`App::grpc_routes`). Turns on `component`. |
| `runtime` (with `component`) | `App::register`, which runs the App as one unit of the [runtime](runtime.md). |
| `runtime-grpc-web` (with `component-grpc`) | gRPC-Web translation on the runtime server, so browsers can call served components ([end users](#how-to-serve-components-to-end-users-browsers-apps)). |

A component is split over two crates. The **`-api` crate** holds the
contract: the `.proto`, the generated messages, the trait, the error type
and the generated handle. Callers depend only on the `-api` crate. The
**implementation crate** holds the struct that implements the trait.

```toml
# inventory-api/Cargo.toml
[dependencies]
prost = "0.14"
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", default-features = false, features = ["component"] }

[build-dependencies]
sekvent-proto-build = { git = "https://github.com/westito/sekvent-api", branch = "master" }
```

```toml
# the binary that wires components and serves them
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", default-features = false, features = ["component", "component-grpc", "config", "runtime"] }
tokio = { version = "1", features = ["full"] }
```

The macros locate the facade through the dependency **key**. Keep the key
`sekvent-api`, or rename it with `package = "sekvent-api"`; the key
`sekvent_api` does not work. An `-api` crate needs neither `tonic` nor
`tonic-prost`: it holds messages and the contract only.

```rust
use sekvent::prelude::*;                          // App, ComponentError, Lifecycle, CallContext, AppError, ErrorCode, …
use sekvent::component::{AppBuilder, Binding, BuildError, Deps};
```

## Quick example

The contract (`inventory-api`) is a `.proto` service, its build script, and a
trait checked against the service:

```proto
// inventory-api/proto/shop/inventory/v1/inventory.proto
syntax = "proto3";
package shop.inventory.v1;

service Inventory {
  rpc Reserve(ReserveRequest) returns (ReserveReply);
  rpc Release(ReleaseRequest) returns (ReleaseReply);
  rpc Stock(StockRequest) returns (StockReply);
}

message ReserveRequest { string order_id = 1; string sku = 2; uint32 quantity = 3; }
message ReserveReply { string reservation_id = 1; uint32 remaining = 2; }
// … ReleaseRequest, ReleaseReply, StockRequest, StockReply
```

```rust
// inventory-api/build.rs
fn main() {
    sekvent_proto_build::ProtoBuild::new("proto")
        .messages_only()
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
```

```rust
// inventory-api/src/lib.rs
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
}

pub use proto::shop::inventory::v1::{
    ReleaseReply, ReleaseRequest, ReserveReply, ReserveRequest, StockReply, StockRequest,
};
use sekvent::prelude::*;

#[derive(Debug, ComponentError)]
#[component_error(domain = "shop.inventory.v1")]
pub enum InventoryError {
    #[reason("OUT_OF_STOCK", code = FailedPrecondition, message = "only {available} of {sku} left")]
    OutOfStock { sku: String, available: u32 },
    #[reason("UNKNOWN_SKU", code = NotFound)]
    UnknownSku { sku: String },
    #[reason("RESERVATION_NOT_FOUND", code = NotFound)]
    ReservationNotFound { reservation_id: String },
    #[other]
    Other(AppError),
}

#[sekvent::component(
    name = "inventory",
    package = "shop.inventory.v1",
    proto = "crate::proto::shop::inventory::v1"
)]
pub trait Inventory: Send + Sync + 'static {
    #[call(idempotent, timeout = "2s", bulkhead = 16)]
    async fn reserve(&self, cx: &CallContext, req: ReserveRequest)
    -> Result<ReserveReply, InventoryError>;

    #[call(timeout = "500ms")]
    async fn release(&self, cx: &CallContext, req: ReleaseRequest)
    -> Result<ReleaseReply, InventoryError>;

    #[call(idempotent, timeout = "500ms")]
    async fn stock(&self, cx: &CallContext, req: StockRequest)
    -> Result<StockReply, InventoryError>;
}
```

Wiring and calling it:

```rust
use inventory::InventoryService;
use inventory_api::{InventoryHandle, StockRequest};
use sekvent::config::EnvSource;
use sekvent::prelude::*;

let source = EnvSource;
let mut builder = App::builder(&source);
InventoryHandle::install(&mut builder, |_deps| {
    Ok(InventoryService::new(vec![("sku-apple".to_owned(), 10)]))
})?;
let app = builder.build()?;          // every configuration problem is reported here
app.start().await?;

let inventory = app.handle::<InventoryHandle>()?;
let reply = inventory
    .stock(&CallContext::new(), StockRequest { sku: "sku-apple".to_owned() })
    .await?;

app.stop(std::time::Duration::from_secs(5)).await?;
```

## Concepts

- **Contract.** The proto `service` and the trait describe the same thing,
  and the `#[component]` macro rejects any mismatch at compile time. The
  service name must equal `<package>.<Trait>`. Each method maps to exactly
  one unary RPC, named in `UpperCamelCase` (`place_order` maps to
  `PlaceOrder`), with the same request and reply types. `()` stands for
  `google.protobuf.Empty`.
- **Handle.** The macro generates `<Trait>Handle` (`InventoryHandle`). It is
  cheap to clone and has one async method per trait method, plus
  `binding()` and either `install` and `install_with_lifecycle` (standard
  and `local_only` components) or, for `remote_only` components, only
  `install_remote`.
- **App.** Components are installed into an `AppBuilder`, built
  fail-closed in one step, then started and stopped together. Dependencies
  are injected through constructors: a factory asks for the handles of
  components installed before it. There is no service locator.
- **Binding.** How calls travel. It is chosen by configuration at build
  time, never in code.

  | Binding | What happens |
  |---|---|
  | `local` (default) | A direct call on the implementation, with no encoding. |
  | `local-serialized` | The request is encoded with prost and the context goes through the header codec. The call runs on a task of its own, and the reply or error is encoded and decoded back. Dropping the caller's future aborts the task. |
  | `grpc` | A unary gRPC call over plaintext HTTP/2 to `SEKVENT_COMPONENT_<C>_ENDPOINT`, authenticated with a [link token](link.md). |

- **Mode.** A `#[component]` is *standard* (any binding) by default.
  `local_only` components take plain Rust types, have no contract and can
  only be bound `local`. `remote_only` components never run in this binary
  and must be bound `grpc`.
- **Gate.** Every installed component has a serving gate that admits calls
  only while it is `Serving`, sheds calls that are already cancelled or
  past their deadline, applies the method's bulkhead and timeout, and
  counts in-flight calls so that a stop can drain them.

## How to define a component's `-api` crate

1. Write the `.proto`. It holds the `service` next to its messages, in a
   versioned package (`shop.orders.v1`). Put it under the crate's `proto/`
   directory, at a path that matches the package
   (`proto/shop/orders/v1/orders.proto`).
2. Add `build.rs` with `ProtoBuild::new("proto").messages_only().compile()`.
   This generates prost messages (with `prost::Name`, which the contract
   check needs) and the contract constants the macro reads. It does not
   generate tonic stubs. It compiles in-process with protox, so no `protoc`
   is needed. See [proto-build](proto-build.md).
3. Include the generated module once, as `pub mod proto { include!(…) }`,
   and re-export the messages callers need.
4. Declare the error enum and the trait, and point `proto = "…"` at the
   generated module for the package: `crate::proto::shop::orders::v1`.

A message from another package works as a request or reply as long as
it is the generated type. The check compares types by identity, nested
messages included.

### `#[sekvent::component(...)]` arguments

| Argument | Required | Meaning |
|---|---|---|
| `name = "orders"` | yes | Component name, `[a-z][a-z0-9_]*`, no `__`, no trailing `_`, at most 48 characters. It appears in configuration keys (`SEKVENT_COMPONENT_ORDERS_*`) and error metadata. |
| `package = "shop.orders.v1"` | unless `local_only` | The proto package: dot-separated lowercase segments. |
| `proto = "crate::proto::shop::orders::v1"` | unless `local_only` (forbidden with it) | Path of the module generated for the package. It must contain `__sekvent_service_<Trait>` and `__sekvent_rpc_<Trait>__<Rpc>`. |
| `local_only` | no | Plain Rust requests and replies (any `Send + 'static` type), no contract, `local` binding only. |
| `remote_only` | no | Never runs here. The handle has `install_remote` instead of `install`. |
| `crate = "::path::to::component"` | no | Names the runtime path explicitly. Without it, the path comes from your `Cargo.toml` (a direct `sekvent-component` dependency, otherwise `sekvent::component`). |

The rules for the trait:

- It may not be generic or `unsafe`, and its only allowed supertraits are
  `Send`, `Sync` and `'static`.
- It needs at least one method and no associated items.
- Every method is
  `async fn name(&self, cx: &CallContext, req: Request) -> Result<Reply, Error>`.
- Method names are snake_case. `install`, `install_with_lifecycle`,
  `install_remote`, `binding` and `clone` are reserved because the handle
  uses them.
- No default bodies, generic methods, or `const`/`unsafe`/`extern`
  methods are allowed.
- Requests and replies are owned, with no lifetimes and no `impl Trait`.
  The error type implements `ComponentError`; `AppError` itself qualifies.
- Only doc comments and the kind attribute are allowed on a method.

### `#[call(...)]` options

Every method needs exactly one kind attribute, and `#[call]` is the only
kind available. `#[async_call]` and `#[deferred]` are reserved for milestone
C3 and are rejected for now.

| Option | Meaning |
|---|---|
| `idempotent` | The method may be retried. Only idempotent methods are retried automatically, and only under `grpc`. |
| `timeout = "2s"` | A humantime string literal, positive. It bounds the **whole call**, retries included, and never extends the caller's deadline. |
| `bulkhead = 16` | An integer literal from 1 to `u32::MAX`: the most concurrent executions where the component runs. Calls beyond it fail with `RESOURCE_EXHAUSTED` / `BULKHEAD_FULL`. |
| `anonymous` | End users may call the method without credentials when the component is served with `SERVE_AUTH=bearer` or `link,bearer`. It waives end-user authentication only, never link authentication. A compile error on a `local_only` component, which is never served. See [How to serve components to end users](#how-to-serve-components-to-end-users-browsers-apps). |

`timeout` and `bulkhead` are defaults. Configuration can override each of
them, as described in
[How to tune resilience per component and method](#how-to-tune-resilience-per-component-and-method).
`anonymous` cannot be changed by configuration.
Unknown or duplicated options are compile errors. The trybuild cases under
`crates/sekvent-macros/tests/ui/fail/` list every message.

## How to declare typed errors

`#[derive(ComponentError)]` turns an enum into an error that travels as an
`AppError` and comes back as the same variant, under every binding.

```rust
#[derive(Debug, ComponentError)]
#[component_error(domain = "shop.orders.v1")]
pub enum OrdersError {
    #[reason("INVALID_QUANTITY", code = InvalidArgument)]
    InvalidQuantity { quantity: u32 },
    #[reason("OUT_OF_STOCK", code = FailedPrecondition, message = "only {available} of {sku} left")]
    OutOfStock { sku: String, available: u32 },
    #[reason("UNKNOWN_SKU", code = NotFound)]
    UnknownSku { sku: String },
    #[reason("CUSTOMER_BLOCKED", code = FailedPrecondition)]
    CustomerBlocked { customer_id: String },
    #[other]
    Other(AppError),
}
```

- `#[component_error(domain = "…")]` is optional. The domain is sent with
  the error, and a reason only decodes into its variant when the domain
  matches too. It must be non-empty and contain no whitespace.
  `crate = "…"` works as it does for `#[component]`.
- `#[reason("REASON", code = X, message = "…")]` marks a variant:
  - The reason is `UPPER_SNAKE_CASE`, at most 63 characters, and unique
    within the enum.
  - `code` is any `ErrorCode` variant except `Ok`, written `NotFound` or
    `ErrorCode::NotFound`.
  - `message` is a format string that may name the fields. Without it, the
    message is the reason in lower case (`out of stock`).
- A variant is either a unit variant or has named fields. Each field becomes
  error metadata: written with `ToString` and read back with `FromStr`. An
  `Option<T>` field is written only when it is `Some`. Tuple variants are
  rejected, except for `#[other]`.
- Exactly one `#[other]` variant holds a single `AppError`. Decoding never
  fails: an unknown reason, a different domain, or a field that does not
  parse all decode as `Other`. Framework errors (deadline, draining,
  bulkhead, transport) arrive there too.
- The derive also generates `From<AppError> for OrdersError` and
  `From<OrdersError> for AppError`.

Map errors from the components you call into your own variants before
returning them, as `orders` does with inventory's errors. Your contract
should not leak another component's error type:

```rust
let reservation = self.inventory.reserve(cx, reserve).await.map_err(|error| match error {
    InventoryError::OutOfStock { sku, available } => OrdersError::OutOfStock { sku, available },
    InventoryError::UnknownSku { sku } => OrdersError::UnknownSku { sku },
    error @ (InventoryError::ReservationNotFound { .. } | InventoryError::Other(_)) => {
        OrdersError::Other(error.into())
    }
})?;
```

Error reasons are not part of `cargo sekvent contract check`. Treat them as
API: renaming one sends old callers to `#[other]`.

## How to implement a component

Implement the trait on a plain struct with `async fn`. The macro rewrites
trait methods to `fn … -> impl Future + Send`, so implementations still
write `async fn`. Their futures must be `Send`, so never hold a
`std::sync::MutexGuard` across an `.await`.

```rust
use inventory_api::{InventoryHandle, ReserveRequest};
use notifications_api::NotificationsHandle;
use orders_api::{Orders, OrdersError, PlaceOrderReply, PlaceOrderRequest};
use sekvent::prelude::*;

pub struct OrdersService {
    inventory: InventoryHandle,
    notifications: NotificationsHandle,
}

impl OrdersService {
    pub fn new(inventory: InventoryHandle, notifications: NotificationsHandle) -> Self {
        Self { inventory, notifications }
    }
}

impl Orders for OrdersService {
    async fn place_order(&self, cx: &CallContext, req: PlaceOrderRequest)
        -> Result<PlaceOrderReply, OrdersError>
    {
        // Pass `cx` on: the deadline, cancellation, request id, trace and
        // hop count follow the call chain.
        let reserve = ReserveRequest {
            order_id: "ord-1".to_owned(),
            sku: req.sku.clone(),
            quantity: req.quantity,
        };
        let reserved = self.inventory.reserve(cx, reserve).await;
        // … map errors, notify, store the order
    }
    // …
}
```

Inside a handler, `cx` is the **callee's** context:

- `cx.caller()` is `ServiceIdentity::trusted("local")` for in-process
  calls, or the authenticated link for gRPC calls (`None` when served with
  `SERVE_AUTH=none`, to an end user, or to an anonymous caller).
- `cx.end_user()` is `Some(&EndUser)` when an end user called this
  component directly and the App's end-user authenticator accepted them
  (`SERVE_AUTH=bearer` or `link,bearer`). `caller()` is then `None`: the
  direct caller is either a service or an end user, never both. It is
  `None` for service calls, in-process calls and anonymous calls.
- `cx.tenant()` and the subject come from the caller. Over gRPC they
  survive only from a link listed in `SEKVENT_LINK_TRUSTED`, or from the
  authenticated end user. Roles stay with the end user: the components
  this handler calls see subject and tenant, but no `end_user()`.
- The deadline is the earlier of the caller's deadline and this method's
  timeout.
- An idempotency key the caller set for this call arrives here. A key the
  caller only *received* from upstream is not forwarded.

To set a key for a call you make, use
`cx.clone().with_idempotency_key("…")` (or `CallContext::with_idempotency_key`)
before calling the handle.

## How to wire components with the App builder

```rust
use sekvent::component::{AppBuilder, BuildError};

pub fn install(app: &mut AppBuilder<'_>, options: ShopOptions) -> Result<(), BuildError> {
    InventoryHandle::install(app, move |_deps| Ok(InventoryService::new(options.stock)))?;
    NotificationsHandle::install_with_lifecycle(app, move |_deps| {
        Ok(NotificationsService::new(options.blocked_customers))
    })?;
    OrdersHandle::install(app, |deps| {
        Ok(OrdersService::new(
            deps.handle::<InventoryHandle>()?,
            deps.handle::<NotificationsHandle>()?,
        ))
    })
}
```

| API | Signature | Notes |
|---|---|---|
| `App::builder` | `fn builder(source: &dyn ConfigSource) -> AppBuilder<'_>` | Every `SEKVENT_COMPONENT_*`, `SEKVENT_POLICY_*` and `SEKVENT_LINK_*` key is read from `source`. |
| `<C>Handle::install` | `fn install<I, F>(app: &mut AppBuilder<'_>, factory: F) -> Result<(), BuildError>` where `F: FnOnce(&mut Deps<'_>) -> Result<I, AppError> + Send + 'static` | Installing a component twice (by handle type or by name) fails at once with `BuildError::DuplicateInstall`. |
| `<C>Handle::install_with_lifecycle` | as `install`, with `I: Trait + Lifecycle` | Also runs the `Lifecycle` hooks. |
| `<C>Handle::install_remote` | `fn install_remote(app: &mut AppBuilder<'_>) -> Result<(), BuildError>` | `remote_only` components only. |
| `AppBuilder::provide` | `fn provide<T: Clone + Send + Sync + 'static>(&mut self, value: T) -> Result<(), BuildError>` | Shared resources (pools, clients, a `Clock`). A second value of the same type is `DuplicateResource`. |
| `AppBuilder::end_user_authenticator` | `fn end_user_authenticator(&mut self, authenticator: impl EndUserAuthenticator) -> Result<(), BuildError>` | The App's one end-user authenticator, for components served with `SERVE_AUTH=bearer` or `link,bearer`. A second one is `DuplicateEndUserAuthenticator`. Available without `component-grpc`. See [How to serve components to end users](#how-to-serve-components-to-end-users-browsers-apps). |
| `AppBuilder::build` | `fn build(self) -> Result<App, BuildError>` | Validates all configuration, then runs the factories in install order. |
| `Deps::handle` | `fn handle<H: ComponentHandle>(&self) -> Result<H, AppError>` | Only components installed **before** this one. Otherwise `FAILED_PRECONDITION`, naming both components. |
| `Deps::resource` | `fn resource<T: Clone + Send + Sync + 'static>(&self) -> Result<T, AppError>` | `FAILED_PRECONDITION`, naming the type, when nothing was provided. |
| `Deps::config` | `fn config(&self) -> &dyn ConfigSource` | The source the App is built from. |
| `Deps::component`, `Deps::binding` | `-> &'static str`, `-> Binding` | The component being built and its resolved binding. |

The build runs in two phases:

1. **Configuration is validated first, all at once.** The checks are:
   unknown keys under `SEKVENT_COMPONENT_` and `SEKVENT_POLICY_` (with
   suggestions), malformed values, invalid policies, key collisions between
   components or methods, bindings that are unavailable or do not fit the
   mode, a missing endpoint or link token, a component served with
   end-user authentication while no end-user authenticator is registered,
   and two exposed components with one gRPC service name. All problems
   come back together, as one
   `BuildError` or `BuildError::Multiple`. Messages name keys and
   components, never values. No factory runs unless the configuration is
   clean.
2. **Factories run in install order.** The first factory error stops the
   build (`BuildError::Factory { component, source }`). A component bound
   `grpc` keeps its `install` call, but **its factory does not run**: it
   builds nothing and needs none of its dependencies or resources.

Pass a database pool or an HTTP client as a resource:

```rust
builder.provide(pool.clone())?;                  // e.g. a sqlx::PgPool from sekvent::db
LedgerHandle::install(&mut builder, |deps| Ok(LedgerService::new(deps.resource::<PgPool>()?)))?;
```

Read the settings of a component's implementation inside its factory,
through `deps.config()`. The factory runs only when the component is bound
`local` or `local-serialized`, so a binary that reaches the component over
`grpc` does not need its keys. Reading them before `install` would make
every binary require them, and switching the component to a remote binding
would then fail on keys the binary never uses. A missing key fails the build
with `BuildError::Factory`; carrying the `ConfigError`'s text in the message,
as below, names the component and the key (never the value):

```rust
LedgerHandle::install(&mut builder, |deps| {
    // your #[derive(EnvConfig)] struct
    let settings = LedgerSettings::from_config(deps.config())
        .map_err(|error| AppError::failed_precondition(error.to_string()))?;
    Ok(LedgerService::new(settings, deps.resource::<PgPool>()?))
})?;
```

Settings that the binary needs whatever the binding (its own listener, say)
are read before building the App as usual; `BuildError` converts from
`ConfigError` with `?`.

`App` is cheap to clone. Other methods:

| Method | Returns |
|---|---|
| `app.handle::<H>()` | A handle for code outside the components (HTTP handlers, jobs, tests). `FAILED_PRECONDITION` when the component is not installed. |
| `app.binding("inventory")` | `Option<Binding>` |
| `app.state("inventory")` | `Option<ComponentState>`: `NotStarted`, `Serving`, `Draining` or `Stopped` |
| `app.components()` | Component names, in install order |
| `app.grpc_services()` | Full service names of the exposed components (empty without `component-grpc`) |
| `app.grpc_routes()` | `tonic::service::Routes` for the exposed components (`component-grpc`) |

## How to run lifecycle hooks

Implement `Lifecycle` and install with `install_with_lifecycle`. Both hooks
default to `Ok(())`.

```rust
impl Lifecycle for NotificationsService {
    async fn on_start(&self) -> Result<(), AppError> {
        self.open.store(true, Ordering::Release);
        Ok(())
    }

    async fn on_stop(&self) -> Result<(), AppError> {
        self.open.store(false, Ordering::Release);
        Ok(())
    }
}
```

- `app.start()` runs `on_start` in install order and opens each component
  for calls once its hook succeeds. Calls before that fail `UNAVAILABLE` /
  `COMPONENT_NOT_STARTED`.
  - If a hook fails, the components already started stop in reverse order,
    and the error is returned with `component` metadata.
  - Starting twice, or after a stop, is `FAILED_PRECONDITION`.
- `app.stop(grace)` stops components in reverse install order. Each one
  rejects new calls (`UNAVAILABLE` / `COMPONENT_DRAINING`), waits for its
  in-flight calls until `grace` has passed (in total, not per component),
  then runs `on_stop`.
  - Every started component's `on_stop` runs exactly once, even when
    another hook fails. The first hook error is returned.
  - The stop runs on its own task, so dropping the future does not
    interrupt it. Calling `stop` again waits for it to finish and returns
    `Ok`.
- A `stop` during `start` wins. The `on_start` still running is cancelled
  and that component counts as never started; `start` fails
  `FAILED_PRECONDITION`. Dropping the `start` future has the same effect.
- After the stop, calls fail `UNAVAILABLE` / `COMPONENT_STOPPED`.

### Under the runtime

With the `runtime` feature, `app.register(runtime_builder)` adds one
critical unit, `components`, in `Stage::Components`. The unit starts the
App, reports ready, and stops it when its stage drains. It drains for half
of the time left before the runtime's stop deadline, so the `on_stop` hooks
keep the other half. Ingress units stop first, so a listener stops taking
calls before the components drain.

```rust
let app = builder.build()?;
let server = Server::builder()
    .grpc_routes(app.grpc_routes())
    .bind(addr)
    .await?;
app.register(Runtime::builder())
    .unit("grpc", Stage::Ingress, UnitPolicy::default(), server.into_unit())
    .build()?
    .run()
    .await?;
```

This is `shop::bind` and `shop::runtime` from the example. See
[runtime](runtime.md) and [server](server.md).

## How to choose bindings

Nothing in code chooses a binding. The App builder reads it:

```sh
cargo run -p shop                                              # every component local
SEKVENT_COMPONENT_BINDING=local-serialized cargo run -p shop   # every component serialized
SEKVENT_COMPONENT_INVENTORY_BINDING=grpc \
  SEKVENT_COMPONENT_INVENTORY_ENDPOINT=http://127.0.0.1:50051 \
  SEKVENT_LINK_OUTBOUND_INVENTORY=$TOKEN cargo run -p shop     # inventory in another process
```

`SEKVENT_COMPONENT_BINDING` is the default for standard components.
`SEKVENT_COMPONENT_<C>_BINDING` overrides it for one component. A
`local_only` component ignores the default and is always `local`. A
`remote_only` component must set its own key to `grpc`. Values are exactly
`local`, `local-serialized` or `grpc` (`Binding::as_str`). Without the
`component-grpc` feature, `grpc` parses but fails the build with
`BindingUnavailable`.

What stays the same under every binding:

- **Results.** A typed error arrives as the same variant, and framework
  errors carry the same code and reason.
- **Deadlines and cancellation.** The callee sees the caller's deadline,
  narrowed by the method timeout. Dropping the caller's future cancels
  the call: under `local-serialized` it aborts the task, and under `grpc`
  it resets the HTTP/2 stream so the server cancels the handler's
  context.
- **Hop counting.** Each component call adds one hop. A call deeper than
  `SEKVENT_COMPONENT_MAX_HOPS` (default 16) fails `FAILED_PRECONDITION` /
  `CALL_DEPTH_EXCEEDED` before anything is sent, which stops accidental
  call cycles.
- **The configuration key set.** Every key is accepted under every
  binding, so one environment validates the same way in every topology.

Use `local-serialized` in CI. It exercises the codec, the header codec,
error mapping and cancellation without a network, so a type that does not
survive the wire fails a test long before you split a service out.

## How to serve components over gRPC

A process exposes a **locally bound** component with
`SEKVENT_COMPONENT_<C>_SERVE=grpc`, and must mount the App's routes on its
server:

```rust
let server = Server::builder()
    .grpc_routes(app.grpc_routes())   // mounts every exposed component
    .bind(addr)
    .await?;
```

- The paths are `/<package>.<Trait>/<Rpc>`, for example
  `/shop.inventory.v1.Inventory/Reserve`. The wire is plain unary gRPC, so a
  client generated from the `.proto` by any toolchain can call it.
- `app.grpc_routes()` returns empty routes when nothing is exposed, so it
  is safe to always mount. If a component is exposed but the routes were
  never taken, `app.start()` fails `FAILED_PRECONDITION` /
  `GRPC_NOT_MOUNTED`.
- The order on the serving side is fixed, and everything up to the gate is
  decided from the request **headers** before the body is read. The link
  token is authenticated first; under `SERVE_AUTH=link` an unauthenticated
  call gets `UNAUTHENTICATED` even for a method that does not exist. Then
  exact routing: a path that is not `/<service>/<Rpc>` of a declared method
  is `UNIMPLEMENTED` / `UNKNOWN_METHOD`. Then the context is decoded from
  the headers (trailers are never trusted) and the deadline narrowed by the
  method's timeout. Under the bearer modes the end user is authenticated
  next ([below](#serving-order-for-end-users)). Then the hop limit is
  checked and the call passes the component's gate and bulkhead.
- Requests are capped at 4 MiB. A handler panic answers `INTERNAL` /
  `HANDLER_PANICKED`, and the payload is never shown. A request body that
  does not decode is `INVALID_ARGUMENT` / `MALFORMED_REQUEST`.
- Health: under `App::register`, every exposed service reports
  `NotServing` in `grpc.health.v1` until the App has started, `Serving`
  while it runs, and `NotServing` from the moment shutdown begins.
- Two exposed components with the same full service name fail the build
  and name both `SERVE` keys.

A component bound `grpc` cannot be exposed (`NotServable`, "it is not bound
locally"), and neither can a `local_only` one.

`inventory-svc` in the example is a complete service: `install`, `bind`,
`runtime` and a thin `main.rs`.

## How to authenticate calls between services (link tokens)

Both directions fail closed. Link tokens and their keys are covered fully
in [link](link.md); the component side works like this:

| Side | Keys | Behaviour |
|---|---|---|
| Caller (`grpc` binding) | `SEKVENT_LINK_OUTBOUND_<LINK>`, where the link is `SEKVENT_COMPONENT_<C>_LINK` (default: the component name) | Every attempt sends `authorization: Bearer <token>`. A missing token is a build error naming the key. `SEKVENT_COMPONENT_<C>_AUTH=none` turns this off explicitly and logs a warning. |
| Server (`SERVE=grpc`, `SERVE_AUTH=link`, the default) | `SEKVENT_LINK_INBOUND_<CALLER>`, at least one; `SEKVENT_LINK_TRUSTED` | A missing, malformed or unknown token gets `UNAUTHENTICATED` with one fixed message and no reason. The caller's identity is the link name. Subject and tenant are kept only for trusted links. `SEKVENT_COMPONENT_<C>_SERVE_AUTH=none` turns this off explicitly and logs a warning. |
| Server (`SERVE_AUTH=link,bearer`) | as above, plus a registered end-user authenticator | A matching link token makes the caller that link, exactly as above. Without a match, the App's end-user authenticator decides ([below](#how-to-serve-components-to-end-users-browsers-apps)). |
| Server (`SERVE_AUTH=bearer`) | a registered end-user authenticator; no `SEKVENT_LINK_*` key | Only end users: link tokens are not checked. |

A token is the same value on both sides. In the split shop, the service
knows it as the inbound link `shop` and the shop presents it as the
outbound link `inventory`. Generate one with
`openssl rand -base64 32 | tr '+/' '-_' | tr -d '='`.

The build also rejects:

- one token used for two purposes (`check_distinct_tokens`);
- a link named `local`, which is reserved for in-process callers;
- a malformed `LINK` value (letters, digits and underscores only).

A token that is valid under one binding is accepted under every binding.

## How to serve components to end users (browsers, apps)

A browser (over gRPC-Web) or a native app cannot hold a link token. To let
your own frontend call a component directly, serve it with end-user
authentication and register one **end-user authenticator** on the App. The
contract, the error mapping and the gate stay the ones every other caller
gets; no hand-written gRPC service is needed.

```sh
SEKVENT_COMPONENT_ORDERS_SERVE=grpc
SEKVENT_COMPONENT_ORDERS_SERVE_AUTH=bearer          # end users only
# SEKVENT_COMPONENT_ORDERS_SERVE_AUTH=link,bearer   # peer services with link tokens, and end users
```

| `SERVE_AUTH` | Who may call a method | Who may call an `anonymous` method |
|---|---|---|
| `link` (default) | a peer with an inbound link token | the same: a link token is required |
| `bearer` | an end user the authenticator accepts | anyone, with no identity |
| `link,bearer` (or `bearer,link`) | a peer with an inbound link token, else an end user the authenticator accepts | a peer with a valid link token keeps its identity; anyone else is served without one |
| `none` | anyone (logged once at `warn`) | anyone |

Spellings are exact; anything else is a malformed key. `SERVE_AUTH=bearer`
alone needs no `SEKVENT_LINK_*` key. A mode that includes `link` still
needs at least one `SEKVENT_LINK_INBOUND_<CALLER>`, as before.

### Register the end-user authenticator

The authenticator gets the request **head** only (`&http::request::Parts`:
method, URI, headers, extensions); the body has not been read yet. It
returns the `EndUser` or an `AppError`. Any sync closure is an
authenticator:

```rust
use sekvent::component::EndUser;

builder.end_user_authenticator(|request: &http::request::Parts| {
    let user = sessions.verify(&request.headers)?;      // your own check; Err(AppError::unauthenticated(..)) when it fails
    Ok(EndUser::new(user.id).with_tenant(user.tenant).with_roles(user.roles))
})?;
```

For JSON Web Tokens, `BearerAuth::end_user` is a ready-made check (facade
feature `auth-axum` or `auth-tonic`, see [auth](auth.md#authenticate-end-users-of-components)).
`sub` becomes the subject; tenant and roles come from your custom claims
through `EndUserClaims`:

```rust
use std::sync::Arc;
use sekvent::auth::{BearerAuth, EndUserClaims, Validation};
use sekvent::context::SystemClock;

#[derive(Debug, serde::Deserialize)]
struct Profile { tenant: String, roles: Vec<String> }

impl EndUserClaims for Profile {
    fn tenant(&self) -> Option<&str> { Some(&self.tenant) }
    fn roles(&self) -> &[String] { &self.roles }
}

let auth = BearerAuth::new(keys, Validation::new().with_audience("web"), Arc::new(SystemClock));
builder.end_user_authenticator(move |request: &http::request::Parts| {
    auth.end_user::<Profile>(&request.headers)
})?;
```

When the check needs I/O (a session store, say), implement
`EndUserAuthenticator` yourself:

```rust
use std::future::Future;
use std::pin::Pin;
use sekvent::component::{EndUser, EndUserAuthenticator};

struct SessionAuth { sessions: SessionStore }

impl EndUserAuthenticator for SessionAuth {
    fn authenticate<'a>(
        &'a self,
        request: &'a http::request::Parts,
    ) -> Pin<Box<dyn Future<Output = Result<EndUser, AppError>> + Send + 'a>> {
        Box::pin(async move {
            let token = request
                .headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or_else(|| AppError::unauthenticated("no session token"))?;
            let session = self.sessions.find(token).await?        // UNAVAILABLE when the store is down
                .ok_or_else(|| AppError::unauthenticated("unknown session"))?;
            Ok(EndUser::new(session.user_id).with_tenant(session.tenant).with_roles(session.roles))
        })
    }
}

builder.end_user_authenticator(SessionAuth { sessions })?;
```

- One authenticator per App. A second registration fails at once with
  `BuildError::DuplicateEndUserAuthenticator`. If components need
  different rules, look at `request.uri.path()` in the one authenticator.
- Fail closed: a component exposed with `SERVE=grpc` whose `SERVE_AUTH`
  includes `bearer` fails the build with
  `BuildError::EndUserAuthenticatorMissing`, naming the component and its
  `SERVE_AUTH` key, when none is registered. An authenticator that no
  component uses is not an error.
- `end_user_authenticator` and `EndUser` exist without the
  `component-grpc` feature, so application code compiles the same way
  under every feature set.
- The authenticator runs bounded by the call's deadline; when it passes,
  the caller gets `DEADLINE_EXCEEDED`. The framework never logs anything
  about the token.

### Read the end user in a handler

`cx.end_user()` returns the `EndUser` the authenticator accepted. Check
roles with `sekvent::auth::require_any_role`, which accepts an `EndUser`
(`HasRoles`):

```rust
async fn get_order(&self, cx: &CallContext, req: GetOrderRequest) -> Result<Order, OrdersError> {
    if let Some(user) = cx.end_user() {
        // A browser: only staff may read any order by id.
        sekvent::auth::require_any_role(user, &["admin"])?;   // PERMISSION_DENIED otherwise
    }
    // Otherwise a peer service (cx.caller()), trusted by its link.
    // …
}
```

`EndUser` has `subject()`, `tenant()`, `roles()` and `has_role(..)`. The
context's subject and tenant are set from it, so `cx.tenant()` works the
same for an end user as behind a trusted link. Subject and tenant headers
the client sent itself are dropped before the authenticator runs, so an end
user cannot assert another identity. The components this handler calls see
subject and tenant only; the end user and its roles stay here.

### Public methods: `#[call(anonymous)]`

A few RPCs must work without credentials, such as signing in:

```rust
#[sekvent::component(name = "accounts", package = "shop.accounts.v1", proto = "crate::proto::shop::accounts::v1")]
pub trait Accounts: Send + Sync + 'static {
    /// Exchange a password for a session token.
    #[call(anonymous, timeout = "2s")]
    async fn sign_in(&self, cx: &CallContext, req: SignInRequest) -> Result<SignInReply, AccountsError>;

    #[call(idempotent, timeout = "500ms")]
    async fn profile(&self, cx: &CallContext, req: ProfileRequest) -> Result<ProfileReply, AccountsError>;
}
```

- `anonymous` waives **end-user authentication only**. It never waives link
  authentication: under `SERVE_AUTH=link` an anonymous method still needs a
  link token, so a component that must stay closed cannot be opened by the
  attribute alone; the operator has to choose a bearer mode too.
- Under the local bindings it has no effect. On a `local_only` component it
  is a compile error (`` `anonymous` has no effect on a local_only
  component, which is never served; remove it``).
- An anonymous method never sees an end user. A token sent to it is
  ignored, not verified, so `cx.end_user()` is `None` there. A method that
  behaves differently for signed-in users is not anonymous.
- It combines with the other options (`#[call(anonymous, timeout = "2s")]`)
  and sets `MethodDescriptor::with_anonymous()` (read back with
  `is_anonymous()`).

### What a rejected end user sees

| The authenticator returns | The caller gets |
|---|---|
| `Ok(user)` | The call runs with `cx.end_user() == Some(&user)`. |
| `Err(e)` with code `UNAUTHENTICATED` | `UNAUTHENTICATED` with the message `invalid or expired credentials` (`END_USER_REJECTED_MESSAGE`), no reason and no metadata. The answer is the same for a missing, malformed, expired or unknown token. |
| `Err(e)` with any other code | `e` unchanged, for example `UNAVAILABLE` when the session store is down, so a client retries instead of signing the user out. |
| nothing before the call's deadline | `DEADLINE_EXCEEDED` |

In every rejected case the request body is never read and the handler
never runs.

### Serving order for end users

Everything is still decided from the request head before the body is read:

1. **Link.** When the mode includes `link`, the bearer token is compared
   with the inbound link tokens. A match makes the caller that link, and
   the end-user authenticator is not consulted.
2. **Route.** The path must be exactly `/<service>/<Rpc>`; anything else is
   `UNIMPLEMENTED` / `UNKNOWN_METHOD`. Under the bearer modes routing comes
   before the end-user authenticator, so an unknown path never costs a
   token verification.
3. **Context.** It is decoded from the headers and the deadline narrowed by
   the method's timeout.
4. **End user.** When the mode includes `bearer`, no link matched and the
   method is not anonymous, the authenticator runs. On success the context
   carries the end user.
5. **Hop limit**, gate, bulkhead, method timeout and handler, exactly as
   for a link caller.

### Mount the routes for browsers

Mount `app.grpc_routes()` on the [runtime server](server.md) as usual. With
the facade feature `runtime-grpc-web` (off in the facade's defaults), the
server translates gRPC-Web, so the same routes answer browsers over
HTTP/1.1:

```rust
use sekvent::runtime::{Cors, Server};

let app = builder.build()?;
let server = Server::builder()
    .grpc_routes(app.grpc_routes())
    .rest(rest_routes())
    .prefix("/api")
    .cors(Cors::origins(["https://shop.example.com"])?)
    .bind(addr)
    .await?;
```

- The browser calls `POST /api/shop.orders.v1.Orders/PlaceOrder` with
  `content-type: application/grpc-web+proto`, `x-grpc-web: 1` and
  `authorization: Bearer <token>`. Native gRPC clients keep using
  `/shop.orders.v1.Orders/PlaceOrder` at the root, unless
  `grpc_at_root(false)`.
- CORS preflights are answered by the server before any authentication, so
  they never reach a component. The default allowed and exposed headers
  already include `authorization`, `x-grpc-web`, `grpc-timeout`,
  `grpc-status`, `grpc-message` and `grpc-status-details-bin`.
- `ServerBuilder::authenticator` does not decide component calls. It never
  rejects a request, and the component service never reads it: component
  calls are authenticated by `SERVE_AUTH` alone. The end-user authenticator
  receives the request head with its extensions, so it may read the
  `CallContext` the server stored there if you want to reuse that decision.
- Global layers (`ServerBuilder::layer`) wrap component calls too; see
  [Pitfalls and security](#pitfalls-and-security).

## How to tune resilience per component and method

Policies come from [`PolicySpec`](resilience.md) fields, layered from the
lowest precedence to the highest:

1. the framework default: three attempts, breaker on;
2. the `#[call]` attribute (`timeout`, `bulkhead`);
3. a named policy: the method's own `…_<C>_<M>_POLICY`, otherwise the
   component's `…_<C>_POLICY`;
4. component keys `SEKVENT_COMPONENT_<C>_<FIELD>`;
5. method keys `SEKVENT_COMPONENT_<C>_<M>_<FIELD>`.

| Field | Levels | Used by |
|---|---|---|
| `TIMEOUT` | method, component, named | The caller under every binding (the whole call, retries included), and the serving side for gRPC-served calls. |
| `BULKHEAD_MAX_CONCURRENT`, `BULKHEAD_MAX_QUEUE`, `BULKHEAD_QUEUE_TIMEOUT` | method, component, named | The serving side, where the component runs. A `grpc` caller has no bulkhead. |
| `RETRY_MAX_ATTEMPTS`, `RETRY_INITIAL_BACKOFF`, `RETRY_MAX_BACKOFF`, `RETRY_MULTIPLIER`, `RETRY_JITTER`, `RETRY_MAX_RETRY_AFTER` | method, component, named | The `grpc` caller, for `idempotent` methods only, within the deadline. |
| `RETRY_BUDGET_RATIO` (default 0.2), `RETRY_BUDGET_MIN_PER_SEC` (default 10) | component, named | One budget per remote component, shared by its methods. |
| `BREAKER_ENABLED`, `BREAKER_FAILURE_RATE`, `BREAKER_WINDOW`, `BREAKER_MIN_CALLS`, `BREAKER_WAIT_IN_OPEN`, `BREAKER_PERMITTED_IN_HALF_OPEN` | component, named | One breaker per remote component. |

The value formats and the meaning of each field are documented in
[resilience](resilience.md). Additional rules:

- `RATE_LIMIT_*` keys are not accepted for components; they fail as
  unknown keys.
- A component-only field (budget, breaker) at method level is unknown.
  Referencing a named policy that sets one from a method's `_POLICY` key
  is rejected.
- Named policies use `SEKVENT_POLICY_<NAME>_<FIELD>`. The name is letters,
  digits and underscores and is upper-cased. Referencing a policy that has
  no key, or setting `SEKVENT_POLICY_*` keys for a policy nobody
  references, fails the build.
- Combinations that do not build fail as `…_<C>_<M>_*` or `…_<C>_*`, with
  the parameter and the reason. One example is
  `BULKHEAD_QUEUE_TIMEOUT=0s` with `BULKHEAD_MAX_QUEUE` above zero.

```sh
SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT=250ms
SEKVENT_COMPONENT_INVENTORY_RESERVE_BULKHEAD_MAX_CONCURRENT=4
SEKVENT_COMPONENT_INVENTORY_POLICY=remote
SEKVENT_POLICY_REMOTE_RETRY_MAX_ATTEMPTS=2
SEKVENT_POLICY_REMOTE_BREAKER_FAILURE_RATE=0.5
```

Under `grpc`:

- **Retries.** Retried codes are the transient ones (`UNAVAILABLE`,
  `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`, `ABORTED`), only for
  `idempotent` methods, with backoff and the budget, within the method
  timeout and the caller's deadline.
- **Breaker.** The breaker counts `UNAVAILABLE`, `DEADLINE_EXCEEDED` and
  `RESOURCE_EXHAUSTED` as failures of the callee. Business errors never
  open it. When open, calls fail `UNAVAILABLE` / `CIRCUIT_OPEN` without
  being sent.
- **Method timeout.** If the method's `TIMEOUT` fires while the caller
  still has time, the call fails `DEADLINE_EXCEEDED` / `METHOD_TIMEOUT`
  (with `timeout_ms` metadata) and counts against the breaker. The
  caller's own deadline or cancellation never counts.
- **Downstream failures.** A transient failure from further down a call
  chain becomes `INTERNAL` / `DOWNSTREAM_FAILURE` at the serving boundary.
  The `downstream` metadata names the component where it started, and
  `downstream_code` / `downstream_reason` keep the original values. Callers
  above therefore neither retry nor blame the healthy component in between.

The `grpc` caller connects lazily, with one channel per endpoint shared
by every component on it. The connect timeout is 5 s, with HTTP/2
keep-alive every 30 s.

## Configuration keys

All keys are reserved `SEKVENT_*` keys. `<C>` is the upper-cased component
name and `<M>` the upper-cased method name. All are optional unless a rule
requires them.

| Key | Values | Default |
|---|---|---|
| `SEKVENT_COMPONENT_BINDING` | `local`, `local-serialized`, `grpc` | `local` |
| `SEKVENT_COMPONENT_MAX_HOPS` | 1 to 1000 | 16 |
| `SEKVENT_COMPONENT_<C>_BINDING` | as above | the default binding |
| `SEKVENT_COMPONENT_<C>_ENDPOINT` | `http://host:port`, optionally with one trailing `/`. `https://` and user information are rejected. | required under `grpc` |
| `SEKVENT_COMPONENT_<C>_LINK` | link name `[A-Za-z0-9_]+`, not `local` | the component name |
| `SEKVENT_COMPONENT_<C>_AUTH` | `link` or `none` | `link` |
| `SEKVENT_COMPONENT_<C>_SERVE` | `grpc` or `none` | `none` |
| `SEKVENT_COMPONENT_<C>_SERVE_AUTH` | `link`, `bearer`, `link,bearer` (or `bearer,link`), `none`. A mode with `bearer` needs `AppBuilder::end_user_authenticator`. | `link` |
| `SEKVENT_COMPONENT_<C>_POLICY`, `SEKVENT_COMPONENT_<C>_<M>_POLICY` | a policy name | — |
| `SEKVENT_COMPONENT_<C>_<FIELD>`, `SEKVENT_COMPONENT_<C>_<M>_<FIELD>` | resilience fields, as in the previous section | attribute and framework defaults |
| `SEKVENT_POLICY_<NAME>_<FIELD>` | named policy fields | — |
| `SEKVENT_LINK_OUTBOUND_<LINK>`, `SEKVENT_LINK_INBOUND_<CALLER>`, `SEKVENT_LINK_TRUSTED` | see [link](link.md) | — |

The constants `CONFIG_PREFIX`, `DEFAULT_BINDING_KEY`, `MAX_HOPS_KEY`,
`DEFAULT_MAX_HOPS`, `POLICY_PREFIX` and `LOCAL_CALLER` are exported from
`sekvent::component`. Two components or methods whose names produce the
same key fail the build with `KeyCollision`.

## Contracts: `cargo sekvent contract`

Baselines record each service's wire shape, so a breaking change cannot
land unnoticed.

```toml
# sekvent.toml
[contract]
roots = ["inventory-api/proto", "notifications-api/proto", "orders-api/proto"]
baseline = "contracts"   # default
# includes = ["third_party/proto"]   extra import directories
# gate = true                        run `contract check` in `cargo sekvent gate` (default when roots is set)
```

```sh
cargo sekvent contract emit                    # write every baseline (or name services: shop.orders.v1.Orders)
cargo sekvent contract check                   # exit 1 on a breaking change or a missing baseline
```

- Both commands compile the protos in-process with protox. They need no
  `protoc` and no cargo build.
- Each service gets `contracts/<full.service.Name>.json`, in baseline
  format 2. The file holds the service, its source file, every RPC (request,
  reply, streaming flags) and every message and enum reachable from it,
  with field numbers, types, cardinality, oneofs, reservations and
  extensions. Run `emit` on your development machine and commit the JSON.
- `check` reports breaking changes:
  - a removed service or RPC;
  - a changed request type, reply type or streaming flag;
  - a removed field or enum number that was not reserved;
  - a changed field type, cardinality or oneof membership;
  - a dropped reservation;
  - an enum turning open or closed.

  Additions and renames are compatible, because the wire carries numbers
  only.
- A breaking change belongs in a new package (`shop.orders.v2`), served
  as a second component alongside `v1` until every caller has moved.

The compile-time check in `#[component]` makes sure the trait matches the
current proto; `contract check` makes sure the current proto still matches
what deployed peers speak.

## Topologies

The shop runs the same component code in each topology:

| Topology | Shop process | `inventory-svc` process |
|---|---|---|
| Monolith | nothing set | — |
| Monolith, serialized (the CI profile) | `SEKVENT_COMPONENT_BINDING=local-serialized` | — |
| Split | `SEKVENT_COMPONENT_INVENTORY_BINDING=grpc`, `SEKVENT_COMPONENT_INVENTORY_ENDPOINT=http://127.0.0.1:50051`, `SEKVENT_LINK_OUTBOUND_INVENTORY=<token>` | `SEKVENT_COMPONENT_INVENTORY_SERVE=grpc`, `SEKVENT_LINK_INBOUND_SHOP=<token>`, `SEKVENT_LINK_TRUSTED=shop` |

In the split topology the shop still calls `shop::install`. Inventory's
factory simply does not run there.

## Error codes and reasons

Reason constants live in `sekvent::component::reasons`. Errors the
framework creates on the caller or serving side carry `component` and
`method` metadata. Errors returned by a remote server keep exactly the
metadata the server sent.

| Constant | Reason | Code | When |
|---|---|---|---|
| `NOT_STARTED` | `COMPONENT_NOT_STARTED` | `UNAVAILABLE` | Called before `start` |
| `DRAINING` | `COMPONENT_DRAINING` | `UNAVAILABLE` | Called while stopping |
| `STOPPED` | `COMPONENT_STOPPED` | `UNAVAILABLE` | Called after `stop` |
| `BULKHEAD_FULL` | `BULKHEAD_FULL` | `RESOURCE_EXHAUSTED` | The method's bulkhead is full |
| `CIRCUIT_OPEN` | `CIRCUIT_OPEN` | `UNAVAILABLE` | The remote component's breaker is open |
| `UNREACHABLE` | `COMPONENT_UNREACHABLE` | `UNAVAILABLE` | A transport failure (connection refused, reset) |
| `METHOD_TIMEOUT` | `METHOD_TIMEOUT` | `DEADLINE_EXCEEDED` | The method timeout fired while the caller had time |
| `CALL_DEPTH_EXCEEDED` | `CALL_DEPTH_EXCEEDED` | `FAILED_PRECONDITION` | More hops than `SEKVENT_COMPONENT_MAX_HOPS` |
| `MALFORMED_REQUEST` | `MALFORMED_REQUEST` | `INVALID_ARGUMENT` | The request bytes did not decode |
| `MALFORMED_REPLY` | `MALFORMED_REPLY` | `INTERNAL` | The reply or error bytes did not decode |
| `UNKNOWN_METHOD` | `UNKNOWN_METHOD` | `UNIMPLEMENTED` | The gRPC path names no method |
| `GRPC_NOT_MOUNTED` | `GRPC_NOT_MOUNTED` | `FAILED_PRECONDITION` | `start` with exposed components but no `grpc_routes()` |
| `DOWNSTREAM_FAILURE` | `DOWNSTREAM_FAILURE` | `INTERNAL` | A transient failure further down the chain |
| `HANDLER_PANICKED` | `HANDLER_PANICKED` | `INTERNAL` | A method panicked |

The caller's own deadline gives a plain `DEADLINE_EXCEEDED` with no reason.
Cancellation gives `CANCELLED`. A rejected link token gives
`UNAUTHENTICATED` with no reason. A rejected end user gets
`UNAUTHENTICATED` with `invalid or expired credentials` and no reason; other
end-user authenticator errors pass through unchanged
([details](#what-a-rejected-end-user-sees)).

The `BuildError` variants are `DuplicateInstall`, `DuplicateResource`,
`DuplicateEndUserAuthenticator`, `EndUserAuthenticatorMissing { component,
key }`, `KeyCollision`, `Config`, `BindingUnavailable`, `LocalOnly`,
`RemoteOnly`, `RemoteOnlyUnbound`, `NotServable`, `Factory` and `Multiple`
(flat, never nested). The enum is `#[non_exhaustive]`.

## Testing components

- **Unit-test the implementation directly.** It is a plain struct: call its
  trait methods with `CallContext::new()`, and its `Lifecycle` hooks too
  (see `notifications/src/lib.rs`).
- **Build Apps from a `MapSource`**, never from the process environment:

  ```rust
  let source = MapSource::new().with("SEKVENT_COMPONENT_BINDING", "local-serialized");
  let mut builder = App::builder(&source);
  shop::install(&mut builder, shop::ShopOptions::demo())?;
  let app = builder.build()?;
  app.start().await?;
  ```

- **Run the same test under several bindings.** The shop uses `rstest`
  cases over three profiles: `monolith-local`, `monolith-serialized`, and
  `split-grpc`, where inventory runs in a second App in the same test
  process. It is served by `inventory_svc::bind` on `127.0.0.1:0` under a
  `Runtime::builder().without_signals().shutdown_delay(Duration::ZERO)`,
  and the caller binds it `grpc` at `server.local_addr()`. See
  `examples/shop/shop/tests/support/mod.rs`.
- **Fakes.** Install a fake through the real handle,
  `InventoryHandle::install(app, move |_deps| Ok(fake))`, so it sits behind
  the same binding, gate and policies as the real component.
- **Assert bindings.** Use `app.binding("inventory")` and check component
  states with `app.state(...)`.
- **Clocks.**
  - Local bindings run on tokio's paused clock (`start_paused = true`),
    where exact time bounds are safe to assert.
  - Anything with a socket needs the real clock. Assert lower bounds only,
    wrap the test in a 30 s `tokio::time::timeout`, and make one call to
    warm the lazy gRPC channel before asserting on deadlines.
  - An address nobody listens on is `127.0.0.1:1`.
  - Never sleep to wait for something.
- **Make split-topology retries fast** with
  `SEKVENT_COMPONENT_<C>_RETRY_INITIAL_BACKOFF=1ms`, `…_MAX_BACKOFF=1ms`
  and `…_RETRY_JITTER=none`.
- **Compile-time contract errors** are covered by trybuild tests in
  `crates/sekvent-macros/tests/ui/`. They are a good reference for what
  the macros reject.

## Pitfalls and security

- **Install order is dependency order.** `deps.handle::<H>()` only sees
  components installed earlier, which also rules out cycles.
- **Handles are lazy about state.** A handle obtained during the build
  fails `COMPONENT_NOT_STARTED` until `app.start()` has opened its
  component. Do not call other components from a factory.
- **`grpc` skips the factory.** Code with side effects in a factory does
  not run in a process where the component is remote. Put startup work in
  `on_start`.
- **Retries need `idempotent`.** Without it nothing is retried, even under
  `grpc`. Mark a method idempotent only when repeating it is safe: either
  it is naturally idempotent (`reserve` returns the existing reservation
  for the same order), or it deduplicates on the idempotency key.
- **`AUTH=none` and `SERVE_AUTH=none` turn authentication off.** Each logs
  a warning at build time. Use them only on a trusted private network,
  and never expose `SERVE_AUTH=none` publicly. To open a component to
  browsers, use `SERVE_AUTH=bearer` with an end-user authenticator instead.
- **`anonymous` is for public RPCs only.** Under a bearer mode anyone on
  the network can call an anonymous method, and any token they send is
  ignored. Use it for sign-in and similar, never for a method that reads
  or changes a user's data.
- **Global layers see component calls.** A `ServerBuilder::layer` that
  rejects requests without its own credentials also rejects link callers
  and anonymous methods served on that listener. Put such a layer on the
  REST router instead, or let the gRPC paths through.
- **The server's authenticator is not component auth.**
  `ServerBuilder::authenticator` never rejects, and component calls ignore
  it; only `SERVE_AUTH` and the App's end-user authenticator decide.
- **The transport is plaintext HTTP/2.** `https://` endpoints are rejected
  until TLS support lands. Keep component traffic on a private network or
  behind a TLS-terminating proxy.
- **Error messages travel to callers.** Do not put secrets or personal data
  in `message` format strings or metadata fields. Internal sources
  (`with_source`) never leave the process.
- **`local_only` is an on-ramp.** To make the component remote-capable,
  remove `local_only`, add the proto `service`, and set `proto = "…"`.
- **Breaking proto changes** need a new package. Never edit a baseline by
  hand to make `check` pass.

## See also

- [The component model](../component-model.md) — concepts, roadmap, and what C3 and C5 add
- [proto-build](proto-build.md) — generating messages and contract constants
- [link](link.md) — service tokens
- [auth](auth.md) — `BearerAuth::end_user`, `EndUserClaims`, role checks
- [component-end-user.md](../design/component-end-user.md) — design note on serving components to end users
- [resilience](resilience.md) — policy fields and their semantics
- [runtime](runtime.md), [server](server.md) — running the App and serving gRPC
- [error](error.md), [context](context.md) — `AppError`, `ErrorCode`, `CallContext`
- [cli](../cli.md) — `cargo sekvent contract`, `gate`
- [`examples/shop`](../../examples/shop) — the complete example
