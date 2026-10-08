# Getting started

This guide takes you from an empty directory to a running sekvent service
with a green gate, then shows how to grow it: configuration, errors, a
database, a background job and an outbound HTTP client. Every command and
flag here is described in full in the [CLI reference](cli.md).

## 1. Prerequisites

| Tool | Why |
|---|---|
| [rustup](https://rustup.rs) | Generated projects pin their compiler in `rust-toolchain.toml` (the same channel sekvent itself is built with, edition 2024, with `rustfmt`, `clippy` and `llvm-tools-preview`); rustup installs it on first use |
| `git` | `cargo sekvent new` runs `git init`, and sekvent is a git dependency |
| Docker | Only for the container-backed database tests (`sekvent-testing`) and the release image |
| `cargo-llvm-cov` | Only if you run `cargo sekvent coverage` on your own machine (`cargo install --locked cargo-llvm-cov`) |

You do **not** need `protoc`: protobuf files are compiled in-process (protox)
by `sekvent-proto-build` and by `cargo sekvent contract`.

### Install the CLI

The install script downloads the prebuilt `cargo-sekvent` of the rolling
`cli-latest` release for your platform (Linux glibc and macOS, x86_64 and
aarch64), checks its SHA-256 and installs it into
`${SEKVENT_INSTALL_DIR:-${CARGO_HOME:-~/.cargo}/bin}`. On other platforms, or
when the download fails, it builds the same tag from source with
`cargo install`. A checksum mismatch is always an error.

```sh
curl -fsSL https://raw.githubusercontent.com/westito/sekvent-api/master/scripts/install.sh | sh
cargo sekvent --version
```

To build from source yourself:

```sh
cargo install --locked --git https://github.com/westito/sekvent-api --tag cli-latest cargo-sekvent
```

To update an installed CLI to the latest release build later:

```sh
cargo sekvent self-update
```

The copy on your machine only needs to be recent enough to generate files.
Inside a remote builder and in CI, the project's `.sekvent/run.sh` installs
the CLI at exactly the sekvent revision your `Cargo.lock` pins (see
[Where commands run](cli.md#where-commands-run)).

## 2. Create a workspace

```sh
cargo sekvent new orders --kind grpc     # or --kind http (the default), --kind worker
```

| Flag | Default | Meaning |
|---|---|---|
| `NAME` (positional) | — | Project name and the first service's crate name: 1–64 characters, lowercase letters, digits and `-`, starting with a letter, no trailing `-` and no `--` |
| `--kind grpc\|http\|worker` | `http` | Flavour of the first service |
| `--dir PATH` | `./NAME` | Target directory; must be missing or empty |
| `--ci github\|bitbucket\|none` | `github` | CI template to add |
| `--sekvent-path PATH` | — | Depend on a local sekvent checkout by path instead of git (see [dependency modes](#dependency-modes)) |
| `--no-git` | off | Skip `git init` |

Pick the kind by what the service does:

| Kind | You get | Use it for |
|---|---|---|
| `grpc` | `crates/NAME` (a tonic service on the sekvent server, gRPC-Web included), `crates/NAME-proto` (portable messages, no tonic) and `proto/NAME/v1/NAME.proto` | Services other services or a typed frontend call |
| `http` | `crates/NAME` with axum routes under `/api` | REST/JSON APIs, webhooks, browser backends |
| `worker` | `crates/NAME` with one runtime unit, a backoff loop and no listener | Pollers, queue consumers, sync jobs |

The examples below use the name `orders`, which gives the proto package
`orders.v1` and the gRPC service `OrdersService`. A name with dashes such as
`orders-api` gives the crate identifier `orders_api`, the proto package
`orders_api.v1` and the service `OrdersApiService`.

### What was generated

```text
orders/
├── Cargo.toml                      workspace manifest and pinned dependencies
├── sekvent.toml                    cargo sekvent project file
├── rust-toolchain.toml  rustfmt.toml
├── .env.example                    example environment
├── .gitignore  .dockerignore
├── .remote-build.toml              remote builder configuration
├── .sekvent/run.sh                 CLI bootstrap for the builder and CI
├── .github/workflows/gate.yml      --ci github (bitbucket-pipelines.yml with --ci bitbucket)
├── AGENTS.md  README.md  Dockerfile
├── crates/
│   ├── orders/                     the service
│   │   ├── AGENTS.md  Cargo.toml  build.rs (grpc only)
│   │   ├── src/main.rs  src/lib.rs
│   │   └── tests/ping.rs           tests/api.rs (http), tests/worker.rs (worker)
│   └── orders-proto/               grpc only: generated messages
│       └── AGENTS.md  Cargo.toml  build.rs  src/lib.rs
└── proto/orders/v1/orders.proto    grpc only
```

**Workspace files**

- `Cargo.toml` — the workspace (`members = ["crates/*"]`, resolver 3) and
  `[workspace.package]` (edition, `rust-version`, `publish = false`).
  `[workspace.dependencies]` holds the three sekvent crates — `sekvent-api`
  (the facade; code writes `use sekvent::…`), `sekvent-testing` and
  `sekvent-proto-build` — as git dependencies on `master`, followed by every
  third-party version sekvent pins and is tested against. Crates refer to
  them with `{ workspace = true }`. It also sets the lint policy
  (`unsafe_code = "deny"`, `missing_docs`, `unreachable_pub`, clippy `all`
  and `pedantic` as warnings), `opt-level = 3` for the password-hashing
  crates in dev builds (they dominate test time otherwise) and a thin-LTO,
  stripped release profile.
- `sekvent.toml` — the CLI's project file: where compiling commands run,
  what the gate checks, coverage floors (95 % lines per package, 0 for the
  `-proto` crate), test-container labels, hooks, contracts and custom tasks.
  Unknown keys are rejected. See the [`sekvent.toml`
  reference](cli.md#sekventtoml-reference).
- `rust-toolchain.toml` — the single source of the Rust version. The
  `Dockerfile`'s `RUST_VERSION`, the CI toolchain, `rust-version` in
  `Cargo.toml` and the `[toolchain]` of `.remote-build.toml` must agree with
  it; change them in the same commit.
- `rustfmt.toml` — edition and `max_width = 100`.
- `.env.example` — example environment keys: `SEKVENT_LOG`,
  `SEKVENT_LOG_FORMAT`, `LISTEN_ADDR`, `SHUTDOWN_DELAY`, and commented-out
  link tokens and `SEKVENT_DOCKER_TESTS`. It is the same for every kind: a
  `worker` reads neither `LISTEN_ADDR` nor `SHUTDOWN_DELAY` (they are
  ignored) and takes its own keys, listed below, from their defaults. Copy it
  to `.env` (git-ignored) for local runs.
- `.remote-build.toml` and `.sekvent/run.sh` — configuration of the `rrb`
  remote builder and the script it (and CI) runs. `run.sh` reads the sekvent
  revision from `Cargo.lock`, installs `cargo-sekvent` at that revision into
  `$CARGO_HOME/sekvent-cli/<rev>` once, and runs it. You can ignore both if
  you build locally.
- `Dockerfile` — a cargo-chef multi-stage release build for one binary
  (`--build-arg BIN=<crate>`), running as a non-root user on
  `debian:bookworm-slim`. It builds only; pushing and deploying are yours.
- `AGENTS.md` — instructions for people and coding agents. The block between
  `<!-- sekvent:begin -->` and `<!-- sekvent:end -->` is managed by
  `cargo sekvent agents`; each crate gets its own short `AGENTS.md`.
- `README.md` — a skeleton for your project's readme.
- CI file — a gate-only pipeline (format check, clippy, tests, coverage)
  through `.sekvent/run.sh` with `CI=true`; see [CI](#ci).

**The service crate (`crates/orders`)**

- `src/main.rs` — deliberately thin: initialise telemetry
  (`sekvent::telemetry::init`), read `Config` with
  `sekvent::config::from_env()` (a bad or missing key stops the process and
  names the key, never the value), call `serve`, map the result to an exit
  code.
- `src/lib.rs` — everything else, so tests can reach it:
  - `Config`, a `#[derive(EnvConfig)]` struct. For `grpc` and `http`:
    `LISTEN_ADDR` (default `0.0.0.0:8080`) and `SHUTDOWN_DELAY` (default
    `0s`). For `worker`: `IDLE_INTERVAL` (`5s`), `RETRY_INITIAL` (`1s`) and
    `RETRY_MAX` (`1m`).
  - `runtime(..)` assembles a `RuntimeBuilder` without starting it (and, for
    `grpc`/`http`, binds the listener and returns its address), so tests can
    bind port 0.
  - `serve(..)` runs it until SIGINT/SIGTERM or the given future resolves,
    then drains gracefully.
  - The example API: `grpc` implements `OrdersService::ping` (echo, rejects an
    empty or over-long message with `INVALID_ARGUMENT`); `http` serves
    `GET /api/ping` and `GET /api/greetings/{name}`; `worker` defines a `Job`
    trait, an `Outcome` (`Worked`/`Idle`) and a placeholder `Heartbeat` job,
    run in a loop with exponential backoff after failures.
- `tests/` — an integration test that starts the service through the runtime
  handle on port 0 (`grpc`: `tests/ping.rs`, `http`: `tests/api.rs`) or on a
  paused clock (`worker`: `tests/worker.rs`). No fixed ports, no sleeps.
- `Cargo.toml` — the facade with only the features this kind needs:
  `error-grpc` + `runtime-grpc-web` (grpc), `error-http` (http),
  `resilience` (worker); the default features (`config`, `error`, `context`,
  `telemetry`, `runtime`) are always on. See the [feature
  matrix](features.md).
- `build.rs` (grpc only) — generates the tonic server and client stubs with
  `sekvent_proto_build::ProtoBuild::new("../../proto")…services_only("::orders_proto")`,
  so the stubs refer to the messages of `orders-proto`, and writes a
  `package_alias.rs` that re-exports the package module as `proto::api`.

**The proto crate (`crates/orders-proto`, grpc only)**

- `build.rs` compiles `proto/orders/v1/orders.proto` with
  `.messages_only()`; `src/lib.rs` includes the generated
  `sekvent_protos.rs` and the `api` alias. It depends on `prost` only — no
  tonic — so client libraries and wasm builds can use the messages.
- `proto/orders/v1/orders.proto` is the public contract (`package
  orders.v1; service OrdersService { rpc Ping(PingRequest) returns
  (PingReply); }`). Change it only compatibly.

See [proto-build](modules/proto-build.md) for the codegen options.

## 3. First run

### Lock the dependencies

```sh
cd orders
cargo generate-lockfile
```

This resolves sekvent's current `master` revision and records it in
`Cargo.lock`. Commit `Cargo.lock`: every gate command passes `--locked`, and
`.sekvent/run.sh` reads the sekvent revision from it.

### Run the service

The service reads its configuration from the process environment only (it
does not load `.env` files itself):

```sh
cp .env.example .env
set -a; . ./.env; set +a
cargo run -p orders
```

Keys under `SEKVENT_` are reserved for the framework: an unknown
`SEKVENT_*` key is a startup error, so a typo cannot silently fall back to a
default. `SEKVENT_LOG` takes tracing filter directives (falling back to
`RUST_LOG`, then `info`) and `SEKVENT_LOG_FORMAT` is `compact`, `pretty` or
`json`. See [config](modules/config.md) and [telemetry](modules/telemetry.md).

gRPC and HTTP services serve everything on one listener (`LISTEN_ADDR`,
`127.0.0.1:8080` in `.env.example`):

| Path | Meaning |
|---|---|
| `/livez` | Liveness: the process is up |
| `/readyz` | Readiness: `200` once the runtime has started and required dependency probes pass, `503` otherwise (including from the moment shutdown begins) |
| `/healthz` | Same as `/readyz` |
| `grpc.health.v1.Health` | The standard gRPC health service, on by default for both kinds (`Server::builder().grpc_health(false)` turns it off) |

A worker has no listener and therefore no health endpoints.

### Call it

HTTP:

```sh
curl -s http://127.0.0.1:8080/readyz
curl -s -H 'x-request-id: demo-1' http://127.0.0.1:8080/api/ping
curl -s http://127.0.0.1:8080/api/greetings/ada
```

Errors render as `{"error": {...}}` with the matching HTTP status, for
example `400` with `"code": "INVALID_ARGUMENT"` for a name over 64 bytes.

gRPC: the server does not expose reflection, so give your client the
`.proto`. With [grpcurl](https://github.com/fullstorydev/grpcurl):

```sh
grpcurl -plaintext -import-path proto -proto orders/v1/orders.proto \
  -d '{"message": "hello"}' 127.0.0.1:8080 orders.v1.OrdersService/Ping
```

The same listener also answers gRPC-Web, and `curl http://127.0.0.1:8080/readyz`
works for a gRPC service too.

Press Ctrl-C to stop: health turns not-serving, the runtime waits
`SHUTDOWN_DELAY`, drains in-flight requests and exits.

### Run the tests and the gate

`cargo sekvent` forwards everything that compiles (`gate`, `check`,
`clippy`, `test`, `coverage`, `harness-clean`) to a remote builder through
`rrb`, unless the current machine is the place to compile. If you do not use
a remote builder, opt out once in `sekvent.toml`:

```toml
[remote]
mode = "local"
```

or for a single shell:

```sh
export SEKVENT_LOCAL=1
```

CI (`CI` set) always runs locally. A missing `rrb` is an error that says
exactly this; it never turns into a silent local build.

```sh
cargo fmt --all            # formatting writes files, so it always runs here
cargo sekvent test         # the test suite; extra arguments after --
cargo sekvent gate         # fmt --check, clippy -D warnings, tests (+ doc, boundaries, contracts when configured)
cargo sekvent coverage     # instrumented tests and per-package line floors
```

The gate stops at the first failing step and exits with its code. Read
[`cargo sekvent gate`](cli.md#cargo-sekvent-gate) for the exact steps.

Database tests that start containers are `#[ignore]`d by default. To run
them in the gate and coverage, set `docker_tests = true` under `[harness]` in
`sekvent.toml` (Docker must be available wherever the tests run); see
[testing](modules/testing.md).

## 4. Grow the service

The snippets below extend the `http` service. Each needs a facade feature.
In `crates/orders/Cargo.toml`, extend the existing `sekvent-api` line under
`[dependencies]` and add `sqlx` (`axum`, `serde` and `tokio` are already
there):

```toml
[dependencies]
sekvent-api = { workspace = true, features = ["error-http", "client", "db-sqlx-postgres", "db-migrate"] }
sqlx = { workspace = true, features = ["postgres"] }
```

`use sekvent::prelude::*;` brings `AppError`, `ErrorCode`, `CallContext`,
`Secret`, `EnvConfig`, `FromConfig`, `Runtime`, `RuntimeBuilder`, `Server`,
`Stage`, `UnitPolicy`, `JobSpec`, `JobContext` and the `Ctx` extractor.

### Configuration

Add fields to `Config`. Each field reads one key, by default the field name
in `UPPER_SNAKE_CASE`; `Secret` fields are required and never printed.

```rust
use std::net::SocketAddr;
use std::time::Duration;

use sekvent::prelude::*;

/// Service configuration, read from the environment.
#[derive(Debug, EnvConfig)]
pub struct Config {
    /// Address for the API and the health endpoints.
    #[config(default = "0.0.0.0:8080")]
    pub listen_addr: SocketAddr,
    /// Pause before draining on shutdown.
    #[config(default = "0s")]
    pub shutdown_delay: Duration,
    /// Base URL of the billing API.
    pub billing_url: String,
    /// Bearer token for the billing API.
    pub billing_token: Secret,
    /// How often expired carts are removed.
    #[config(default = "5m")]
    pub cart_sweep_every: Duration,
}
```

All missing or malformed keys are reported together, by name. In tests,
build the struct from a map instead of touching the process environment:

```rust
use sekvent::config::MapSource;

let config = Config::from_config(
    &MapSource::new()
        .with("LISTEN_ADDR", "127.0.0.1:0")
        .with("BILLING_URL", "http://127.0.0.1:1")
        .with("BILLING_TOKEN", "test-token"),
)
.unwrap();
```

More attributes (`key`, `prefix`, `nested`, `validate`, `secret`) are in
[config](modules/config.md).

### Errors

Handlers return `AppError`: a code, a message that is safe to show the
caller, and optional reason, metadata and field violations. Internal detail
goes into the source, which is logged but never sent.

```rust
use std::io::ErrorKind;

use axum::extract::Path;
use sekvent::prelude::*;

async fn receipt(Path(id): Path<i64>) -> Result<String, AppError> {
    if id <= 0 {
        return Err(AppError::invalid_argument("the id must be positive")
            .with_field_violation("id", "must be greater than zero"));
    }
    tokio::fs::read_to_string(format!("receipts/{id}.txt"))
        .await
        .map_err(|error| match error.kind() {
            ErrorKind::NotFound => {
                AppError::not_found("no such receipt").with_reason("RECEIPT_NOT_FOUND")
            }
            // Any std error can be the source; the caller only sees "internal error".
            _ => AppError::internal(error),
        })
}
```

With `error-http` an `AppError` is an axum response; with `error-grpc`, `?`
turns it into a `tonic::Status`. See [error](modules/error.md).

### A Postgres pool with migrations

Pools are described by environment keys under a prefix and built once at
startup. With the prefix `ORDERS_DB_`, the pool is named `orders_db` and reads
`ORDERS_DB_URL` (required), `ORDERS_DB_MAX_CONNECTIONS`,
`ORDERS_DB_MIGRATIONS` and more (see [db](modules/db.md)).

```rust
use sekvent::config::ConfigSource;
use sekvent::db::{PoolRegistry, PoolSpec};

/// Connect the database and apply pending migrations.
pub async fn connect(source: &dyn ConfigSource) -> Result<PoolRegistry, AppError> {
    let spec = PoolSpec::from_config(source, "ORDERS_DB_").map_err(AppError::internal)?;
    let pools = PoolRegistry::build(vec![spec]).await?;
    pools.migrate_all().await?; // runs ORDERS_DB_MIGRATIONS (a sqlx migrations directory), if set
    Ok(pools)
}
```

`PoolRegistry::build` fails at startup, naming the pool, when a required URL
is missing or an eager pool cannot connect; `DbError` converts into
`AppError` with `?`. The migrations directory is read at runtime, relative to
the working directory, so ship it with your image. Call `connect` from
`main.rs` with `&sekvent::config::EnvSource` and pass the registry into
`runtime`, where you take the pool, add readiness probes and mount routes
with state:

```rust
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use sekvent::db::IntoAppError;

#[derive(Clone)]
struct AppState {
    db: sqlx::PgPool,
}

async fn order_status(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<String>, AppError> {
    let row: Option<(String,)> = sqlx::query_as("SELECT status FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .into_app_error()?; // maps sqlx errors to AppError codes without leaking SQL
    let (status,) = row.ok_or_else(|| AppError::not_found("no such order"))?;
    Ok(Json(status))
}

pub async fn runtime(
    config: &Config,
    pools: &PoolRegistry,
) -> Result<(RuntimeBuilder, SocketAddr), AppError> {
    let db = pools
        .get("orders_db")?
        .postgres()
        .cloned()
        .ok_or_else(|| AppError::internal("orders_db is not a Postgres pool"))?;
    let routes = Router::new()
        .route("/orders/{id}/status", get(order_status))
        .with_state(AppState { db: db.clone() });
    let server = Server::builder()
        .prefix("/api")
        .rest(routes)
        .bind(config.listen_addr)
        .await?;
    let addr = server.local_addr();
    let mut builder = Runtime::builder()
        .shutdown_delay(config.shutdown_delay)
        .unit("http", Stage::Ingress, UnitPolicy::Critical, server.into_unit());
    for probe in pools.probes() {
        builder = builder.probe(probe); // /readyz fails while a required pool is unreachable
    }
    Ok((builder, addr))
}
```

`main.rs` now connects first and passes the registry on; update `serve` and
the integration test the same way. Tests get a throwaway Postgres from
`sekvent-testing` (add it under `[dev-dependencies]`); see
[testing](modules/testing.md).

### A periodic job

Jobs run as runtime units: they start with their stage, stop on shutdown and
never take the process down because one run failed. Register one in
`runtime`, next to the probes, reusing the pool taken there:

```rust
use std::time::Duration;

let sweep_db = db.clone();
builder = builder.job(
    "sweep-carts",
    Stage::Workers,
    JobSpec::interval(config.cart_sweep_every)
        .jitter(Duration::from_secs(10))
        .timeout(Duration::from_secs(60)),
    move |job: JobContext| {
        let db = sweep_db.clone();
        async move {
            if job.is_cancelled() {
                return Ok(());
            }
            sqlx::query("DELETE FROM carts WHERE expires_at < now()")
                .execute(&db)
                .await
                .into_app_error()?;
            Ok(())
        }
    },
);
```

Cron schedules (`JobSpec::cron`, UTC) need the `runtime-cron` feature;
`JobSpec::manual()` runs only when triggered through its handle. To run a job
on one instance at a time across replicas, guard it with a database lease
(`db-lease`). See [jobs](modules/jobs.md).

### An outbound HTTP client

`HttpClient` wraps reqwest (rustls) with a timeout and retry policy,
propagates the request id and remaining deadline from the `CallContext`, and
maps upstream failures to `AppError` without leaking response bodies. Build
it once in `runtime` and keep it in the router state (it is cheap to clone):

```rust
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use sekvent::client::HttpClient;
use sekvent::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
struct AppState {
    db: sqlx::PgPool,
    billing: HttpClient,
}

#[derive(Debug, Deserialize, Serialize)]
struct Invoice {
    id: String,
    total_cents: i64,
}

fn billing_client(config: &Config) -> Result<HttpClient, AppError> {
    HttpClient::builder()
        .base_url(config.billing_url.clone())
        .bearer_token(config.billing_token.clone())
        .request_timeout(Duration::from_secs(5))
        .build()
        .map_err(AppError::internal)
}

async fn invoice(
    State(state): State<AppState>,
    Ctx(cx): Ctx,
    Path(id): Path<String>,
) -> Result<Json<Invoice>, AppError> {
    let well_formed = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !well_formed {
        return Err(AppError::invalid_argument("malformed invoice id")
            .with_field_violation("id", "letters, digits and '-' only, at most 64"));
    }
    let invoice = state
        .billing
        .get(&format!("/invoices/{id}"))
        .send_json(&cx)
        .await?;
    Ok(Json(invoice))
}
```

`Ctx` extracts the `CallContext` the server attached to the request, so the
outbound call carries the caller's request id and deadline.

Check every caller-supplied value before it becomes part of an upstream path.
The client only keeps the request on the base URL's host; an id such as
`..%2Fadmin`, which axum decodes to `../admin`, would otherwise reach a
different endpoint of the billing API with your bearer token attached.

See [client](modules/client.md) for policies, OAuth 2.0 client credentials and
calls between sekvent services, and [resilience](modules/resilience.md) for
the building blocks.

## 5. Beyond the first service

### Add a service

```sh
cargo sekvent add billing --kind http     # --kind is required: grpc, http or worker
```

`add` renders the service templates into the workspace that holds
`sekvent.toml` (it uses `[project].name` from there), adds the new crate
directories to `[workspace].members` unless a pattern such as `crates/*`
already covers them, and refuses to touch an existing crate directory
without `--force`. For a `grpc` service it also writes
`proto/billing/v1/billing.proto` and `crates/billing-proto`; add
`"billing-proto" = 0.0` under `[coverage.thresholds]` if you want the same
floor the first proto crate has.

### Put an existing workspace on sekvent

From anywhere inside a Cargo workspace (a `Cargo.toml` with `[workspace]`):

```sh
cargo sekvent init
```

`init` writes `sekvent.toml` (project name derived from the directory name)
and `.sekvent/run.sh`, creates `.remote-build.toml` or adds the
`[run.commands].sekvent` entry to an existing one, and inserts the managed
sekvent section into `AGENTS.md`. Existing files are left alone unless you
pass `--force`. It does **not** change your `Cargo.toml`: add the sekvent
crates to `[workspace.dependencies]` yourself,

```toml
[workspace.dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master" }
sekvent-testing = { git = "https://github.com/westito/sekvent-api", branch = "master" }
sekvent-proto-build = { git = "https://github.com/westito/sekvent-api", branch = "master" }
```

then run `cargo sekvent deps check` to compare your third-party versions with
sekvent's pins. `--kind` only changes the suggested next command (`cargo
sekvent add <name> --kind …`). Moving an existing backend's code onto
sekvent is covered by the `sekvent-migrate` agent skill.

### Dependency modes

The facade's package name is `sekvent-api`; its library is `sekvent`. Keep
the dependency key `sekvent-api` (or rename it with
`package = "sekvent-api"`): the proc macros find the facade through that key.

- **Git on `master` (default).** What `new` writes. `Cargo.lock` records the
  exact commit, so builds are reproducible until you move it on purpose.
- **Path dependencies.** `cargo sekvent new NAME --sekvent-path ../sekvent-api`
  writes path dependencies on `crates/sekvent` (and the other two crates)
  instead. The path must be a sekvent checkout, and `new` writes it
  **absolute** (resolved from the current directory, symlinks followed), so
  the result does not build anywhere the checkout is not at that exact path
  (other clones, CI, a remote builder). Use it to develop sekvent itself next
  to a scratch project.

  To vendor sekvent as a git submodule inside your repository, write relative
  path dependencies by hand. `new` refuses a directory that is not empty, so
  it cannot scaffold into a repository that already holds the submodule;
  scaffold first (with the default git dependencies) or use `init` in an
  existing workspace, then run
  `git submodule add https://github.com/westito/sekvent-api vendor/sekvent-api`
  and replace the three entries in the root `[workspace.dependencies]`:

  ```toml
  sekvent-api = { path = "vendor/sekvent-api/crates/sekvent" }
  sekvent-testing = { path = "vendor/sekvent-api/crates/sekvent-testing" }
  sekvent-proto-build = { path = "vendor/sekvent-api/crates/sekvent-proto-build" }
  ```

  A path dependency has no git revision for `.sekvent/run.sh` to install the
  CLI from; set `SEKVENT_SRC` to the checkout in the builder or CI
  environment. A path outside your repository does not exist on a remote
  builder that syncs only your repository's files.

### Keep sekvent current

```sh
cargo sekvent sdk status --remote   # locked revision, and whether master moved on
cargo sekvent sdk update            # cargo update -p for the sekvent crates; writes Cargo.lock only
cargo sekvent self-update           # newer CLI, which carries the newer third-party pins
cargo sekvent deps check            # compare [workspace.dependencies] with those pins
cargo sekvent deps sync             # align versions and features in place
cargo update --workspace            # refresh Cargo.lock after deps sync
```

`sdk update` moves only the sekvent revision. The third-party pins that
`deps check` and `deps sync` compare against are compiled into the CLI, so
update the CLI first. `deps sync` never removes anything and keeps comments
and formatting. The gate prints a one-line hint when the locked sekvent is
behind `master` (best effort, at most 5 seconds; `SEKVENT_NO_UPDATE_CHECK=1`
turns it off).

### CI

```sh
cargo sekvent ci generate github       # .github/workflows/gate.yml
cargo sekvent ci generate bitbucket    # bitbucket-pipelines.yml
```

Both run `gate` and `coverage` through `.sekvent/run.sh` with `CI=true`, so
the CLI runs every step on the runner at the sekvent revision `Cargo.lock`
pins. They pin the Rust toolchain that the running CLI was built for
(sekvent's own `rust-toolchain.toml`), which is what `new` wrote into your
`rust-toolchain.toml`; if your project has since moved to another version,
edit the workflow to match. They give the harness Docker and set
`SEKVENT_DOCKER_TESTS=1`, but the container-backed tests run only when
`sekvent.toml` also sets `docker_tests = true` under `[harness]` (the
generated file has `false`): the gate passes `--include-ignored` only then.
They do not install `protoc` and do not build or push images. Existing files
are replaced only with `--force`.

### Agent skills and AGENTS.md

```sh
cargo sekvent skills install            # into ~/.kodein/skills, and ~/.claude/skills if it exists
cargo sekvent skills install --dest DIR
cargo sekvent agents                    # refresh the managed section of AGENTS.md
```

The skills are `sekvent` (writing code in a sekvent project),
`sekvent-new-project` and `sekvent-migrate`. Only skill directories whose
name starts with `sekvent` are written or replaced. `agents` rewrites only
the text between the `sekvent:begin` and `sekvent:end` markers (appending
the section when there is none) and refuses a file with an opening marker
but no closing one.

## 6. Next steps

- [CLI reference](cli.md): every command, flag, environment variable and
  `sekvent.toml` key.
- [Facade features](features.md): which feature turns on what.
- Modules: [config](modules/config.md), [error](modules/error.md),
  [context](modules/context.md), [telemetry](modules/telemetry.md),
  [runtime](modules/runtime.md), [server](modules/server.md),
  [jobs](modules/jobs.md), [db](modules/db.md), [auth](modules/auth.md),
  [resilience](modules/resilience.md), [client](modules/client.md),
  [link](modules/link.md), [components](modules/components.md),
  [proto-build](modules/proto-build.md), [testing](modules/testing.md).
- [Component model](component-model.md): split a feature into a component
  that runs in-process today and in its own service later, by configuration.
- [examples/shop](../examples/shop/README.md): a complete three-component
  example, including a split topology over gRPC and contract baselines.
