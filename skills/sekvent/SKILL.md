---
name: sekvent
description: Write or change code in a Rust backend built on the sekvent framework (a workspace with `sekvent.toml` and a `sekvent-api` dependency). Use when adding a config struct, an error, a gRPC or REST endpoint, CORS or a body limit, a background worker, a periodic or singleton job, a database lease, an outbound HTTP client with retries or a breaker, a small TTL cache, a database pool or its readiness probe, a JWT login or bcrypt-compatible passwords, browser single sign-on (Bitbucket Cloud), a file download or upload, service-to-service authentication, a container-backed test, protobuf codegen or a component (a trait that runs in-process now and can move to its own service later) — and before hand-rolling any of those, since sekvent already has them. Gives the crate map, copy-ready recipes with the real API names, and the pitfalls (secrets in logs, fail-open defaults, missing deadlines, retrying non-idempotent calls). For creating a project use `sekvent-new-project`; for moving an existing backend onto sekvent use `sekvent-migrate`.
---

# Building on sekvent

sekvent is a Rust backend framework: one facade package (`sekvent-api`, whose
library is `sekvent`) re-exporting focused libraries behind features, plus the
`cargo sekvent` CLI. Most service
code needs `use sekvent::prelude::*;` (`AppError`, `ErrorCode`, `CallContext`,
`Secret`, `EnvConfig`, `FromConfig`, `Runtime`, `RuntimeBuilder`,
`RuntimeHandle`, `Server`, `ServerBuilder`, `Stage`, `UnitPolicy`,
`UnitContext`, `Ctx`, `ShutdownTrigger`, `JobSpec`, `JobContext`; with the `component` feature also
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
| Logging setup, request ids, access log, truncating untrusted text | `telemetry` | default |
| Lifecycle, health, one listener for gRPC + gRPC-Web + REST, CORS, body limits, global layers | `runtime` | default; `runtime-grpc-web` |
| Interval, cron and manual jobs | `runtime` (`JobSpec`, `RuntimeBuilder::job`) | default; `runtime-cron` for cron |
| File downloads (safe headers, type sniffing, filenames) | `runtime` (`Download`) | default |
| Timeout, retry, rate gate, bulkhead, breaker, backoff, TTL cache | `resilience` | `resilience` |
| Password hashing (argon2id, bcrypt), JWT, login | `auth` | `auth`; `auth-axum`, `auth-tonic`, `auth-tokio` (async hashing) |
| Browser sign-in with an external account (OAuth 2.0 code flow, Bitbucket Cloud) | `sso` | `sso` |
| Service-to-service tokens | `link` | `link`; `link-axum`, `link-tonic` |
| Outbound HTTP, OAuth 2.0 client credentials | `client` | `client` |
| Pools, migrations, list filters | `db` | `db-sqlx-postgres`, `db-sqlx-mysql`, `db-sea-orm-postgres`, `db-sea-orm-mysql`, `db-migrate`, `db-sea-orm-migrate` |
| Readiness probes per pool | `db` (`PoolRegistry::probes`) | `db` + `runtime` + a sqlx backend |
| Leases with fencing tokens, singleton jobs | `db` (`LeaseStore`, `LeaseGuard`) | `db-lease` + a sqlx backend; `runtime` for `LeaseGuard` |
| A feature as a component (in-process now, a service later) | `component` (+ `#[sekvent::component]`, `sekvent::ComponentError`) | `component`; `component-grpc` for the `grpc` binding and serving; `runtime` for `App::register` |
| Postgres/MySQL test containers, `await_until!` | crate `sekvent-testing` | `[dev-dependencies]` |
| protobuf codegen in `build.rs` | crate `sekvent-proto-build` | `[build-dependencies]` |

Enable features on the crate's own `sekvent-api = { workspace = true, features = [...] }`
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
- Those two conversions log a server-side failure (`UNKNOWN`, `INTERNAL`,
  `DATA_LOSS`) once: `error` event, target `sekvent::error`, message
  `request failed`, fields `code`, `reason` and `source` (the source chain
  joined with `": "`, capped at 2 KiB); never the message or metadata. Do not
  log such an error yourself before returning it. Caller errors are not
  logged (the access log has them). The component gRPC binding logs the same
  way; `grpc::to_status` and `to_wire` alone never log.

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
- Every request except health carries a `CallContext`: axum handlers take
  `Ctx(cx): Ctx`; tonic handlers read
  `request.extensions().get::<CallContext>()`. Its request id is the
  response's `x-request-id` (a valid incoming one is kept, otherwise a
  UUID v7 is minted).
- Stages start in order `Infrastructure → Components → Workers → Ingress`
  and drain in reverse. A unit calls `ctx.ready()` once up and returns when
  `ctx.shutdown()` fires.
- Readiness from dependencies: implement `sekvent::runtime::DependencyProbe`
  and register with `.probe(..)`. `ProbeFailure` details are `&'static str`,
  never upstream text. Database pools have ready-made probes (see
  "Database probes").
- Service-to-service auth on the listener:
  `let auth = sekvent::link::authenticator(link.inbound().clone());`
  `Server::builder().authenticator(move |parts| auth(parts))`.

### Server: CORS, limits, layers, access log

```rust
use sekvent::runtime::Cors;

let mut builder = Server::builder()
    .add_service(OrdersServiceServer::new(api).max_decoding_message_size(16 << 20)) // gRPC limits: per service
    .rest(rest_routes())
    .prefix("/api")
    .rest_body_limit(4 * 1024 * 1024)        // REST extractors; default 2 MiB, 413 above
    .layer(AuditLayer::new(audit))           // every REST, gRPC and gRPC-Web request, never health
    .access_log(true);                       // default: one event per request
// APP_CORS_ORIGINS (`*` or a comma list), _CREDENTIALS, _MAX_AGE, _ALLOW_HEADERS, _EXPOSE_HEADERS
if let Some(cors) = Cors::from_config(&EnvSource, "APP_CORS_")? {   // unset or blank ORIGINS: CORS off
    builder = builder.cors(cors);
}
let server = builder.bind(config.listen_addr).await?;
// in code: Cors::origins(["https://app.example.com"])?.allow_credentials(true).max_age(Duration::from_secs(600))
```

- The stack, outermost first: request id and access log, CORS, the health
  endpoints, the call context (authenticator), your `.layer(..)`s (a later
  call wraps the earlier ones, as `Router::layer`), then routing. A
  preflight never reaches the authenticator or a handler; health (HTTP and
  `grpc.health.v1`) never sees your layers or the authenticator.
- CORS covers REST and gRPC-Web. Origins are `http(s)://host[:port]`,
  normalized (IPv6 hosts included, e.g. `http://[::1]:8080`); a path, `*`
  inside a list, `null` or an empty list is `INVALID_ARGUMENT` naming the
  key and the entry's index, never its text. Credentials with
  `Cors::any_origin()` fail `bind`.
  Defaults allow `authorization`, `content-type`, `grpc-timeout`,
  `idempotency-key`, `traceparent`, `x-grpc-web`, `x-request-id`,
  `x-user-agent` and expose `grpc-status`, `grpc-message`,
  `grpc-status-details-bin`, `content-disposition`, `x-request-id`; add to
  them with `.allow_headers(&[..])` / `.expose_headers(&[..])`.
  `Cors::config_keys(prefix)` lists the keys for an env template.
- Configured CORS is authoritative: `Access-Control-*` headers set by a
  handler or layer are stripped. Only a real preflight (`OPTIONS` with
  `Origin` and `Access-Control-Request-Method`) is answered by CORS; any
  other `OPTIONS` reaches your routes.
- `rest_body_limit` is axum's `DefaultBodyLimit`, so one route may raise it
  with its own `DefaultBodyLimit::max(..)` (see "Downloads and uploads").
  A handler that reads `Body` raw bounds it itself
  (`http_body_util::Limited`).
- gRPC message sizes are set on each generated server
  (`max_decoding_message_size`, `max_encoding_message_size`; tonic's
  default is 4 MiB); the component service keeps a fixed 4 MiB.
- The access log is one event per request, target `sekvent::access`,
  message `request completed`, with `method`, `path` (never the query),
  `route`, `protocol`, `status`, `grpc_status` (gRPC-Web text included),
  `latency_ms`, `aborted` (a `HEAD` probe is not aborted) and `request_id`;
  `warn` for 5xx and gRPC `UNKNOWN`/`INTERNAL`/`DATA_LOSS`, `debug` for
  health, `info` otherwise. `SEKVENT_LOG=info,sekvent::access=warn`
  silences routine requests; `.access_log(false)` drops the event but
  keeps request ids and the `request` span.
  `sekvent::telemetry::access_log::AccessLogLayer` is the same layer for a
  router you serve yourself.

### Downloads and uploads

```rust
use sekvent::runtime::{CacheControl, Disposition, Download};

async fn invoice_pdf(Ctx(cx): Ctx, Path(id): Path<String>) -> Result<Download, AppError> {
    let pdf = invoices.pdf(&cx, &id).await?;                       // Vec<u8> / Bytes
    Ok(Download::bytes(pdf)
        .filename(&format!("invoice-{id}.pdf"))                    // sanitized; filename* for non-ASCII
        .disposition(Disposition::Inline)                          // default Attachment
        .cache(CacheControl::NoStore))                             // default: `private, no-cache`
}
// Text is never sniffed: set CSV, JSON and plain text explicitly.
Download::bytes(csv).filename("orders.csv").content_type("text/csv; charset=utf-8")?;
Download::stream(axum::body::Body::from_stream(rows), Some(len)).filename("export.ndjson");
```

- Headers: `Content-Type` (explicit, else sniffed from a `bytes` body by
  magic bytes, else `application/octet-stream`), `Content-Disposition`,
  `Content-Length`, `Cache-Control`, `X-Content-Type-Options: nosniff`.
- `Inline` is kept only for PDF, PNG, JPEG, GIF, WebP, AVIF, `text/plain`,
  audio and video; anything else (HTML, SVG, XML, unknown) is sent as an
  attachment, which closes stored XSS through downloads.
- `sanitize_filename`, `content_disposition` and `sniff_content_type` are
  public for code that builds headers or stores files itself.
- `Download`'s `Debug` never prints the body.

Uploads: raise the limit on the upload route alone, enable axum's
`multipart` feature in the service's manifest, cap each field while
reading it, decide the type from the first bytes and store under a name
the server chooses:

```rust
use axum::extract::{DefaultBodyLimit, Multipart};
use sekvent::runtime::{sanitize_filename, sniff_content_type};

let rest = Router::new()
    .route("/files", post(upload).route_layer(DefaultBodyLimit::max(16 * 1024 * 1024))) // this route only
    .route("/orders", post(create_order));                                              // keeps rest_body_limit

async fn upload(Ctx(cx): Ctx, mut multipart: Multipart) -> Result<Json<Stored>, AppError> {
    let malformed = || AppError::invalid_argument("malformed upload");
    while let Some(mut field) = multipart.next_field().await.map_err(|_| malformed())? {
        let original = field.file_name().map(sanitize_filename);          // metadata only
        let mut data = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(|_| malformed())? {
            if data.len() + chunk.len() > MAX_FILE_BYTES {
                return Err(AppError::invalid_argument("the file is too large"));
            }
            data.extend_from_slice(&chunk);
        }
        let content_type = sniff_content_type(&data, original.as_deref()); // never the client's Content-Type
        files.store(&cx, new_file_id(), content_type, original, data).await?; // server-chosen name
    }
    …
}
```
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

For periodic or on-demand work use a job instead of a hand-written loop.

### Jobs

```rust
let cleanup = JobSpec::interval(Duration::from_secs(600))   // ticks at start + 10m, + 20m, … however long runs take
    .jitter(Duration::from_secs(30))                        // below the interval; never moves the grid
    .timeout(Duration::from_secs(120));                     // dropped at once: DEADLINE_EXCEEDED / JOB_TIMED_OUT
let report = JobSpec::cron("0 30 2 * * *")?;                // feature runtime-cron; UTC; 5 fields, or 6 with seconds
let reindex = JobSpec::manual();                            // runs only when triggered
let reindex_now = reindex.handle();                         // usable before registration: give it to the admin API

let runtime = Runtime::builder()
    .job("sessions-cleanup", Stage::Workers, cleanup, {
        let repo = repo.clone();
        move |cx: JobContext| {
            let repo = repo.clone();
            async move { repo.delete_expired(&cx.call_context()).await }   // Result<(), AppError>
        }
    })
    .job("daily-report", Stage::Workers, report, move |cx| reports.clone().build(cx))
    .job("reindex", Stage::Workers, reindex, move |cx| search.clone().reindex(cx))
    .unit("api", Stage::Ingress, UnitPolicy::Critical, server.into_unit())
    .build()?;

// in the admin handler
let started = reindex_now.trigger().await.map_err(AppError::from)?;  // RunStarted { run_id, fence }
let status = reindex_now.status();   // state, current, last, next_tick, runs, failures, skipped
```

- A job is one unit (`UnitPolicy::Critical`) that never fails because a
  run failed: an `Err` (`warn!`) or a panic (`error!`, `INTERNAL` /
  `JOB_PANICKED`) lands in `JobStatus` and the next tick runs. Failed runs
  are not retried.
- Fixed cadence: in-process ticks are `start + initial_delay + k × period`
  on tokio's clock; `initial_delay` defaults to one period
  (`.initial_delay(Duration::ZERO)` runs at start). A tick that finds the
  previous run still going is skipped (overlap); a tick that starts later
  than `.misfire_grace(..)` (default 1 min, at least 1 s) is skipped too.
  After a clock jump or a stall, every tick due before the current one is
  passed over: one run per stall, never a burst. Passed-over ticks count
  in `JobStatus.skipped`; each pass-over logs one `info` event.
- `JobContext`: `job()`, `run_id()` (a fresh UUID v7, also on the run's
  `job` span), `trigger()` (`RunTrigger::Schedule`, `CatchUp`, `Manual`),
  `tick()`, `fence()`, `is_cancelled()`, `cancelled()`, `cancel_token()`
  (a child token for spawned tasks), `cancel_reason()` (`Shutdown`,
  `Timeout`, `LeaseLost`) and `call_context()`, a root `CallContext` with
  the run id, the run's cancellation and the timeout as deadline; pass it
  to everything the run calls.
- On drain the run's token fires and the run is awaited until the stop
  deadline; check `cx.is_cancelled()` between batches (or await
  `cx.cancelled()` in a `select!`). A timeout or a lost lease drops the
  run at once, and a run whose lease or time is already gone never starts
  the closure.
- `trigger()` starts a run now (`RunTrigger::Manual`, no jitter) or fails
  with a `TriggerError`; `error.kind()` is a `TriggerErrorKind`:
  `AlreadyRunning` → `FAILED_PRECONDITION` / `JOB_ALREADY_RUNNING`,
  `HeldElsewhere` → `FAILED_PRECONDITION` / `JOB_HELD_ELSEWHERE`,
  `NotRunning` (before start, while draining, after stop) → `UNAVAILABLE`
  / `JOB_NOT_RUNNING`, `GuardFailed(code)` → `UNAVAILABLE` /
  `JOB_GUARD_FAILED`. Reasons: `sekvent::runtime::reasons`.
- Guard calls are bounded: a tick's acquire waits at most its misfire
  grace (a manual trigger's a fixed cap), and a permit obtained too late
  is released and the tick skipped. A panicking guard is a guard error
  (`GUARD_PANICKED`), not a crash.
- `build()` fails with `INVALID_ARGUMENT` naming the job for a name outside
  `[A-Za-z0-9._:-]{1,100}`, a zero interval or timeout, jitter not below
  the interval (or above 1 h for cron), a cron pattern that never fires
  again, a `misfire_grace` below 1 s, a singleton interval below 1 s or
  not in whole milliseconds, and `jitter`, `initial_delay` or
  `misfire_grace` on a manual job.
- Tests: `#[tokio::test(start_paused = true)]`, a runtime built
  `without_signals()`, runs reporting through channels. Cron and singleton
  schedules read the wall clock:
  `.clock(Arc::new(sekvent::runtime::TokioWallClock::new(start)))`
  makes it follow the paused tokio clock; `.jitter_rng(StdRng::seed_from_u64(7))`
  fixes the jitter.

### Singleton jobs and leases

A singleton job runs on one instance at a time across every replica
(features `db-lease`, `runtime` and a sqlx backend):

```rust
use sekvent::db::{DEFAULT_LEASE_TTL, FencingToken, LeaseGuard, LeaseStore};

let store = LeaseStore::new(pools.get("orders_db")?).with_holder(&pod_name)?; // holder: shown in `info`, never a secret
store.verify_schema().await?;      // FAILED_PRECONDITION / LEASE_SCHEMA_MISSING when the table is absent
// the app owns the table: put store.schema_sql() into a migration (or call store.ensure_schema())

let spec = JobSpec::interval(Duration::from_secs(15 * 60))      // singleton: :00, :15, :30, :45 UTC on every instance
    .singleton(LeaseGuard::new(store.clone()));                  // lease named after the job, TTL 30 s
let builder = builder.job("orders-sync", Stage::Workers, spec, move |cx| sync(cx, pg.clone(), store.clone()));

async fn sync(cx: JobContext, pg: PgPool, store: LeaseStore) -> Result<(), AppError> {
    let fence = FencingToken::new(cx.fence().expect("singleton runs carry a fence"));
    let mut tx = pg.begin().await.map_err(sekvent::db::to_app_error)?;
    store.check_fence_postgres(&mut *tx, "orders-sync", fence).await?; // ABORTED / LEASE_LOST after a takeover
    // … writes …
    tx.commit().await.map_err(sekvent::db::to_app_error)?;
    Ok(())
}
```

- Each tick runs on exactly one instance: the guard takes the lease row at
  the tick, releases it after the run, and the row remembers the last tick.
  Expiry comes from the database clock; a heartbeat renews every `ttl / 3`.
  A run still going at the next tick holds the lease, so every instance
  skips that tick. After downtime one catch-up run for the latest missed
  tick happens if it is within `misfire_grace`; older ticks never run.
- Losing the lease cancels the run (`cancel_reason()` is `LeaseLost`,
  outcome `ABORTED` / `LEASE_LOST`). Every acquisition increments the
  fencing token: guard writes with `check_fence_postgres` /
  `check_fence_mysql` inside the transaction, or store the fence in the
  rows (`WHERE last_fence <= $fence`).
- `LeaseGuard::with_ttl(..)?` changes the TTL (1 s to 24 h);
  `.with_lease_name("orders-sync")?` shares one lease between several jobs
  or direct lease users, so they exclude each other.
- Acquisition is one transaction (claim and fence read); the lease's
  validity is measured from just before the claim. An acquisition whose
  outcome is unknown (e.g. the commit's reply was lost) is released in the
  background by owner id, so it never blocks the lease for a full TTL.

On-demand exclusive work, two shapes:

```rust
// 1. A manual job: the runtime drives the lease, heartbeat, drain and panics.
let spec = JobSpec::manual().singleton(LeaseGuard::new(store.clone()).with_lease_name("orders-sync")?);
let sync = spec.handle();                      // give it to the RPC service
let builder = builder.job("orders-full-sync", Stage::Workers, spec, move |cx| full_sync(cx));
// in the RPC: sync.trigger().await.map_err(AppError::from)?  → FAILED_PRECONDITION / JOB_HELD_ELSEWHERE …

// 2. Direct: code that is not a job holds the lease itself.
let Some(lease) = store.try_acquire("orders-sync", DEFAULT_LEASE_TTL).await? else {
    return Err(AppError::failed_precondition("a sync is already running").with_reason("SYNC_RUNNING"));
};
let held = lease.keep_alive();                 // renews in the background; held.lost() fires when it is gone
let fence = held.fence();
let lost = held.lost();                        // an owned token: select! must not borrow a temporary
tokio::select! {
    result = sync_all(fence) => { held.release().await?; result }
    () = lost.cancelled() => Err(AppError::new(ErrorCode::Aborted, "the sync lost its lease")),
}
```

`keep_alive()` on a lease that has already expired returns one whose
`lost()` has fired. A `release()` cancelled midway still releases in the
background on drop.
`Lease` also has `renew()` and `release()` for hand-driven renewal;
`store.info(name)` reports holder, fence and remaining time;
`store.try_acquire_tick(name, tick, ttl)` and `store.last_tick(name)` are
the per-tick primitives the guard uses. Lease names are 1–200 bytes of
`[A-Za-z0-9._:/-]`; the default table is `sekvent_leases`
(`.with_table(..)?` to change it).
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
- The workspace `reqwest` pin has no built-in crypto provider (rustls with
  ring, supplied by sekvent), so `reqwest::Client::new()` panics. A plain
  `reqwest` client, e.g. in tests, starts from
  `sekvent::client::reqwest_builder()`, which installs ring as the
  process-wide rustls provider unless one is installed; reqwest's own TLS
  settings then apply.

### TTL cache

```rust
use sekvent::resilience::TtlCache;

let permissions: TtlCache<String, Arc<Permissions>> = TtlCache::builder(Duration::from_secs(60))
    .max_entries(1_000)                          // default 10 000
    .stale_if_error(Duration::from_secs(300))    // serve the expired value while reloads fail transiently
    .build()?;
let perms = permissions
    .get_or_try_insert(user_id.clone(), || iam.load_permissions(&cx, &user_id))   // one load per key, however many wait
    .await?;
permissions.invalidate(&user_id);                // after a change
```

- `TtlCache::new(ttl)?` for the defaults; clones share entries. `get`
  returns only fresh values; `insert`, `invalidate`, `clear`, `len`.
- Single flight: concurrent callers of one key wait for one load and get
  its value or its error (code, message, reason, metadata). A dropped or
  panicking leader hands the load to a waiter.
- Stale values are served only for transient errors (`UNAVAILABLE`,
  `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`, `ABORTED`) within the window,
  with a `warn!`; other errors are returned.
- For small hot sets (tokens, permissions, settings), not as a general
  cache. TTLs run on a monotonic clock (`.clock(..)`, `TokioClock` in
  paused tests).
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
- Statement and grant failures (MySQL errno 1044, 1142, 1143, 1227, 1370;
  SQLSTATE `42501`) map to `PERMISSION_DENIED` with the message
  `permission denied`; table and column names never reach it. Rejected
  connection credentials (MySQL 1045; SQLSTATE `28000`, `28P01`) are the
  service's own misconfiguration: `INTERNAL` / `DB_CREDENTIALS_REJECTED`,
  not retryable, as in `sekvent-client`.

### Database probes

```rust
let pools = PoolRegistry::build(vec![main, reports]).await?;
let runtime = pools
    .probes()                                    // one per connected pool, required exactly when its PoolSpec is
    .into_iter()
    .fold(Runtime::builder(), |b, p| b.probe(p))
    .unit("api", Stage::Ingress, UnitPolicy::Critical, server.into_unit())
    .build()?;
// by hand: PoolProbe::new("reports", pools.get("reports")?.clone()).optional()
```

- Needs `db`, `runtime` and a sqlx backend. A probe acquires a connection
  and pings it, bounded by the runtime's `probe_timeout`. An optional pool
  that is down is reported down but never makes the service
  unready; unconfigured optional pools get no probe.
- A pool whose connections are all in use reuses its previous `Up` while
  that is fresh (about 30 s) instead of queueing behind the load, so
  readiness does not flap; a closed pool is down.
- Failures are `Down(Rejected("credentials rejected"))`,
  `Down(Rejected("access denied"))` or `Down(Unreachable(..))`; never the
  URL or driver text.
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

### Browser single sign-on

```rust
use sekvent::config::{EnvSource, FromConfig, Prefixed};
use sekvent::sso::{BitbucketProvider, Sso, SsoConfig, SsoIdentity};

let sso = Sso::from_config(SsoConfig::from_config(&Prefixed::new(&EnvSource, "SSO_"))?) // SSO_APP_URL, SSO_STATE_KEY
    .provider(BitbucketProvider::from_config(&EnvSource, "SSO_BITBUCKET_")?)        // CLIENT_ID, CLIENT_SECRET, CALLBACK_URL, WORKSPACE
    .on_login(move |ctx: CallContext, identity: SsoIdentity| {
        let users = users.clone();
        async move { users.find_or_create(&ctx, &identity).await }            // -> Result<O, AppError>
    })
    .build()?;
let server = Server::builder().prefix("/api").rest(sso.router()); // GET /api/sso/bitbucket/{login,callback}

// In the app's anonymous exchange RPC (#[call(anonymous)]):
let user = sso.redeem(&request.code).ok_or_else(|| AppError::unauthenticated("invalid code"))?;
// … mint the session with JwtKeys::issue.
```

The browser navigates to `…/login?redirect=/path`, comes back to
`<APP_URL>/path#sso_code=<code>` (or `#sso_error=access_denied`, …) and
posts the code to the exchange RPC. Codes are single use, 60 s, in process
memory. Link users on `(identity.provider, identity.subject)`, never on the
email (which is set only when verified). Bitbucket admits only members of
`WORKSPACE`; the consumer needs Account: Read and Account: Email. Details:
`docs/modules/sso.md`.

### bcrypt-compatible passwords

A service sharing a password store with applications that read bcrypt
(for example `{bcrypt}$2b$10$…` from a delegating encoder) writes bcrypt
itself:

```rust
use sekvent::auth::{BcryptParams, PasswordHasher, authenticate_async};

let hasher = PasswordHasher::bcrypt(BcryptParams::new(10).prefixed())?;   // cost 4..=14; `$2b$`
// PasswordHasher::with_scheme(PasswordScheme::Bcrypt(..)) / hasher.scheme() for config-driven setups

// feature auth-tokio: hashing runs on tokio's blocking pool, not the async workers
let password = Secret::new(body.password);
let lookup = account.map(|a| (a.user_id, a.password_hash, a.enabled));   // Option<(U, String, bool)>
let ok = authenticate_async(&hasher, lookup, password.clone()).await.map_err(|rejected| {
    AppError::new(rejected.error_code(), rejected.user_message()).with_reason(rejected.reason())
})?;
if ok.needs_rehash {
    repo.set_password_hash(&ok.user, &hasher.hash_async(password).await?).await?;
}
```

- "Current" is the configured scheme: under bcrypt, a bcrypt hash in the
  configured form (prefix or not) at or above the configured cost is
  `Valid`; argon2 hashes and other bcrypt forms verify as
  `ValidNeedsRehash`, so the store converges on what every reader
  understands. The version tag (`2a`, `2b`, `2y`) never triggers a rehash;
  `.version(BcryptVersion::TwoA)` picks the tag written.
- bcrypt reads only 72 bytes (`BCRYPT_MAX_PASSWORD_BYTES`): longer
  passwords are refused by `hash` (`INVALID_ARGUMENT` /
  `PASSWORD_TOO_LONG`), so enforce the limit in the sign-up and
  change-password forms. `verify` checks a longer password on its first
  72 bytes, as the writers of existing hashes did; under argon2id that
  match asks for a rehash, under bcrypt it does not.
- Unknown accounts cost one dummy verification in the configured scheme,
  so timing does not reveal them. The default stays argon2id
  (`PasswordHasher::default()`).
- `verify_async(password, stored)` is the async form of `verify`.
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
`#[component(proto = …)]` checks (`.service_contracts(false)` turns it off).
Protos are compiled in-process with protox: no `protoc` is needed anywhere
(machine, CI, Docker), and the `google/protobuf/*` well-known types are
bundled.

### Components

A component is a trait with a protobuf contract; callers hold its generated
`XHandle` and never know whether calls stay in-process (`local`), cross a
serialization boundary (`local-serialized`: prost encode, separate task,
decode) or go to another process (`grpc`). Milestones C1 and C2: `#[call]`
methods only (no `#[async_call]`/`#[deferred]` yet). Specs:
`docs/design/component-c1.md`, `docs/design/component-c2.md`,
`docs/design/component-end-user.md` (serving to browsers and apps); full example
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
  method's request or reply is not its RPC's input or output type (compared
  by type, so a nested message never passes for a top-level one of the
  same name), or when two methods map to one RPC name. `()` is
  `google.protobuf.Empty`. Change trait and proto together.
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
  within the grace, then `on_stop` runs exactly once (only if `on_start`
  succeeded). A stop during start cancels the pending `on_start`. Under the
  runtime the drain gets half the time left before the unit's stop
  deadline, the hooks the rest. Two components with the same gRPC service
  name, or a link named `local`, fail the build.
- Per call: a cancelled or expired context is rejected before any work;
  the method timeout is capped by the caller's deadline (`DEADLINE_EXCEEDED`);
  a full bulkhead sheds at once (`RESOURCE_EXHAUSTED`/`BULKHEAD_FULL`, no
  queue). Reason constants: `sekvent::component::reasons`.
- Locally, the callee sees `cx.caller() == trusted("local")` with request
  id, subject and tenant kept. An idempotency key is forwarded only when
  set for this call (`cx.with_idempotency_key(k)`); a key a handler
  received is readable but never forwarded to its own callees. Over gRPC the caller is the
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

End users (browsers over gRPC-Web, native apps) call a served component
directly when its `SERVE_AUTH` is `bearer` (end users only) or
`link,bearer` (link tokens first, then end users). Register the App's one
end-user authenticator; it sees the request head only, before the body:

```rust
// facade features: component-grpc, runtime-grpc-web (browsers), auth-axum or auth-tonic (BearerAuth)
let auth = BearerAuth::new(keys, Validation::new().with_audience("web"), Arc::new(SystemClock));
builder.end_user_authenticator(move |request: &http::request::Parts| {
    auth.end_user::<Profile>(&request.headers)         // Profile: Deserialize + EndUserClaims (tenant, roles)
})?;                                                   // or impl EndUserAuthenticator (async) for I/O-bound checks
let server = Server::builder()
    .grpc_routes(app.grpc_routes())
    .prefix("/api")                                    // browser: POST /api/shop.orders.v1.Orders/PlaceOrder
    .cors(Cors::origins(["https://shop.example.com"])?)
    .bind(addr)
    .await?;

// in a handler: Some for an end user calling directly (then cx.caller() is None)
if let Some(user) = cx.end_user() {
    sekvent::auth::require_any_role(user, &["admin"])?;
}
```

- No authenticator with a bearer mode fails the build
  (`BuildError::EndUserAuthenticatorMissing`, naming the `SERVE_AUTH` key);
  a second one is `DuplicateEndUserAuthenticator`.
- An authenticator `Err` with `UNAUTHENTICATED` reaches the client as
  `UNAUTHENTICATED` / `invalid or expired credentials`, no reason, the same
  for every bad token; any other code passes through unchanged
  (`UNAVAILABLE` for a down session store).
- `#[call(anonymous)]` (e.g. `sign_in`) waives end-user authentication only,
  never link authentication: under `link` it still needs a link token, under
  local bindings it has no effect, on `local_only` it is a compile error. A
  token sent to an anonymous method is ignored.
- Roles stay with the authenticating component: callees down the chain see
  subject and tenant (`cx.child()` drops the end user), never roles.
- `ServerBuilder::authenticator` never rejects and component calls ignore
  it; global `.layer(..)`s wrap component calls, so a layer that demands
  its own credentials also blocks link callers and anonymous methods.

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
| `SEKVENT_COMPONENT_<C>_SERVE_AUTH` | `link` (default: needs at least one `SEKVENT_LINK_INBOUND_<CALLER>`), `bearer` (end users; needs `AppBuilder::end_user_authenticator`, no link keys), `link,bearer` (both requirements) or `none` |
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
token), `DEADLINE_EXCEEDED`/`METHOD_TIMEOUT` (the method's `TIMEOUT` ran
out while the caller still had time). The breaker trips only on
`UNAVAILABLE`, `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`, and never on the
caller's own deadline or cancellation; business errors never open it. A handler
whose own component call failed transiently answers `INTERNAL`/
`DOWNSTREAM_FAILURE` (metadata `downstream`, `downstream_code`), so callers
above neither retry nor trip on it; a panicking handler answers
`INTERNAL`/`HANDLER_PANICKED`. The server authenticates, routes (exact
`/<service>/<Rpc>` path) and checks hops from headers before reading the
body, never trusts request trailers, and caps requests at 4 MiB.

Contracts: `[contract]` in `sekvent.toml` lists the proto roots
(`roots = ["crates/billing-api/proto"]`, `baseline = "contracts"`,
`gate = true`). `cargo sekvent contract emit [service…]` writes one
canonical JSON baseline per service — commit them; `cargo sekvent contract
check [service…]` fails on wire-breaking changes (a removed RPC or
unreserved field number, a changed type, cardinality, oneof or default, a
dropped reservation or extension range, an added or removed `required`
field, a removed or changed extension) and runs in the gate. Roots must lie
inside the project and declare at least one service. Both compile the protos in-process
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
- **Secrets in paths.** The access log records every request path (never
  the query, headers or body). Tokens, reset codes and other secrets
  travel in headers or bodies, never in a path segment.
- **Global layers see component calls.** A `ServerBuilder::layer` wraps
  every REST, gRPC and gRPC-Web request, including component gRPC calls
  served through `app.grpc_routes()`. An end-user auth layer must let
  link-authenticated component paths and anonymous methods through, or go
  on each service instead; for components, use `SERVE_AUTH=bearer` and the
  App's end-user authenticator rather than a layer.
- **gRPC limits are per service.** `rest_body_limit` covers REST only; set
  `max_decoding_message_size` / `max_encoding_message_size` on each
  generated tonic server that needs more than 4 MiB.
- **Cron is UTC.** `JobSpec::cron("0 0 2 * * *")` fires at 02:00 UTC
  whatever the host's time zone; there are no other zones. Singleton
  intervals are aligned to the Unix epoch, in-process ones to the unit's
  start.
- **Fenced transactions stay short.** `check_fence_*` takes a shared lock
  on the lease row until the transaction ends; keep such transactions
  shorter than `ttl / 3` or the heartbeat stalls and the lease is lost.
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
  vouches for the end user. Open a component to browsers with
  `SERVE_AUTH=bearer` and an end-user authenticator, never with `none`, and
  mark a method `#[call(anonymous)]` only when it is truly public (sign-in
  and the like): under a bearer mode anyone may call it.
- **SSO.** Never put a token in a URL or hand-roll `returnTo`: use
  `sekvent::sso`, whose return paths are relative only and whose handoff
  code travels in the fragment. Keep `secure_cookies` on outside
  `http://localhost`, give the state key its own secret, and run one
  instance (or sticky routing) because handoff codes live in memory.
- **Time and randomness.** Inject `Clock` (`SystemClock` / `ManualClock`)
  and seedable RNGs where behaviour depends on them; JWT and login APIs take
  `now_unix_secs` explicitly.
- **Tests.** No fixed sleeps: completion signals, `RuntimeHandle::wait`,
  `tokio::time::pause`. Servers bind port 0.
- **Thin `main`.** Telemetry, config, `serve`; everything else in the
  library so the coverage gate can see it.
