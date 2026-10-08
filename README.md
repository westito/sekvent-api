# sekvent

sekvent is a Rust backend framework. It gives a service the pieces every
backend ends up writing by hand — configuration, an error model, a per-call
context, a staged lifecycle with health checks, resilience policies, password
and JWT auth, service-to-service tokens, an outbound HTTP client, database
pools and migrations, a container-backed test harness and protobuf codegen
helpers — plus `cargo sekvent`, a CLI that scaffolds workspaces and runs the
quality gate.

Services depend on one facade package, `sekvent-api`, and turn on only the
modules they use. Its library is named `sekvent`, so code writes
`use sekvent::…`. The code is written for edition 2024 and tonic 0.14, axum 0.8,
sqlx 0.9 and sea-orm 2.0.

## Crates

| Crate | Facade module / feature | Responsibility |
|---|---|---|
| `sekvent-api` (lib `sekvent`) | — | Facade re-exporting the libraries below behind features, plus `sekvent::prelude` |
| `sekvent-config` | `config` (default) | `ConfigSource`, `Secret`, readers, `FromConfig`, `#[derive(EnvConfig)]` |
| `sekvent-macros` | (via `config`, `component`) | Procedural macros (`EnvConfig`, `component`, `ComponentError`) |
| `sekvent-error` | `error` (default); `error-http`, `error-grpc` | `ErrorCode`, `AppError`, `WireError`; HTTP and gRPC mappings |
| `sekvent-context` | `context` (default) | `CallContext`, `ServiceIdentity`, `Clock`, header codec |
| `sekvent-telemetry` | `telemetry` (default) | Tracing init, log ring buffer, request ids, access log, `truncate_for_log` |
| `sekvent-runtime` | `runtime` (default); `runtime-grpc-web`, `runtime-cron` | Staged lifecycle, supervision, health, one listener for gRPC, gRPC-Web and REST with CORS, body limits, global layers and an access log; interval, cron and manual jobs; file downloads |
| `sekvent-resilience` | `resilience` | Backoff, retry budget, rate gate, timeout, bulkhead, circuit breaker, a TTL cache with single flight |
| `sekvent-auth` | `auth`; `auth-axum`, `auth-tonic`, `auth-tokio` | argon2id and bcrypt password schemes, JWT with injected time, login helper, async hashing helpers |
| `sekvent-link` | `link`; `link-axum`, `link-tonic` | Service-to-service tokens, middleware, interceptors |
| `sekvent-client` | `client` | Outbound HTTP (reqwest, rustls) with policies, context propagation, OAuth 2.0 client credentials |
| `sekvent-db` | `db`; `db-sqlx-postgres`, `db-sqlx-mysql`, `db-sea-orm-postgres`, `db-sea-orm-mysql`, `db-migrate`, `db-sea-orm-migrate`, `db-lease` | Named pools, migrations, distinct-target check, list filters, readiness probes per pool, leases with fencing tokens for singleton jobs |
| `sekvent-component` | `component`; `component-grpc` | Components with `local`, `local-serialized` and `grpc` bindings, the fail-closed `App` builder, lifecycle, deadlines, bulkheads, retries and circuit breakers, serving components over gRPC |
| `sekvent-testing` | not re-exported (`[dev-dependencies]`) | Postgres and MySQL test containers, a reaper, `await_until!` |
| `sekvent-proto-build` | not re-exported (`[build-dependencies]`) | `build.rs` protobuf codegen on top of `tonic-prost-build`; protos compile in-process with protox, so no `protoc` is needed |
| `cargo-sekvent` | — | The `cargo sekvent` CLI |
| `sekvent-tasks` | — | The CLI's task library (gate, coverage, scaffolding, pins) |

The `full` feature turns on every module and integration.

```rust
use sekvent::prelude::*; // AppError, ErrorCode, CallContext, Secret, EnvConfig, FromConfig, Runtime, Server, …
```

## Documentation

The user guide lives in [`docs/`](docs/README.md):

- [Getting started](docs/getting-started.md): install the CLI, bootstrap a
  workspace, run and grow a first service.
- [Facade features](docs/features.md) and the [CLI reference](docs/cli.md)
  (every command and the full `sekvent.toml` reference).
- One page per module under [`docs/modules/`](docs/modules/): config, error,
  context, telemetry, runtime, server, jobs, db, auth, link, client,
  resilience, components, proto-build and testing.

## Quick start

Install the CLI (a prebuilt binary checked against its SHA-256, or a source
build of the same `cli-latest` tag when no binary exists for your platform):

```sh
curl -fsSL https://raw.githubusercontent.com/westito/sekvent-api/master/scripts/install.sh | sh
```

Create a workspace with its first service:

```sh
cargo sekvent new my-app --kind grpc     # or --kind http, --kind worker
cd my-app
cargo generate-lockfile
cargo sekvent gate
```

| Kind | First crate |
|---|---|
| `grpc` | A tonic service on the sekvent server (gRPC-Web included), a `-proto` crate with portable messages and a `.proto` file |
| `http` | axum routes under `/api` |
| `worker` | One runtime unit with a backoff loop and no listener |

`cargo sekvent new` also takes `--dir PATH`, `--ci github|bitbucket|none`,
`--no-git` and `--sekvent-path PATH` (depend on a local sekvent checkout).
Add more services later with `cargo sekvent add <name> --kind grpc|http|worker`,
or put an existing Cargo workspace on sekvent with `cargo sekvent init`.

Keep the CLI current with `cargo sekvent self-update`, which replaces the
binary with the latest build of the rolling `cli-latest` release.

## Depending on sekvent

Generated workspaces depend on sekvent through git on the `master` branch:

```toml
[workspace.dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master" }
sekvent-testing = { git = "https://github.com/westito/sekvent-api", branch = "master" }
sekvent-proto-build = { git = "https://github.com/westito/sekvent-api", branch = "master" }
```

`Cargo.lock` records the exact revision, so builds are reproducible until you
move it on purpose. Commit `Cargo.lock`.

| Command | What it does |
|---|---|
| `cargo sekvent sdk status [--remote]` | Show the locked sekvent revision; `--remote` compares it with the branch head |
| `cargo sekvent sdk update` | `cargo update` the sekvent crates (writes `Cargo.lock`, compiles nothing) |
| `cargo sekvent deps check [--strict]` | Compare `[workspace.dependencies]` with the third-party versions sekvent is tested against |
| `cargo sekvent deps sync [--only a,b]` | Rewrite versions and add missing features and dependencies to match those pins |

## `sekvent.toml`

Each workspace has a `sekvent.toml` at its root. Unknown keys are rejected;
every section except `[project]` is optional.
`cargo sekvent config show` prints the effective file with defaults filled in.

| Section | Holds |
|---|---|
| `[project]` | The project name |
| `[remote]` | Where compiling commands run: `mode = "rrb"` forwards them to a remote builder, `"local"` runs them here; CI always runs locally |
| `[gate]` | Excluded packages, features, test threads, extra clippy arguments, `fmt`, `doc`, and `[[gate.boundaries]]` dependency rules |
| `[coverage]` | `fail_under_lines` (default 95), ignored file patterns, excluded packages, per-package `[coverage.thresholds]` |
| `[harness]` | Label namespace of test containers and when they count as stale |
| `[hooks]` | Commands run before and after the gate and coverage |
| `[tasks.<name>]` | Project tasks for `cargo sekvent run <name>` |
| `[contract]` | Proto roots, include paths and the baseline directory of `cargo sekvent contract emit` / `check`; `gate = true` adds the check to the gate |

## Gate and coverage

| Command | What it runs |
|---|---|
| `cargo sekvent gate` | `fmt --check`, clippy with warnings denied, tests; optionally `cargo doc`, boundary checks and the contract check; hooks and test-container cleanup |
| `cargo sekvent check` / `clippy` / `test [-- args]` | One step of the gate over the same package selection |
| `cargo sekvent coverage [--lcov PATH] [--misses PACKAGE]` | Instrumented tests and per-package line coverage floors |
| `cargo sekvent boundaries` | Check `[[gate.boundaries]]` against the dependency graph |
| `cargo sekvent contract emit\|check [service…]` | Write component contract baselines from the protos, or check the protos against them for wire-breaking changes (no Rust build, no `protoc`) |
| `cargo sekvent harness-clean --run ID \| --stale \| --all --yes` | Remove leftover test containers of the label namespace |
| `cargo sekvent run [task] [args]` | Run a `[tasks.<name>]`; without a name, list them |
| `cargo sekvent ci generate github\|bitbucket [--force]` | Render the gate-only CI template |
| `cargo sekvent agents [--file PATH]` | Insert or refresh the sekvent section of `AGENTS.md` |

Compiling commands (`gate`, `check`, `clippy`, `test`, `coverage`,
`harness-clean`) are forwarded to the remote builder when `[remote] mode` is
`rrb`, and run on the current machine in CI (`CI` set to anything but
empty, `0`, `false`, `no` or `off`), inside the builder, with
`SEKVENT_LOCAL=1` or with `mode = "local"`.

The default `[remote]` expects the maintainers' `rrb` remote-build tool at
`~/.kodein/skills/build-on-rtx/bin/rrb` (change it with `[remote].rrb`). If
you do not use a remote builder, set this in `sekvent.toml`:

```toml
[remote]
mode = "local"
```

or export `SEKVENT_LOCAL=1` for a single shell. A missing `rrb` is an error
that says so; it never silently turns into a local build.

Inside the builder and in CI, `.sekvent/run.sh` runs the CLI at the exact
sekvent revision `Cargo.lock` pins. Each revision is built once into its own
directory, `$CARGO_HOME/sekvent-cli/<rev>`, and executed from there, so a
`self-update` or another project sharing `CARGO_HOME` never changes which
CLI a project runs. CI caches that directory.
Unknown subcommands are passed to the workspace's `xtask` package when there
is one.

sekvent's own repository runs the same checks in
[`.github/workflows/gate.yml`](.github/workflows/gate.yml): format check,
clippy with `-D warnings`, the test suite, and a 95 % line coverage floor
through `cargo llvm-cov`.

## Agent skills

sekvent ships skills for coding agents in [`skills/`](skills/):

| Skill | Use it to |
|---|---|
| `sekvent` | Write code in a sekvent project: crate map, recipes, pitfalls |
| `sekvent-new-project` | Create a workspace or add a service crate |
| `sekvent-migrate` | Move an existing Rust backend onto sekvent |

```sh
cargo sekvent skills install               # default agent skill directories
cargo sekvent skills install --dest DIR    # one directory only
```

Only directories whose name starts with `sekvent` are written or replaced.

## Component model

A component is a trait with a protobuf contract. Callers hold a generated
handle and never know whether the implementation runs in the same task,
behind a serialization boundary or in another service; the binding is
chosen by configuration when the App is built. Milestones C1, C2 and C4 are
implemented: `local`, `local-serialized` and `grpc` bindings, a fail-closed
App builder with constructor injection, lifecycle hooks with draining,
per-method deadlines and bulkheads, and — on the `grpc` binding — link
authentication, budgeted retries of idempotent methods, a circuit breaker
per remote component and named policies; C4 adds interval, cron and manual
jobs (`sekvent-runtime`) and singleton jobs over a database lease with
fencing tokens (`sekvent-db`). Queues follow in later milestones.

```rust
use sekvent::prelude::*;

#[derive(Debug, ComponentError)]
#[component_error(domain = "shop.inventory.v1")]
pub enum InventoryError {
    #[reason("OUT_OF_STOCK", code = FailedPrecondition, message = "only {available} of {sku} left")]
    OutOfStock { sku: String, available: u32 },
    #[other]
    Other(AppError),
}

#[sekvent::component(name = "inventory", package = "shop.inventory.v1", proto = "crate::proto::shop::inventory::v1")]
pub trait Inventory: Send + Sync + 'static {
    /// Reserve stock for an order.
    #[call(idempotent, timeout = "2s", bulkhead = 16)]
    async fn reserve(&self, cx: &CallContext, req: ReserveRequest)
        -> Result<ReserveReply, InventoryError>;
}

// Wiring: factories run in install order; dependencies come in as handles.
let mut builder = App::builder(&sekvent::config::EnvSource);
InventoryHandle::install(&mut builder, |_deps| Ok(InventoryService::new(stock)))?;
OrdersHandle::install(&mut builder, |deps| Ok(OrdersService::new(deps.handle::<InventoryHandle>()?)))?;
let app = builder.build()?;          // SEKVENT_COMPONENT_BINDING=local-serialized moves every call behind prost
app.start().await?;
```

The trait's `.proto` declares `service Inventory { rpc Reserve(…) … }`, and
`proto = "…"` makes the macro check the trait against it at compile time.
Moving inventory into its own service is configuration: the service sets
`SEKVENT_COMPONENT_INVENTORY_SERVE=grpc` and mounts `app.grpc_routes()` on
its `sekvent-runtime` server; callers set
`SEKVENT_COMPONENT_INVENTORY_BINDING=grpc`, `…_ENDPOINT=http://host:port`
and a link token. `cargo sekvent contract check` keeps the protos
wire-compatible with committed baselines.

Enable it with the facade feature `component` (plus `component-grpc` for
the `grpc` binding and serving, and `runtime` for `App::register`, which
runs every component as one runtime unit). The design is in
[docs/component-model.md](docs/component-model.md), the specifications in
[docs/design/component-c1.md](docs/design/component-c1.md),
[docs/design/component-c2.md](docs/design/component-c2.md) and, for jobs and
leases, [docs/design/p8-service-essentials.md](docs/design/p8-service-essentials.md), and
[examples/shop](examples/shop) is a complete three-component example whose
tests run under `monolith-local`, `monolith-serialized` and a `split-grpc`
topology where inventory runs as its own service
([split topology](examples/shop/README.md#split-topology)).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
