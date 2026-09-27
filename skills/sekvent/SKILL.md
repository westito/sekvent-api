---
name: sekvent
description: Write or change code in a Rust backend built on the sekvent framework (a workspace with `sekvent.toml` and a `sekvent` dependency). Use when adding a config struct, an error, a gRPC or REST endpoint, a background worker, an outbound HTTP client with retries or a breaker, a database pool, a JWT login, service-to-service authentication, a container-backed test or protobuf codegen — and before hand-rolling any of those, since sekvent already has them. Gives the crate map, copy-ready recipes with the real API names, and the pitfalls (secrets in logs, fail-open defaults, missing deadlines, retrying non-idempotent calls). For creating a project use `sekvent-new-project`; for moving an existing backend onto sekvent use `sekvent-migrate`.
---

# Building on sekvent

sekvent is a Rust backend framework: one facade crate (`sekvent`) re-exporting
focused libraries behind features, plus the `cargo sekvent` CLI. Most service
code needs `use sekvent::prelude::*;` (`AppError`, `ErrorCode`, `CallContext`,
`Secret`, `EnvConfig`, `FromConfig`, `Runtime`, `RuntimeBuilder`,
`RuntimeHandle`, `Server`, `ServerBuilder`, `Stage`, `UnitPolicy`,
`UnitContext`, `Ctx`, `ShutdownTrigger`).

Where commands run is the `build-on-rtx` skill's business: compile, lint and
test through `cargo sekvent check|clippy|test|gate|coverage` (forwarded to the
remote builder), `cargo fmt` locally. Test speed and flakes: the
`rust-testing` skill.

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
process.

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
  `sekvent::db::list` (`ListParams`, column filters).
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
generated `.rs`. The package `sekvent.v1` is reserved. `protoc` must be on
`PATH` (or `PROTOC`).

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
  retried.
- **Time and randomness.** Inject `Clock` (`SystemClock` / `ManualClock`)
  and seedable RNGs where behaviour depends on them; JWT and login APIs take
  `now_unix_secs` explicitly.
- **Tests.** No fixed sleeps: completion signals, `RuntimeHandle::wait`,
  `tokio::time::pause`. Servers bind port 0.
- **Thin `main`.** Telemetry, config, `serve`; everything else in the
  library so the coverage gate can see it.
