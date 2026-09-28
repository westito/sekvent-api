---
name: sekvent
description: Write or change code in a Rust backend built on the sekvent framework (a workspace with `sekvent.toml` and a `sekvent` dependency). Use when adding a config struct, an error, a gRPC or REST endpoint, a background worker, an outbound HTTP client with retries or a breaker, a database pool, a JWT login, service-to-service authentication, a container-backed test, protobuf codegen or a component (a trait that runs in-process now and can move to its own service later) — and before hand-rolling any of those, since sekvent already has them. Gives the crate map, copy-ready recipes with the real API names, and the pitfalls (secrets in logs, fail-open defaults, missing deadlines, retrying non-idempotent calls). For creating a project use `sekvent-new-project`; for moving an existing backend onto sekvent use `sekvent-migrate`.
---

# Building on sekvent

sekvent is a Rust backend framework: one facade crate (`sekvent`) re-exporting
focused libraries behind features, plus the `cargo sekvent` CLI. Most service
code needs `use sekvent::prelude::*;` (`AppError`, `ErrorCode`, `CallContext`,
`Secret`, `EnvConfig`, `FromConfig`, `Runtime`, `RuntimeBuilder`,
`RuntimeHandle`, `Server`, `ServerBuilder`, `Stage`, `UnitPolicy`,
`UnitContext`, `Ctx`, `ShutdownTrigger`; with the `component` feature also
`App`, `ComponentError`, `Lifecycle`).

Where commands run is the `build-on-rtx` skill's business: compile, lint and
test through `cargo sekvent check|clippy|test|gate|coverage` (forwarded to the
remote builder, or run here when `sekvent.toml` sets `[remote].mode =
"local"`), `cargo fmt` locally. Test speed and flakes: the `rust-testing`
skill.

## Crate map — which one to reach for

| Need | Module (`sekvent::…`) | Facade feature |
|---|---|---|
| Read env config, secrets | `config` | default |
| Return an error from any handler | `error` | default; `error-http`, `error-grpc` for the mappings |
| Request id, deadline, caller, clock | `context` | default |
| Logging setup, truncating untrusted text | `telemetry` | default |
| Lifecycle, health, one listener for gRPC + gRPC-Web + REST | `runtime` | default; `runtime-grpc-web` |
| Timeout, retry, rate gate, bulkhead, breaker, backoff | `resilience` | `resilience` |
| Password hashing, JWT, login | `auth` | `auth`; `auth-axum`, `auth-tonic` |
| Service-to-service tokens | `link` | `link`; `link-axum`, `link-tonic` |
| Outbound HTTP, OAuth 2.0 client credentials | `client` | `client` |
| Pools, migrations, list filters | `db` | `db-sqlx-postgres`, `db-sqlx-mysql`, `db-sea-orm-postgres`, `db-sea-orm-mysql`, `db-migrate`, `db-sea-orm-migrate` |
| A feature as a component (in-process now, a service later) | `component` (+ `#[sekvent::component]`, `sekvent::ComponentError`) | `component`; `component-grpc` for the `grpc` binding and serving; `runtime` for `App::register` |
| Postgres/MySQL test containers, `await_until!` | crate `sekvent-testing` | `[dev-dependencies]` |
| protobuf codegen in `build.rs` | crate `sekvent-proto-build` | `[build-dependencies]` |

Enable features on the crate's own `sekvent = { workspace = true, features = [...] }`
line; never add a sekvent sub-crate directly.

## Recipes

### Config struct

```rust
use std::time::Duration;
use sekvent::prelude::*;

/// Settings of the billing client.
#[derive(Debug, EnvConfig)]
#[config(prefix = "BILLING_")]
pub struct BillingConfig {
    /// Base URL of the billing API.
    pub base_url: String,
    /// Per-call timeout.
    #[config(default = "2s")]
    pub timeout: Duration,
    /// API key; `Debug` prints it redacted.
    pub api_key: Secret,
    /// Nested settings read under `BILLING_POLICY_`.
    #[config(nested)]
    pub policy: PolicySettings,
}

let config: BillingConfig = sekvent::config::from_env()?;          // main
let config = BillingConfig::from_config(                             // tests
    &sekvent::config::MapSource::new().with("BILLING_BASE_URL", "http://x").with("BILLING_API_KEY", "k"),
)?;
```

- Field types pick the reader: `Secret`, `Option<Secret>`, `Option<T>`,
  `Duration` (`5s`, `250ms`), `bool`, anything `FromStr`.
- Attributes: `key = "…"`, `default = "…"`, `secret`, `nested`,
  `validate = path::to_fn` (`fn(&T) -> Result<(), String>`).
- All failing keys are reported at once, by name, never by value.
- Keys under `SEKVENT_` are reserved; do not invent them.
- Never `std::env::set_var` in tests (it is `unsafe` in edition 2024): use
  `MapSource`.

### Errors

```rust
fn find(id: &str) -> Result<Order, AppError> {
    let row = repo.get(id).map_err(|err| AppError::internal(err))?;   // source: logged, never sent
    row.ok_or_else(|| AppError::not_found("no such order").with_reason("ORDER_NOT_FOUND"))
}

AppError::invalid_argument("the request is invalid")
    .with_field_violation("quantity", "must be positive")
    .with_metadata("max", "100");
AppError::unavailable("billing is unavailable").with_retry_after(Duration::from_secs(5));
```

- Constructors: `invalid_argument`, `not_found`, `already_exists`,
  `permission_denied`, `unauthenticated`, `failed_precondition`,
  `unavailable`, `deadline_exceeded`, `resource_exhausted`, `unimplemented`,
  `cancelled`, `internal(source)`, or `AppError::new(ErrorCode::…, msg)`.
- The message is shown to callers: no internals, ids of other tenants, SQL
  or upstream text. Put those in `.with_source(err)`.
- `is_transient()` drives retries: `UNAVAILABLE`, `DEADLINE_EXCEEDED`,
  `RESOURCE_EXHAUSTED`, `ABORTED`.
- Mapping: `impl From<AppError> for tonic::Status` (`error-grpc`), axum
  `IntoResponse` with body `{"error": {"code": "NOT_FOUND", ...}}`
  (`error-http`). Clients decode with `sekvent::error::grpc::from_status` and
  `sekvent::error::http::from_json_body`.

### gRPC + REST server with health

```rust
let server = Server::builder()
    .add_service(OrdersServiceServer::new(OrdersApi::new(repo)))   // tonic services
    .rest(rest_routes())                                            // axum Router
    .prefix("/api")                  // REST + gRPC-Web under /api; native gRPC stays at /
    .bind(config.listen_addr)        // port 0 in tests
    .await?;
let addr = server.local_addr();
let runtime = Runtime::builder()
    .shutdown_delay(config.shutdown_delay)
    .unit("api", Stage::Ingress, UnitPolicy::Critical, server.into_unit())
    .build()?;
runtime.run().await?;               // until SIGINT/SIGTERM, then drain
```

- Health is automatic: `/livez`, `/readyz`, `/healthz` at the root and
  `grpc.health.v1` (NOT_SERVING while draining).
- Every request carries a `CallContext`: axum handlers take `Ctx(cx): Ctx`;
  tonic handlers read `request.extensions().get::<CallContext>()`.
- Stages start in order `Infrastructure → Components → Workers → Ingress`
  and drain in reverse. A unit calls `ctx.ready()` once up and returns when
  `ctx.shutdown()` fires.
- Readiness from dependencies: implement `sekvent::runtime::DependencyProbe`
  and register with `.probe(..)`. `ProbeFailure` details are `&'static str`,
  never upstream text.
- Service-to-service auth on the listener:
  `let auth = sekvent::link::authenticator(link.inbound().clone());`
  `Server::builder().authenticator(move |parts| auth(parts))`.

### Worker unit

```rust
Runtime::builder().unit("sync", Stage::Workers, UnitPolicy::Critical, move |ctx: UnitContext| {
    let job = Arc::clone(&job);
    async move {
        let backoff = sekvent::resilience::Backoff::exponential(Duration::from_secs(1), Duration::from_secs(60));
        let mut delays = backoff.iter();
        let shutdown = ctx.shutdown();
        ctx.ready();
        while !shutdown.is_cancelled() {
            let pause = match job.run_once().await {
                Ok(()) => { delays = backoff.iter(); idle }
                Err(err) => { tracing::warn!(error = ?err, "sync failed"); delays.next().unwrap_or(max) }
            };
            tokio::select! { biased; () = shutdown.cancelled() => break, () = tokio::time::sleep(pause) => {} }
        }
        Ok(())
    }
})
```

`UnitPolicy::Restart(RestartPolicy { .. })` restarts a failing unit with
backoff; `BestEffort` logs and forgets; `Critical` (default choice) stops the
process. A restarting unit holds its stage until one of its runs calls
`ctx.ready()`; a run longer than `RuntimeBuilder::restart_reset_after`
(default 60s) resets the restart count, so `max_restarts` bounds crash loops.

### Outbound client with a policy

```rust
use sekvent::resilience::PolicySpec;
use sekvent::client::HttpClient;

let policy = PolicySpec::from_config(&sekvent::config::EnvSource, "BILLING_")?  // BILLING_TIMEOUT, BILLING_RETRY_MAX_ATTEMPTS, …
    .build("billing")?;
let billing = HttpClient::builder()
    .base_url(config.base_url.clone())
    .bearer_token(config.api_key.clone())
    .policy(policy)
    .build()?;

let ctx = cx.child().with_timeout(Duration::from_secs(2));      // never more time than the caller has
let invoice: Invoice = billing.get("/invoices/42").send_json(&ctx).await?;
let created: Invoice = billing.post("/invoices").json(&draft)
    .header("idempotency-key", key)
    .idempotent(true)                                           // only then may it be retried
    .send_json(&ctx).await?;
```

- Retries apply only to idempotent requests and transient errors, spend a
  shared `RetryBudget`, and stop at the context deadline.
- The breaker (`CircuitBreaker`) trips only on `UNAVAILABLE`,
  `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`; one per upstream endpoint.
- For non-HTTP calls wrap the operation: `policy.call(&ctx, idempotent, || op()).await`.
- OAuth 2.0 client credentials: `sekvent::client::oauth2::ClientCredentials::builder(..)`,
  then `.with_bearer_source(Arc::new(creds))`; one retry after a `401`.

### Database pools

```rust
use sekvent::db::{PoolRegistry, PoolSpec, assert_distinct_targets};

let main = PoolSpec::from_config(&EnvSource, "DB_")?;            // pool "db": DB_URL (Secret), DB_MAX_CONNECTIONS, …
let audit = PoolSpec::from_config(&EnvSource, "AUDIT_DB_")?;     // pool "audit_db"
assert_distinct_targets(&[("db", &main.url), ("audit_db", &audit.url)])?;  // two roles must not share one database
let pools = PoolRegistry::build(vec![main, audit]).await?;
pools.migrate_all().await?;                                      // feature db-migrate
let pg = pools.get("db")?.postgres().expect("a Postgres pool");
```

- Transactions are explicit (`pg.begin()`); pass the transaction down. Map
  errors with `sekvent::db::to_app_error` (constraint violations become
  `ALREADY_EXISTS`/`FAILED_PRECONDITION`, never SQL text).
- sea-orm: `pools.get("db")?.sea_orm()`; list endpoints use
  `sekvent::db::list` (`ListParams`, `paginate`, and a `ListSpec` that
  allow-lists sortable and filterable columns; anything else is
  `INVALID_ARGUMENT`).
- Connect errors name the pool and a failure class, never the URL.

### JWT login

```rust
use sekvent::auth::{Claims, JwtKeys, NoClaims, PasswordHasher, Validation, authenticate};

let hasher = PasswordHasher::default();
let keys = JwtKeys::hs256("k1", &config.jwt_secret)?;           // a Secret

let account = repo.find_login(&email).await?;                    // Option<Account>
let lookup = account.as_ref().map(|a| (a.user_id.clone(), a.password_hash.as_str(), a.enabled));
let user_id = match authenticate(&hasher, lookup, &password) {
    Ok(ok) => {
        if ok.needs_rehash {
            repo.set_password_hash(&ok.user, &hasher.hash(&password)?).await?;
        }
        ok.user
    }
    Err(rejected) => {
        return Err(AppError::new(rejected.error_code(), rejected.user_message())
            .with_reason(rejected.reason()));
    }
};
let now = clock.now_unix_millis() / 1000;                        // injected Clock
let token = keys.issue(&Claims::new(user_id, now, 3600, NoClaims {}))?;
let claims: Claims<NoClaims> = keys.verify(&token, now, &Validation::new())?;
```

`authenticate` runs exactly one password verification whether or not the
account exists (a dummy verify for unknown accounts) and returns the same
rejection for "no such user" and "wrong password". Do not add an early
return before it. Bearer guards: `auth-axum` (`Bearer`, `RequireRole`),
`auth-tonic` (`BearerInterceptor`).

### Service links

```rust
let links = sekvent::link::LinkConfig::from_source(&EnvSource)?;  // SEKVENT_LINK_INBOUND_<NAME>, _OUTBOUND_<NAME>, SEKVENT_LINK_TRUSTED
let to_billing = links.require_outbound("billing")?.clone();       // BearerInjector: tower Layer and tonic Interceptor
let channel = tonic::transport::Endpoint::from_shared(url)?.connect_lazy();
let client = BillingClient::with_interceptor(channel, to_billing);
```

Only links listed in `SEKVENT_LINK_TRUSTED` may assert an end-user subject
or tenant; everyone else's are stripped. Tokens are compared in constant
time and never logged.

### Test harness

```rust
#[tokio::test]
#[ignore = "needs Docker; SEKVENT_DOCKER_TESTS=1"]
async fn orders_are_persisted() {
    if !sekvent_testing::docker_tests_enabled() { return; }
    let pg = sekvent_testing::PostgresHarness::shared().await;    // one container per test binary
    let db = pg.create_database().await.unwrap();                  // fresh database per test
    // connect to db.url, run migrations, test…
    sekvent_testing::await_until!(repo.count().await == 1);        // only for effects without a signal
}
```

`sekvent-testing = { workspace = true, features = ["postgres"] }` under
`[dev-dependencies]`. Containers carry a label namespace and are reaped even
after `SIGKILL`; `cargo sekvent harness-clean` sweeps leftovers.
`cargo sekvent gate`/`test`/`coverage` skip these tests unless `sekvent.toml`
sets `[harness] docker_tests = true` (exports `SEKVENT_DOCKER_TESTS=1` and
passes `--include-ignored` to the test targets; doctests run in their own
step without it). `SEKVENT_DOCKER_TESTS=0` turns them off for one run.

### Proto build

```rust
// crates/orders-proto/build.rs — portable messages, no tonic
sekvent_proto_build::ProtoBuild::new("../../proto")
    .files(["orders/v1/orders.proto"])
    .messages_only()
    .compile()
    .unwrap_or_else(|e| panic!("{e}"));
// crates/orders/build.rs — stubs that reuse those messages
sekvent_proto_build::ProtoBuild::new("../../proto")
    .files(["orders/v1/orders.proto"])
    .services_only("::orders_proto")
    .compile()
    .unwrap_or_else(|e| panic!("{e}"));
// src/lib.rs
pub mod proto { include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs")); }   // proto::orders::v1::…
```

Generated code is included with the lint allowances it needs; never commit
generated `.rs`. The package `sekvent.v1` and the names `__sekvent_*` are
reserved. `messages_only()` and `both()` also emit one
`__sekvent_service_<Service>` constant per proto `service`, which
`#[component(proto = …)]` checks (`.service_contracts(false)` turns it off). `protoc` must be on
`PATH` (or `PROTOC`).

### Components

A component is a trait with a protobuf contract; callers hold its generated
`XHandle` and never know whether calls stay in-process (`local`), cross a
serialization boundary (`local-serialized`: prost encode, separate task,
decode) or go to another process (`grpc`). Milestones C1 and C2: `#[call]`
methods only (no `#[async_call]`/`#[deferred]` yet). Specs:
`docs/design/component-c1.md`, `docs/design/component-c2.md`; full example
with a split topology: `examples/shop`. Facade features `component`,
`component-grpc` (the `grpc` binding and `App::grpc_routes`), `runtime`
(`App::register`).

Declare it in an `-api` crate: the `.proto` with the component's `service`,
the messages from `sekvent-proto-build` `.messages_only()`, the trait, the
error. The proto is the contract:

```proto
// proto/shop/inventory/v1/inventory.proto
syntax = "proto3";
package shop.inventory.v1;
service Inventory {                                    // <Trait>, in package = "…"
  rpc Reserve(ReserveRequest) returns (ReserveReply);  // one RPC per method, UpperCamelCase
}
message ReserveRequest { string order_id = 1; string sku = 2; uint32 quantity = 3; }
message ReserveReply { string reservation_id = 1; uint32 remaining = 2; }
```

```rust
use sekvent::prelude::*;                                  // App, ComponentError, Lifecycle, CallContext, AppError
pub mod proto { include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs")); }
use proto::shop::inventory::v1::{ReserveReply, ReserveRequest};

#[derive(Debug, ComponentError)]
#[component_error(domain = "shop.inventory.v1")]          // optional; decoding then requires it
pub enum InventoryError {
    #[reason("OUT_OF_STOCK", code = FailedPrecondition, message = "only {available} of {sku} left")]
    OutOfStock { sku: String, available: u32 },           // fields travel as metadata (Display + FromStr)
    #[other]
    Other(AppError),                                      // unknown reasons and framework errors land here
}

#[sekvent::component(
    name = "inventory",
    package = "shop.inventory.v1",
    proto = "crate::proto::shop::inventory::v1",          // the generated module; checked at compile time
)]
pub trait Inventory: Send + Sync + 'static {
    /// Reserve stock for an order.
    #[call(idempotent, timeout = "2s", bulkhead = 16)]
    async fn reserve(&self, cx: &CallContext, req: ReserveRequest)
        -> Result<ReserveReply, InventoryError>;
}
// generates InventoryHandle (Clone): .reserve(&cx, req), ::install, ::install_with_lifecycle, .binding()
```

- Every method: `async fn m(&self, cx: &CallContext, req: Req) -> Result<Rep, E>`;
  `Req`/`Rep` are prost messages, `E` implements `ComponentError` (or is
  `AppError`). `local_only` components take plain Rust types and need no
  `package` or `proto` (and must not have `proto`); `remote_only` ones have
  `install_remote` and must be bound `grpc`.
- `proto` is required for standard and `remote_only` components. The macro
  fails to compile (`component contract: …`) when the proto service is not
  `<package>.<Trait>`, when the RPC set differs from the methods, or when a
  method's request or reply is not its RPC's input or output type. Change
  trait and proto together.
- The derive gives `From<E> for AppError` and `From<AppError> for E`; do not
  add another `From<AppError>`.

Implement with plain `async fn`s, and wire with constructor injection:

```rust
impl Inventory for InventoryService {
    async fn reserve(&self, cx: &CallContext, req: ReserveRequest) -> Result<ReserveReply, InventoryError> { … }
}

let mut builder = App::builder(&sekvent::config::EnvSource);
InventoryHandle::install(&mut builder, move |_deps| Ok(InventoryService::new(stock)))?;
NotificationsHandle::install_with_lifecycle(&mut builder, |_deps| Ok(NotificationsService::new()))?; // runs Lifecycle::on_start/on_stop
OrdersHandle::install(&mut builder, |deps| Ok(OrdersService::new(
    deps.handle::<InventoryHandle>()?,                 // only components installed earlier
    deps.handle::<NotificationsHandle>()?,
)))?;
let app = builder.build()?;                            // BuildError names the key, never the value
app.register(Runtime::builder()).build()?.run().await?; // one unit in Stage::Components
// or standalone: app.start().await?; …; app.stop(grace).await?;
let orders = app.handle::<OrdersHandle>()?;            // for ingress code and tests
```

- Factories run in `build()`, in install order, only for local bindings;
  shared resources go in with `builder.provide(value)?` and out with
  `deps.resource::<T>()?`. Give each component its own pool (a newtype),
  never a shared one.
- Components start in install order and stop in reverse. Stopping drains:
  new calls get `UNAVAILABLE`/`COMPONENT_DRAINING`, in-flight calls finish
  within the grace.
- Per call: a cancelled or expired context is rejected before any work;
  the method timeout is capped by the caller's deadline (`DEADLINE_EXCEEDED`);
  a full bulkhead sheds at once (`RESOURCE_EXHAUSTED`/`BULKHEAD_FULL`, no
  queue). Reason constants: `sekvent::component::reasons`.
- Locally, the callee sees `cx.caller() == trusted("local")` with request
  id, subject, tenant and idempotency key kept. Over gRPC the caller is the
  authenticated link; subject and tenant survive only for a link listed in
  `SEKVENT_LINK_TRUSTED`.
- Every call counts a hop; a chain deeper than `SEKVENT_COMPONENT_MAX_HOPS`
  (default 16) fails `FAILED_PRECONDITION`/`CALL_DEPTH_EXCEEDED` before
  anything is sent. It catches call cycles; it is not a security control.

Serving a component over gRPC is configuration plus one line of wiring: the
hosting process mounts `app.grpc_routes()` on its server (next to any other
tonic service) and the environment exposes the component:

```rust
let app = builder.build()?;                            // SEKVENT_COMPONENT_INVENTORY_SERVE=grpc
let server = Server::builder()
    .grpc_routes(app.grpc_routes())                    // every exposed component; required, else start() fails GRPC_NOT_MOUNTED
    .bind(addr)
    .await?;
app.register(Runtime::builder())
    .unit("grpc", Stage::Ingress, UnitPolicy::default(), server.into_unit())
    .build()?
    .run()
    .await?;
// grpc.health.v1 reports "shop.inventory.v1.Inventory" SERVING while the component runs
```

The caller installs the component exactly as before (`InventoryHandle::install`);
bound `grpc`, its factory never runs. The wire is plain gRPC on
`/<package>.<Trait>/<Rpc>`, so a client generated from the `.proto` by any
toolchain can call the server with `authorization: Bearer <token>`.

Configuration keys (all optional unless noted; unknown keys under
`SEKVENT_COMPONENT_` and `SEKVENT_POLICY_`, malformed values and zeros fail
the build; every key is accepted under every binding, so one environment
works for every topology):

| Key | Values |
|---|---|
| `SEKVENT_COMPONENT_BINDING` | `local` (default), `local-serialized`, `grpc` — all standard components |
| `SEKVENT_COMPONENT_<C>_BINDING` | same, one component |
| `SEKVENT_COMPONENT_<C>_ENDPOINT` | `http://host:port` (required for `grpc`; `https://` is rejected — plaintext on a private network or behind a TLS-terminating mesh) |
| `SEKVENT_COMPONENT_<C>_LINK` | link name (default: the component name); the binding presents `SEKVENT_LINK_OUTBOUND_<LINK>` (required unless `AUTH=none`) |
| `SEKVENT_COMPONENT_<C>_AUTH` | `link` (default) or `none` (logged at `warn!`) |
| `SEKVENT_COMPONENT_<C>_SERVE` | `none` (default) or `grpc`: expose a locally bound component |
| `SEKVENT_COMPONENT_<C>_SERVE_AUTH` | `link` (default: needs at least one `SEKVENT_LINK_INBOUND_<CALLER>`) or `none` |
| `SEKVENT_COMPONENT_MAX_HOPS` | 1–1000, default 16 |
| `SEKVENT_COMPONENT_<C>_POLICY`, `SEKVENT_COMPONENT_<C>_<M>_POLICY` | a named policy `<N>` |
| `SEKVENT_POLICY_<N>_<FIELD>` | fields of a named policy |
| `SEKVENT_COMPONENT_<C>_<FIELD>`, `SEKVENT_COMPONENT_<C>_<M>_<FIELD>` | component defaults, one method |

Fields (grammar of `PolicySpec::from_config`): `TIMEOUT` (`250ms`, `2s`;
the whole call's deadline, retries included), `BULKHEAD_MAX_CONCURRENT`,
`BULKHEAD_MAX_QUEUE`, `BULKHEAD_QUEUE_TIMEOUT` (act where the component
runs), `RETRY_MAX_ATTEMPTS`, `RETRY_INITIAL_BACKOFF`, `RETRY_MAX_BACKOFF`,
`RETRY_MULTIPLIER`, `RETRY_JITTER`, `RETRY_MAX_RETRY_AFTER` (`grpc` caller,
`idempotent` methods only), and component-only `RETRY_BUDGET_RATIO`,
`RETRY_BUDGET_MIN_PER_SEC`, `BREAKER_ENABLED`, `BREAKER_FAILURE_RATE`,
`BREAKER_WINDOW`, `BREAKER_MIN_CALLS`, `BREAKER_WAIT_IN_OPEN`,
`BREAKER_PERMITTED_IN_HALF_OPEN` (one breaker and budget per remote
component). `RATE_LIMIT_*` is an unknown key.

`<C>`/`<M>`/`<N>` are upper-cased names
(`SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT`, `SEKVENT_POLICY_REMOTE_TIMEOUT`).
Precedence, lowest first: framework default (remote only: 3 attempts, a
breaker at 50 % of the last 20 calls, min 10, 30 s open, 3 probes, budget
20 % + 10/s, `Retry-After` up to 30 s), `#[call]` attribute, named policy
(the method's `POLICY`, else the component's), component key, method key.
`RETRY_MAX_ATTEMPTS=1` / `BREAKER_ENABLED=false` switch the defaults off.
Configuration never makes a method retryable: only `#[call(idempotent)]`.

Remote errors: a server's `AppError` (typed variants included) survives the
hop unchanged; errors made on the caller side carry `component`/`method`
metadata: `UNAVAILABLE`/`COMPONENT_UNREACHABLE` (connect refused, reset),
`UNAVAILABLE`/`CIRCUIT_OPEN` (with `retry_after`), `UNAUTHENTICATED`
("service authentication required", same answer for a missing or wrong
token). The breaker trips only on `UNAVAILABLE`, `DEADLINE_EXCEEDED`,
`RESOURCE_EXHAUSTED`; business errors never open it.

Contracts: `[contract]` in `sekvent.toml` lists the proto roots
(`roots = ["crates/billing-api/proto"]`, `baseline = "contracts"`,
`gate = true`). `cargo sekvent contract emit [service…]` writes one
canonical JSON baseline per service — commit them; `cargo sekvent contract
check [service…]` fails on wire-breaking changes (a removed RPC or
unreserved field number, a changed type, cardinality or oneof, a dropped
reservation) and runs in the gate. Both compile the protos in-process
(protox: no `protoc`, no Rust build). Renames and additions are compatible.
A breaking change goes into a new package (`billing.v2`) served alongside
v1 as a second component; delete v1 and its baseline once no caller uses it.

Test every scenario under every binding in one `cargo test`, so a type that
does not survive the wire fails early; a `split-grpc` profile runs the
hosting App in the same test process on `127.0.0.1:0` (see
`examples/shop/shop/tests/support/mod.rs`):

```rust
#[derive(Debug, Clone, Copy)]
enum Profile { MonolithLocal, MonolithSerialized }
impl Profile {
    fn source(self) -> MapSource {
        match self {
            Self::MonolithLocal => MapSource::new(),
            Self::MonolithSerialized => MapSource::new().with("SEKVENT_COMPONENT_BINDING", "local-serialized"),
        }
    }
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]                    // paused clock for deadline tests
async fn reserve_times_out(#[case] profile: Profile) {
    let source = profile.source().with("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT", "50ms");
    let mut builder = App::builder(&source);
    InventoryHandle::install(&mut builder, |_deps| Ok(PendingInventory)).unwrap(); // fakes go through install too
    let app = builder.build().unwrap();
    app.start().await.unwrap();
    let error = app.handle::<InventoryHandle>().unwrap().reserve(&CallContext::new(), req).await.unwrap_err();
    // InventoryError::Other(e) with e.code() == ErrorCode::DeadlineExceeded
}
```

Caller deadlines in paused-clock tests come from the tokio clock:
`cx.with_deadline((tokio::time::Instant::now() + d).into_std())`, not
`with_timeout`. Signal entry and exit from fakes with channels, a
`Semaphore` or a `oneshot` fired from a `Drop` guard; never sleep. Anything
over a socket (the `split-grpc` profile) runs on the real clock: write the
timed scenario as one `async fn(profile)`, call it from a
`start_paused = true` test for the in-process profiles and from a plain
`#[tokio::test]` twin wrapped in a 30 s `tokio::time::timeout`, and assert
only lower time bounds there. Observe external state (health, a closed
listener) with `await_until!`.

## Pitfalls

- **Secrets in logs.** Never log, `format!`, `Debug`-print or return a
  `Secret`, token, password, database URL, `Authorization` header or
  upstream body. `Secret::expose()` only at the point of use. Use
  `sekvent::telemetry::truncate_for_log` for untrusted text you must log.
- **Fail closed.** A missing key, binding or token is a startup error. No
  "if unset, allow everyone"; no default secrets outside tests
  (`JwtKeys::development()` is for tests).
- **Deadlines.** Derive outbound contexts from the inbound one
  (`cx.child()`); queued or background work uses `cx.detached()` so it does
  not inherit a request's deadline or cancellation. Every outbound call has
  a timeout.
- **Retries.** Only idempotent operations, only transient codes, within a
  budget and the deadline. A `POST` without an idempotency key is never
  retried. Each remote hop retries on its own, so a chain of n hops can
  multiply load by 3ⁿ: set `RETRY_MAX_ATTEMPTS=1` on inner hops.
- **Component links.** Never reuse one token for two links or directions
  (the build refuses it), never set `AUTH=none`/`SERVE_AUTH=none` outside a
  test, and list a caller in `SEKVENT_LINK_TRUSTED` only when it really
  vouches for the end user.
- **Time and randomness.** Inject `Clock` (`SystemClock` / `ManualClock`)
  and seedable RNGs where behaviour depends on them; JWT and login APIs take
  `now_unix_secs` explicitly.
- **Tests.** No fixed sleeps: completion signals, `RuntimeHandle::wait`,
  `tokio::time::pause`. Servers bind port 0.
- **Thin `main`.** Telemetry, config, `serve`; everything else in the
  library so the coverage gate can see it.
