# sekvent

sekvent is a Rust backend framework. It gives a service the pieces every
backend ends up writing by hand — configuration, an error model, a per-call
context, a staged lifecycle with health checks, resilience policies, password
and JWT auth, service-to-service tokens, an outbound HTTP client, database
pools and migrations, a container-backed test harness and protobuf codegen
helpers — plus `cargo sekvent`, a CLI that scaffolds workspaces and runs the
quality gate.

Services depend on one facade crate, `sekvent`, and turn on only the modules
they use. The code is written for edition 2024 and tonic 0.14, axum 0.8,
sqlx 0.9 and sea-orm 2.0.

## Crates

| Crate | Facade module / feature | Responsibility |
|---|---|---|
| `sekvent` | — | Facade re-exporting the libraries below behind features, plus `sekvent::prelude` |
| `sekvent-config` | `config` (default) | `ConfigSource`, `Secret`, readers, `FromConfig`, `#[derive(EnvConfig)]` |
| `sekvent-macros` | (via `config`) | Procedural macros (`EnvConfig`) |
| `sekvent-error` | `error` (default); `error-http`, `error-grpc` | `ErrorCode`, `AppError`, `WireError`; HTTP and gRPC mappings |
| `sekvent-context` | `context` (default) | `CallContext`, `ServiceIdentity`, `Clock`, header codec |
| `sekvent-telemetry` | `telemetry` (default) | Tracing init, log ring buffer, request ids, `truncate_for_log` |
| `sekvent-runtime` | `runtime` (default); `runtime-grpc-web` | Staged lifecycle, supervision, health, one listener for gRPC, gRPC-Web and REST |
| `sekvent-resilience` | `resilience` | Backoff, retry budget, rate gate, timeout, bulkhead, circuit breaker |
| `sekvent-auth` | `auth`; `auth-axum`, `auth-tonic` | Argon2/bcrypt password hashing, JWT with injected time, login helper |
| `sekvent-link` | `link`; `link-axum`, `link-tonic` | Service-to-service tokens, middleware, interceptors |
| `sekvent-client` | `client` | Outbound HTTP (reqwest, rustls) with policies, context propagation, OAuth 2.0 client credentials |
| `sekvent-db` | `db`; `db-sqlx-postgres`, `db-sqlx-mysql`, `db-sea-orm-postgres`, `db-sea-orm-mysql`, `db-migrate`, `db-sea-orm-migrate` | Named pools, migrations, distinct-target check, list filters |
| `sekvent-testing` | not re-exported (`[dev-dependencies]`) | Postgres and MySQL test containers, a reaper, `await_until!` |
| `sekvent-proto-build` | not re-exported (`[build-dependencies]`) | `build.rs` protobuf codegen on top of `tonic-prost-build` |
| `cargo-sekvent` | — | The `cargo sekvent` CLI |
| `sekvent-tasks` | — | The CLI's task library (gate, coverage, scaffolding, pins) |

The `full` feature turns on every module and integration.

```rust
use sekvent::prelude::*; // AppError, ErrorCode, CallContext, Secret, EnvConfig, FromConfig, Runtime, Server, …
```

## Quick start

Install the CLI (a prebuilt binary checked against its SHA-256, or a source
build when no binary exists for your platform):

```sh
curl -fsSL https://raw.githubusercontent.com/westito/sekvent/master/scripts/install.sh | sh
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
sekvent = { git = "https://github.com/westito/sekvent", branch = "master" }
sekvent-testing = { git = "https://github.com/westito/sekvent", branch = "master" }
sekvent-proto-build = { git = "https://github.com/westito/sekvent", branch = "master" }
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

## Gate and coverage

| Command | What it runs |
|---|---|
| `cargo sekvent gate` | `fmt --check`, clippy with warnings denied, tests; optionally `cargo doc` and boundary checks; hooks and test-container cleanup |
| `cargo sekvent check` / `clippy` / `test [-- args]` | One step of the gate over the same package selection |
| `cargo sekvent coverage [--lcov PATH] [--misses PACKAGE]` | Instrumented tests and per-package line coverage floors |
| `cargo sekvent boundaries` | Check `[[gate.boundaries]]` against the dependency graph |
| `cargo sekvent harness-clean --run ID \| --stale \| --all --yes` | Remove leftover test containers of the label namespace |
| `cargo sekvent run [task] [args]` | Run a `[tasks.<name>]`; without a name, list them |
| `cargo sekvent ci generate github\|bitbucket [--force]` | Render the gate-only CI template |
| `cargo sekvent agents [--file PATH]` | Insert or refresh the sekvent section of `AGENTS.md` |

Compiling commands (`gate`, `check`, `clippy`, `test`, `coverage`,
`harness-clean`) are forwarded to the remote builder when `[remote] mode` is
`rrb`, and run on the current machine in CI or with `mode = "local"`.
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

## Component model (planned)

A component model is planned: start a feature as a component inside one
binary and later move it into its own service by configuration, without
changing call sites. It is not implemented yet; the design is in
[docs/component-model.md](docs/component-model.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
