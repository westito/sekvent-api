# sekvent — workspace rules

sekvent is a general-purpose Rust backend framework: configuration, errors,
call context, lifecycle, resilience, auth, service links, clients, databases,
an embedded component model, a test harness and codegen helpers, plus the
`cargo sekvent` CLI. It is written from scratch. Never copy code, comments,
names, hosts, labels or business rules from any client project into this
repository; describe behaviour in your own words and use neutral examples
(`billing`, `orders`, `inventory`).

- Code, comments, identifiers and docs are in English.
- Edition 2024, toolchain pinned in `rust-toolchain.toml` (includes `rust-src`,
  which trybuild needs).
- Every crate has `#![forbid(unsafe_code)]` unless its manual says otherwise.
- Licence: MIT OR Apache-2.0. Deployment is out of scope: projects own it.

## Repository and consumption

- Remote: `git@github.com:westito/sekvent-api.git` (public; anonymous
  `https://github.com/westito/sekvent-api` works for clones, git
  dependencies, the install script and `self-update`), branch `master`.
  Projects track `master`; the only tag is the rolling `cli-latest`
  prerelease that `cli-release.yml` moves on every push.
- The repository is public: no client names, client paths, hosts or
  session notes in any committed file, history included. Handoff state
  lives outside the repo.
- The facade is the package **`sekvent-api`** with lib name **`sekvent`**
  (directory `crates/sekvent`). Consumers depend on it under the key
  `sekvent-api` and write `use sekvent::…`. Internal crates keep their
  `sekvent-*` names; the CLI stays `cargo sekvent`.
- Projects consume sekvent either as git dependencies on `branch =
  "master"` (what `cargo sekvent new` writes by default; `init` writes no
  dependencies, only `sekvent.toml`, `.sekvent/run.sh`, `.remote-build.toml`
  and the `AGENTS.md` section) or as **path dependencies** on a checkout,
  e.g. a submodule:
  ```toml
  sekvent-api = { path = "vendor/sekvent-api/crates/sekvent", features = ["…"] }
  sekvent-testing = { path = "vendor/sekvent-api/crates/sekvent-testing" }         # dev
  sekvent-proto-build = { path = "vendor/sekvent-api/crates/sekvent-proto-build" } # build
  ```
  `cargo sekvent new --sekvent-path <checkout>` writes absolute path deps
  (relative submodule paths are written by hand); `cargo sekvent add` keeps
  the source an existing workspace already uses.
- The proc macros locate the facade with proc-macro-crate, which reports the
  dependency *key*. `sekvent-api` (mapped to `sekvent`) and any rename via
  `package = "sekvent-api"` work; the key `sekvent_api` does not.
- rrb syncs only a project's own git file set to rtx, so a path dependency
  outside the consuming project does not exist on rtx. Use git
  dependencies or a submodule inside the project (rrb syncs submodule
  contents) before relying on remote builds.

## Crate map

| Crate | Responsibility | Depends on (sekvent) |
|---|---|---|
| `sekvent-config` | `ConfigSource`, `Secret`, readers, `FromConfig`, reserved `SEKVENT_*` keys | macros (derive) |
| `sekvent-macros` | proc macros (`EnvConfig`, `component`, `ComponentError`) | — |
| `sekvent-telemetry` | tracing init, log ring buffer, request-id and access-log layers, `truncate_for_log` | — |
| `sekvent-error` | `ErrorCode`, `AppError`, `WireError`, HTTP + gRPC mappings | — |
| `sekvent-context` | `CallContext`, `ServiceIdentity`, `Clock`, header codec (peer vs external propagation) | — |
| `sekvent-runtime` | staged lifecycle, supervision, health, combined server (CORS, limits, layers, access log), jobs, downloads | config, error, context, telemetry |
| `sekvent-resilience` | backoff, retry budget, rate gate, timeout, bulkhead, circuit breaker, TTL cache | config, error, context |
| `sekvent-auth` | argon2id and bcrypt schemes, JWT with the time passed in; runtime-free, wasm-compatible core | error, config, context (optional) |
| `sekvent-link` | service tokens, `TokenMap`, middleware, interceptor | config, context, error |
| `sekvent-client` | reqwest builder, resilience, context propagation, OAuth2 cache | config, context, error, resilience, telemetry |
| `sekvent-db` | named pools, migrations, distinct-target check, sea-orm filters, probes, leases | config, error, runtime (optional) |
| `sekvent-component` | components, App builder, local, serialized and gRPC bindings | config, error, context, resilience, link (optional), macros, runtime (optional) |
| `sekvent-testing` | Postgres/MySQL containers, reaper, `await_until!` | — |
| `sekvent-proto-build` | build.rs codegen helpers, component service-contract constants and RPC type aliases | — |
| `sekvent-api` (lib `sekvent`) | facade re-exporting the above behind features | all |
| `sekvent-facade-check` | compiles and tests a component through the facade alone | facade |
| `sekvent-tasks` | `cargo sekvent` logic: scaffold, gate, coverage, deps, contract emit/check, rrb dispatch, self-update | — |
| `cargo-sekvent` | thin CLI binary over `sekvent-tasks` | tasks |

Dependencies point down this table's "Depends on" column only; never add a
cycle, and keep `sekvent-error` and `sekvent-context` dependency-light.

## Dependencies

- All third-party versions are pinned once in the root `[workspace.dependencies]`;
  crates use `{ workspace = true }`. One TLS stack (rustls); never native-tls.
- Pin new dependencies to their latest release (look it up, never guess).
- A crate's heavy integrations sit behind features (`http`, `grpc`, `axum`,
  `tonic`, `sqlx-postgres`, `sea-orm-mysql`, …) so a consumer pays only for
  what it uses.

## Design rules

- Fail closed. Constructors and builders return `Result`; a missing binding,
  token or config key is a startup error that names the key, never the value.
- Never log or format secrets, tokens, passwords, database URLs or upstream
  bodies. Use `Secret` and `truncate_for_log`. `AppError` is neither `Clone`
  nor `PartialEq`.
- Inject time (`Clock`) and randomness where behaviour depends on them.
- Public types that downstream code may match on are exhaustive only when the
  set is fixed by a spec (`ErrorCode`); otherwise `#[non_exhaustive]`.
- Keep `src/main.rs` files thin; logic lives in libraries.
- Dependency injection is constructor injection through the fail-closed
  `App` builder; there is no runtime service locator.

## Components

Specs: `docs/component-model.md` (overview), `docs/design/component-c1.md`
(local bindings, lifecycle), `docs/design/component-c2.md` (gRPC, contracts),
`docs/design/p8-service-essentials.md` (jobs, leases).
Milestones C1, C2 and C4 are done (C4: jobs in `sekvent-runtime`, leases in
`sekvent-db`); C3 and C5 (bus, async/deferred calls, NATS, extraction
tooling) are not started.

- Every component that may cross a process boundary (standard and
  `remote_only`) declares its `service` in the `-api` crate's `.proto` and
  names the generated module with `proto = "…"` on `#[component]`; the
  macro checks trait and service against each other at compile time: an
  exact 1:1 method/RPC mapping and request/reply types by identity (nested
  messages included). `()` maps to `google.protobuf.Empty`. `local_only`
  components have no contract.
- The `grpc` binding (feature `grpc`, facade `component-grpc`) is one
  generic byte-level tonic client and service over C1's `Dispatch`; never
  generate per-component tonic code, and keep `-api` crates free of tonic.
  Transport is plaintext HTTP/2 with link tokens; TLS comes later.
- Serving order is fixed: authenticate and decode the context from request
  **headers** before the body is read (trailers are never trusted), exact
  `/<service>/<Rpc>` routing, then the gate. Requests are capped at 4 MiB.
  A handler panic answers `INTERNAL` (`HANDLER_PANICKED`).
- A method `TIMEOUT` bounds the whole call and, when it fires while the
  caller still has time, fails with `METHOD_TIMEOUT` and counts against the
  callee's breaker; the caller's own deadline or cancellation never does.
  Transient failures from further down a call chain become `INTERNAL`
  (`DOWNSTREAM_FAILURE`) at the serving boundary so callers above neither
  retry nor blame the healthy component in between.
- An idempotency key set for a call is forwarded to its callee; a key
  received from upstream is not. Third-party HTTP upstreams get only
  external headers (request id, traceparent, timeout), never hops.
- Lifecycle: start and stop are cancel-safe, phase transitions are atomic,
  every started component's `on_stop` runs exactly once within the runtime
  stop deadline. Duplicate gRPC service names and the link name `local`
  fail the build.
- Link authentication is fail-closed in both directions; a key that acts
  under one binding is accepted under every binding, so one environment
  serves every topology.
- `cargo sekvent contract emit` writes baselines (format 2) into the tree:
  run it on the Mac, like other in-tree codegen, and commit the JSON.
  `contract check` runs in-process (protox, no `protoc`, no build) and is
  part of the gate when `[contract]` has roots; `examples/shop` dogfoods it.

## Tests

- Unit tests sit next to the code (`#[cfg(test)] mod tests`); integration
  tests in `tests/`. Tests are deterministic: completion signals, injected
  clocks and `tokio::time::pause`, never fixed sleeps.
- Tests that mutate process environment or global tracing live in their own
  test binary.
- Anything with a socket binds `127.0.0.1:0` and runs on the real clock
  (auto-advance would fire timers while loopback I/O is pending): assert
  lower time bounds and structure, and guard against hangs with a 30 s
  `tokio::time::timeout`. An address nobody listens on is `127.0.0.1:1`,
  never a bound-then-dropped port. Warm lazy gRPC channels before asserting
  on deadlines. `await_until!` only observes external state.
- Docker tests are `#[ignore]`d and run with `SEKVENT_DOCKER_TESTS=1` and
  `--include-ignored` (the gate does this).
- Coverage gate: 95% lines per library crate (`rrb run coverage`);
  `tests/`, `examples/`, `main.rs` and `sekvent-macros` are excluded.

## Verification (rtx via rrb)

Everything that compiles runs on rtx; `cargo fmt` (writing form), lockfile
refreshes and in-tree codegen stay on the Mac. See
`~/.kodein/skills/build-on-rtx/SKILL.md`. Never run two rrb commands at once;
logs land in `~/.cache/rrb/sekvent/logs/`.

```sh
~/.kodein/skills/build-on-rtx/bin/rrb run check
~/.kodein/skills/build-on-rtx/bin/rrb run clippy     # --keep-going
~/.kodein/skills/build-on-rtx/bin/rrb run gate       # fmt --check + clippy -D warnings + tests + doctests
~/.kodein/skills/build-on-rtx/bin/rrb run coverage
```

`rrb run test` stops at the first failing test binary; for a full picture use
`rrb shell bash -c 'cargo test --workspace --no-fail-fast …'`.

In-tree codegen (Mac only, after the code compiles):

```sh
INSTA_UPDATE=always cargo test -p sekvent-macros --lib          # expansion snapshots
TRYBUILD=overwrite cargo test -p sekvent-macros --test ui        # ui .stderr
(cd examples/shop && cargo run -p cargo-sekvent -- sekvent contract emit)
```

Review every regenerated snapshot, `.stderr` and baseline before committing.

If rtx is unreachable, stop and report; there is no local build fallback.

## Working method

- Larger changes: a design note under `docs/design/`, then parallel agents
  with disjoint write sets that write code and tests without building, then
  a single verification agent that runs the rrb loop, then review and fix.
- User docs live in `docs/` (index `docs/README.md`, one page per module
  under `docs/modules/`, `getting-started.md`, `cli.md`, `features.md`). A
  change to a public API, feature, config key, CLI flag or default updates
  the matching page in the same commit; design notes record decisions, the
  module pages describe current behaviour.
- Commit only when the gate and coverage are green. Do not push or change
  the repository's visibility without asking.
