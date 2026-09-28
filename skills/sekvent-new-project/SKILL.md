---
name: sekvent-new-project
description: Create a new Rust backend project on the sekvent framework, or add another service crate to one, with `cargo sekvent new` / `cargo sekvent add`. Use when the user asks for a new service, API, worker or backend workspace in Rust and no project exists yet, or wants a second gRPC/HTTP/worker crate in an existing sekvent workspace. Covers choosing the kind (grpc, http, worker), what the generator writes, getting the first gate green, and choosing GitHub or Bitbucket CI. Not for moving an existing backend onto sekvent (use `sekvent-migrate`) or for writing code inside a project (use `sekvent`).
---

# New sekvent project

## 0. The CLI

```sh
cargo sekvent --version || curl -fsSL https://raw.githubusercontent.com/westito/sekvent/master/scripts/install.sh | sh
```

The installer puts a prebuilt `cargo-sekvent` into `${CARGO_HOME:-~/.cargo}/bin`
after checking its SHA-256. Inside the remote builder and in CI the project's
`.sekvent/run.sh` installs the CLI at the exact revision `Cargo.lock` pins
into `$CARGO_HOME/sekvent-cli/<rev>` and runs that binary directly, so the
laptop copy only needs to be recent enough to generate.

## 1. Pick the kind

| Kind | You get | Pick it when |
|---|---|---|
| `grpc` | `crates/<name>` (tonic service on the sekvent server, gRPC-Web included) + `crates/<name>-proto` (portable messages) + `proto/<pkg>/v1/<name>.proto` | other services or a typed frontend call it |
| `http` | `crates/<name>` (axum routes under `/api`) | a REST/JSON API, webhooks, a browser backend without protobuf |
| `worker` | `crates/<name>` (one runtime unit, no listener, backoff loop) | queue consumers, pollers, sync jobs |

Every kind gets health, graceful shutdown, `Config` via `#[derive(EnvConfig)]`,
a thin `main.rs` and an integration test that starts it through the runtime
handle.

## 2. Generate

```sh
cargo sekvent new orders --kind grpc      # new workspace ./orders with one service
cd orders
cargo sekvent add billing --kind http     # another crate in the same workspace
```

Names are kebab-case crate names; the proto package defaults to
`<name>.v1`. Generation never overwrites existing files.

What the workspace contains:

- `Cargo.toml` — `[workspace.dependencies]` with `sekvent`, `sekvent-testing`,
  `sekvent-proto-build` as git dependencies on `master` (the revision is
  pinned in `Cargo.lock`) and the full pinned third-party set; the lint
  policy (`clippy::pedantic`, `missing_docs`, `unsafe_code = "deny"`);
  release profile.
- `rust-toolchain.toml`, `rustfmt.toml`, `.gitignore`, `.dockerignore`,
  `.env.example`.
- `sekvent.toml` — gate, coverage floors (95 % lines; 0 for `*-proto`),
  harness labels, hooks, tasks.
- `.remote-build.toml` and `.sekvent/run.sh` — the remote builder config and
  the in-container CLI bootstrap (see the `build-on-rtx` skill).
- `AGENTS.md` (with the managed sekvent section) and one per crate.
- `Dockerfile` — cargo-chef multi-stage, non-root, `ARG BIN`; build only.

## 3. First gate

```sh
cargo generate-lockfile          # local; pins the sekvent revision
cargo fmt --all                  # local
cargo sekvent gate               # remote: fmt --check, clippy -D warnings, tests
cargo sekvent coverage           # remote: per-package line floors
```

Commit `Cargo.lock`: the CLI in containers and CI reads the sekvent revision
from it. If the remote builder is unreachable, stop and report; do not fall
back to local builds. A project or machine that has no remote builder at all
opts out explicitly with `[remote].mode = "local"` in `sekvent.toml` (or
`SEKVENT_LOCAL=1` for one shell); the CLI's error for a missing `rrb` says
the same.

Then make it yours: rewrite the `Ping` RPC / `/api/ping` route or the
`Heartbeat` job, fill in the "what this is" parts of `AGENTS.md` and
`README.md`, and keep `main.rs` thin.

## 4. CI

```sh
cargo sekvent ci generate github       # .github/workflows/gate.yml
cargo sekvent ci generate bitbucket    # bitbucket-pipelines.yml
```

Both run the same gate and coverage through `.sekvent/run.sh` with `CI=true`
(the CLI then runs locally on the runner), install `protoc`, pin the
toolchain to `rust-toolchain.toml` and enable Docker for the harness tests.
Both are gate-only: image publishing and deployment belong to the
environment, not to this template.

## Later

- `cargo sekvent sdk update` moves to the newest sekvent and its pinned set;
  `cargo sekvent deps check` reports drift.
- `cargo sekvent agents` refreshes the sekvent section of `AGENTS.md`.
