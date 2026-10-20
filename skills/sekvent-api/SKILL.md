---
name: sekvent-api
description: How to use the sekvent Rust backend framework — the entry point for questions like "how do I do X with sekvent", "which feature or module do I need", "where is this documented", "how is a sekvent service put together" or "how do I bootstrap a sekvent project". Gives the end-to-end flow (install the CLI, scaffold, run, grow, test, gate), the anatomy of a service, the feature picker, a map from task to the documentation page that answers it, and where to find those docs from inside any project (repository, vendored submodule or cargo git checkout). Use it to orient and to answer usage questions; for copy-ready recipes while writing code use `sekvent`, to create a workspace use `sekvent-new-project`, to move an existing backend use `sekvent-migrate`.
---

# Using sekvent

sekvent is a Rust backend framework. A service depends on one facade
package, `sekvent-api`, whose library is named `sekvent` (`use sekvent::…`),
and turns modules on with Cargo features. The `cargo sekvent` CLI scaffolds
workspaces and runs the quality gate.

This skill orients you and points at the documentation. The documentation is
the source of truth; read the matching page before writing non-trivial code.

## 1. Find the documentation

The user guide is the `docs/` folder of the sekvent repository. Look for it
in this order and use the first that exists:

| Situation | Where `docs/` is |
|---|---|
| You are in the sekvent repository | `./docs/` |
| The project vendors sekvent as a submodule | `vendor/sekvent-api/docs/` (or wherever `path = ".../crates/sekvent"` points, two levels up) |
| The project uses git dependencies | `~/.cargo/git/checkouts/sekvent-api-*/<short-rev>/docs/`, where `<short-rev>` is the start of the revision in `Cargo.lock` (`source = "git+…#<rev>"` of `sekvent-api`) — this matches the code you compile against |
| None of the above | https://github.com/westito/sekvent-api/tree/master/docs (current `master`, may be newer than the project's lock) |

Prefer the copy that matches the project's `Cargo.lock`; the online `master`
can describe APIs the project does not have yet.

## 2. Which page answers which question

| You want to… | Read |
|---|---|
| Start from nothing, see the whole flow | `docs/getting-started.md` |
| Pick Cargo features, know what the prelude brings | `docs/features.md` |
| Know a CLI command, flag, env var or `sekvent.toml` key | `docs/cli.md` |
| Add config, a secret, a reserved `SEKVENT_*` key | `docs/modules/config.md` |
| Return errors, map them to HTTP/gRPC, know what gets logged | `docs/modules/error.md` |
| Use deadlines, cancellation, idempotency keys, an injected clock | `docs/modules/context.md` |
| Set up logging, JSON logs, the access log, request ids | `docs/modules/telemetry.md` |
| Structure `main.rs`, stages, units, shutdown, health, probes | `docs/modules/runtime.md` |
| Serve gRPC, gRPC-Web and REST; CORS; body limits; layers; downloads | `docs/modules/server.md` |
| Serve a bundled SPA/frontend (Vite `dist`) from the binary next to the API; the dev proxy setup | `docs/modules/server.md` ("Serve a bundled frontend") |
| Run interval, cron, manual or singleton jobs | `docs/modules/jobs.md` |
| Pools, migrations, DB error mapping, list filters, probes, leases | `docs/modules/db.md` |
| Queries with sea-query (no ORM), embedded `sqlx::migrate!` migrations, mapping a named constraint violation | `docs/modules/db.md` ("Build queries with sea-query", "Embedded migrations") |
| Passwords (argon2id, bcrypt), login, JWT, roles | `docs/modules/auth.md` |
| Browser single sign-on (OAuth 2.0 code flow, Bitbucket Cloud) | `docs/modules/sso.md` |
| Browser uploads/downloads via S3-compatible storage (presigned URLs), keys, MinIO tests | `docs/modules/storage.md` |
| Service-to-service tokens | `docs/modules/link.md` |
| Call an upstream HTTP API (retries, OAuth 2.0, context propagation) | `docs/modules/client.md` |
| Retry, breaker, bulkhead, rate gate, timeout, TTL cache | `docs/modules/resilience.md` |
| Components (trait + proto contract, App builder, bindings, split topology) | `docs/modules/components.md`, concepts in `docs/component-model.md` |
| Stream live updates to a browser or service (server-streaming `#[call(stream)]`, `CallStream`, lifetime, drain, HTTP/2 keep-alive) | `docs/modules/components.md` ("How to stream replies"), `docs/modules/server.md` ("Keep long streams alive") |
| `build.rs` protobuf codegen without `protoc` | `docs/modules/proto-build.md` |
| Container-backed DB and MinIO tests, deterministic tests, coverage | `docs/modules/testing.md` |
| Why something is designed the way it is | `docs/design/*.md` (decisions; the module pages describe current behaviour) |
| A complete multi-component example | `examples/shop/` |

Every module page has the same shape: *Enable it* (feature + import path),
*Quick example*, *Concepts*, *How to …* sections, configuration keys, error
codes and reasons, testing tips, pitfalls. Jump to the section you need.

## 3. Bootstrap a project

```sh
# 1. CLI (prebuilt, checksum-verified; falls back to a source build)
curl -fsSL https://raw.githubusercontent.com/westito/sekvent-api/master/scripts/install.sh | sh
cargo sekvent --version

# 2. Workspace with its first service: http (default), grpc or worker
cargo sekvent new orders --kind http
cd orders
cargo generate-lockfile          # pins the sekvent revision; commit Cargo.lock

# 3. Run it (config comes from the process environment only)
cp .env.example .env && set -a && . ./.env && set +a
cargo run -p orders
curl -s http://127.0.0.1:8080/readyz
curl -s http://127.0.0.1:8080/api/ping

# 4. Quality gate. Runs locally, or through the remote builder named by
#    BUILD_CMD when that is set (SEKVENT_LOCAL=1 forces local)
cargo fmt --all
cargo sekvent gate               # fmt --check, clippy -D warnings, tests
```

Then: `cargo sekvent add <name> --kind grpc|http|worker` for more services,
`cargo sekvent ci generate github|bitbucket` for CI,
`cargo sekvent agents` for the `AGENTS.md` section and
`cargo sekvent skills install` for these skills. An existing workspace gets
the sekvent files with `cargo sekvent init` (it does not edit `Cargo.toml`;
see the `sekvent-migrate` skill). Details: `docs/getting-started.md`,
`docs/cli.md`.

## 4. Anatomy of a service

```text
main.rs   telemetry::init → Config::from_env (EnvConfig) → lib::serve(config)
lib.rs    runtime(&config) builds a RuntimeBuilder:
            Stage::Infrastructure  pools, caches, dependency probes
            Stage::Components      the component App (App::register)
            Stage::Workers         jobs, consumers
            Stage::Ingress         Server (gRPC + gRPC-Web + REST on one listener)
          serve() adds signal handling and runs it
handlers  take CallContext (Ctx extractor), return Result<_, AppError>,
          use pools / HttpClient / component handles, read time from a Clock
tests     MapSource configs, paused tokio time, ManualClock, 127.0.0.1:0,
          sekvent-testing containers for databases
```

- Stages start in that order and stop in reverse on SIGTERM within one stop
  deadline; health turns not-serving first, then the server drains.
- `/livez`, `/readyz`, `/healthz` and `grpc.health.v1` are served at the
  root, outside your layers and authenticator. Required dependency probes
  (for example `PoolRegistry::probes()`) gate `/readyz`.
- Keep `main.rs` thin; `runtime(&config)` returning the builder lets tests
  bind port 0 and drive the service in-process.

The generated `crates/<name>/src/lib.rs` is the reference implementation of
this shape; read it before restructuring.

## 5. Pick features

`default = ["config", "error", "context", "telemetry", "runtime"]`. Add:

| Need | Feature(s) |
|---|---|
| gRPC-Web next to gRPC (browser clients) | `runtime-grpc-web` (without it `grpc_web(true)` does nothing) |
| Cron schedules | `runtime-cron` |
| Serve a built frontend (SPA) next to the API | `runtime-vfs` (`StaticSite`, `Directory`); `runtime-vfs-embed` to compile `web/dist` into the binary (`Embedded`, `rust-embed` re-exported) |
| Postgres / MySQL with sqlx | `db-sqlx-postgres` / `db-sqlx-mysql` (+ `db-migrate`) |
| sea-orm | `db-sea-orm-postgres` / `db-sea-orm-mysql` (+ `db-sea-orm-migrate`) |
| sea-query over sqlx, no ORM | `db-sea-query-postgres` / `db-sea-query-mysql` (+ `db-migrate`) |
| Singleton jobs, leases with fencing | `db-lease` (with `runtime`: `LeaseGuard`) |
| Passwords, JWT | `auth` (+ `auth-tokio` for hashing off the async threads, `auth-axum` / `auth-tonic` adapters) |
| Browser single sign-on | `sso` |
| Outbound HTTP | `client` |
| File uploads/downloads (S3, MinIO, presigned URLs) | `storage` |
| Retry, breaker, TTL cache | `resilience` |
| Service-to-service tokens | `link` (+ `link-axum` / `link-tonic`) |
| Components | `component` (+ `component-grpc` for the gRPC binding, also for serving streams; `runtime-grpc-web` for browsers) |

A browser frontend: in development run Vite's dev server with
`server.proxy` sending `/api` to the backend (one origin, no CORS); in
production embed the `vite build` output with `runtime-vfs-embed` behind a
feature of the service's own (`embed-ui`, so `cargo check` works without a
built frontend; the feature itself needs `web/dist` to exist) and pass
`StaticSite` to `ServerBuilder::site`. The API must then live under a
prefix (`.prefix("/api")`); browser navigations to unknown paths get
`index.html`, missing files below `assets/` a `404`. Auth layers go on the
API router, not `ServerBuilder::layer`, which also wraps the site.

`sekvent-testing` is a dev-dependency and `sekvent-proto-build` a
build-dependency; neither is re-exported. The dependency key must be
`sekvent-api` (the proc macros find the facade by that key). Full matrix:
`docs/features.md`.

## 6. The handful of patterns you will use most

Names below exist in the code; the linked pages have the complete versions.

```rust
use sekvent::prelude::*; // AppError, ErrorCode, CallContext, Secret, EnvConfig, FromConfig,
                         // Runtime, RuntimeBuilder, Server, Stage, UnitPolicy, JobSpec, JobContext, Ctx, …

// Config (docs/modules/config.md): one key per field, UPPER_SNAKE_CASE by default.
#[derive(Debug, EnvConfig)]
pub struct Config {
    #[config(default = "0.0.0.0:8080")]
    pub listen_addr: std::net::SocketAddr,
    pub billing_url: String,
    pub billing_token: Secret,               // required, never printed
}

// Errors (docs/modules/error.md): caller-safe message, detail in the source.
fn check(id: i64) -> Result<(), AppError> {
    if id <= 0 {
        return Err(AppError::invalid_argument("the id must be positive")
            .with_field_violation("id", "must be greater than zero"));
    }
    Ok(())
}

// Server + runtime (docs/modules/server.md, runtime.md).
let server = Server::builder().prefix("/api").rest(routes()).bind(config.listen_addr).await?;
let builder = Runtime::builder()
    .unit("http", Stage::Ingress, UnitPolicy::Critical, server.into_unit());

// Job (docs/modules/jobs.md): never take the process down because one run failed.
let builder = builder.job("sweep", Stage::Workers, JobSpec::interval(every),
    move |job: JobContext| async move { if job.is_cancelled() { return Ok(()); } Ok(()) });

// Database (docs/modules/db.md): pools from <PREFIX>URL etc., probes gate readiness.
let pools = sekvent::db::PoolRegistry::build(vec![
    sekvent::db::PoolSpec::from_config(&sekvent::config::EnvSource, "ORDERS_DB_").map_err(AppError::internal)?,
]).await?;
pools.migrate_all().await?;

// Outbound HTTP (docs/modules/client.md): propagates request id and deadline.
let billing = sekvent::client::HttpClient::builder()
    .base_url(config.billing_url.clone())
    .bearer_token(config.billing_token.clone())
    .build()
    .map_err(AppError::internal)?;
let invoice: Invoice = billing.get(&format!("/invoices/{id}")).send_json(&cx).await?; // validate `id` first
```

## 7. Rules sekvent expects you to follow

- **Fail closed.** Missing config, tokens or bindings are startup errors
  naming the key, never the value. Don't add silent fallbacks.
- **Secrets.** Use `Secret`; never log tokens, passwords, database URLs or
  upstream bodies. `truncate_for_log` shortens text, it does not redact.
  Error *sources* of INTERNAL/UNKNOWN/DATA_LOSS are logged verbatim at the
  serving boundary, so keep secrets out of them too.
- **Time and randomness are injected.** Read time from a `Clock`
  (`SystemClock` in production, `ManualClock` in tests).
- **Deadlines.** Handlers get a `CallContext`; pass it to outbound calls and
  components. Only retry idempotent operations.
- **Tests are deterministic.** `tokio::time::pause`/`start_paused`, manual
  clocks, completion signals — no fixed sleeps. Sockets bind `127.0.0.1:0`;
  an address nobody listens on is `127.0.0.1:1`. Container tests are
  `#[ignore]`d and run when `[harness] docker_tests = true`.
- **Validate before building URLs or queries** from caller input
  (`HttpClient` only checks the origin, not the path).

## 8. Where commands run

Compiling commands (`gate`, `check`, `clippy`, `test`, `coverage`,
`harness-clean`) are forwarded to a remote builder through `rrb` when the
`BUILD_CMD` environment variable names one (`[remote] mode = "auto"`, the
default) and run locally otherwise; they always run locally in CI, with
`SEKVENT_LOCAL=1` or with `mode = "local"`, and `mode = "rrb"` always
forwards. Once forwarding is selected, a missing `rrb` is an error, never a
silent local build. `cargo fmt`, scaffolding, `deps sync`,
`sdk update` and `contract emit` always run on the current machine because
they write files. See `docs/cli.md#where-commands-run`.

## 9. Keeping sekvent current

```sh
cargo sekvent sdk status --remote   # locked revision vs branch head
cargo sekvent sdk update            # move the sekvent crates in Cargo.lock
cargo sekvent self-update           # newer CLI (and its third-party pins)
cargo sekvent deps check            # compare your pins with sekvent's
cargo sekvent deps sync             # adopt them
```

After an update, re-read the module pages for anything you use: the docs
change in the same commit as the code.

## Related skills

| Skill | Use it for |
|---|---|
| `sekvent` | Writing code in a sekvent project: crate map, copy-ready recipes, pitfalls |
| `sekvent-new-project` | `cargo sekvent new` / `add` step by step |
| `sekvent-migrate` | Moving an existing backend onto sekvent |
