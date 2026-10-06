---
name: sekvent-migrate
description: Move an EXISTING Rust backend workspace (tonic, axum, sqlx or sea-orm services) onto the sekvent framework with `cargo sekvent init` and `cargo sekvent deps sync`. Use when the user wants an existing backend "on sekvent", wants its dependencies aligned with sekvent's pinned set, or wants hand-rolled config readers, error enums, tracing setup, password hashing, outbound HTTP clients, rate limits or protobuf codegen replaced by sekvent modules. Covers the inventory, the upgrade fallout (edition 2024, tonic 0.14 and `tonic-prost`, axum 0.8 paths, sea-orm 2.0, sqlx 0.9), keeping the wire format compatible, and verifying with the gate. Not for new projects (use `sekvent-new-project`) or for day-to-day code in a project already on sekvent (use `sekvent`).
---

# Migrating a backend onto sekvent

A migration is an upgrade plus a replacement, done in that order: first get
the workspace onto sekvent's toolchain and dependency set with nothing else
changed, then swap hand-rolled pieces for sekvent modules one at a time.
The service must behave the same on the wire when you are done.

Where commands run is the `build-on-rtx` skill's business: everything that
compiles (`cargo sekvent check|clippy|test|gate|coverage`) goes to the remote
builder; `cargo fmt`, `cargo update` and edits stay local. If the builder is
unreachable, stop and report — never fall back to a local build. (A project
without a remote builder opts out explicitly with `[remote].mode = "local"`
in `sekvent.toml`; that is a configuration choice, not a fallback.) For a test
suite that is slow or flaky once it compiles, use the `rust-testing` skill.

## 0. Work on a branch in a worktree

```sh
git worktree add ../<repo>-sekvent -b sekvent-migration
cd ../<repo>-sekvent
```

Never migrate in the user's main checkout: other work may depend on its
uncommitted state. The result is one commit on that branch. Do not push,
open a pull request or merge unless the user asks.

## 1. Inventory before touching anything

Read the root `Cargo.toml`, every member's `Cargo.toml`, `build.rs` files and
`rust-toolchain.toml`, and write down:

- edition and toolchain of each crate;
- versions of `tonic`, `prost`, `tonic-build`/`prost-build`, `axum`,
  `tower-http`, `sqlx`, `sea-orm`, `sea-orm-migration`, `reqwest`, `tokio`,
  `jsonwebtoken`, `argon2`/`bcrypt`;
- which crates declare versions directly instead of `{ workspace = true }`;
- native-tls or aws-lc-rs/aws-lc-sys anywhere in the graph (sekvent is
  rustls-only, with the ring crypto provider:
  `cargo tree -e features -i aws-lc-sys`);
- the wire contract: custom request/trace header names, error body shape,
  gRPC error detail trailers, health endpoints the platform probes.

Then ask the CLI how far the workspace is from sekvent's pins (reads only):

```sh
cargo sekvent deps check              # older, newer, feature-mismatch, missing
```

Summarise the gap for the user before step 2 when it spans a major version
of tonic, axum, sqlx or sea-orm: those are real code changes, not a bump.

## 2. Add sekvent to the workspace

```sh
cargo sekvent init --kind grpc        # or http / worker: only the suggested flavour of new crates
```

`init` finds the workspace root and writes `sekvent.toml`,
`.sekvent/run.sh`, a `.remote-build.toml` (or adds its `sekvent` command to
an existing one) and the managed sekvent section of `AGENTS.md`. It never
overwrites without `--force`. It does not edit `Cargo.toml`; add the sekvent
crates to `[workspace.dependencies]` yourself:

```toml
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master" }   # the facade; code writes `use sekvent::…`
sekvent-testing = { git = "https://github.com/westito/sekvent-api", branch = "master" }       # if tests need containers
sekvent-proto-build = { git = "https://github.com/westito/sekvent-api", branch = "master" }   # if a build.rs compiles protos
```

Set `edition = "2024"` in `[workspace.package]` (and in members that do not
inherit it), and align `rust-toolchain.toml` with sekvent's. Review
`sekvent.toml`: put generated-code crates under `[coverage.thresholds]` with
`0.0`, and set `[gate] exclude` for crates that cannot pass yet — with a note
for the user, not silently.

## 3. Align dependencies, then fix the fallout crate by crate

Move every member's third-party dependency to `{ workspace = true }` first;
`deps sync` only edits the root `[workspace.dependencies]`.

```sh
cargo sekvent deps sync               # or --only tonic,prost,axum to go in slices
cargo update --workspace              # refreshes Cargo.lock, compiles nothing
cargo sekvent check                   # remote
```

`deps sync` sets each version to sekvent's pin (a project that was ahead is
moved back to it), appends features sekvent relies on and adds missing
pins; it never removes anything. Then fix compile errors one crate at a
time, leaves of the dependency graph first, re-running `cargo sekvent check`
after each. Typical fallout:

- **Edition 2024.** `std::env::set_var`/`remove_var` are `unsafe`: in tests,
  replace them with `sekvent::config::MapSource`; `gen` is a keyword;
  `impl Trait` in return position captures all lifetimes (add
  `+ use<..>` where a borrow must not be captured); `unsafe extern` blocks;
  `if let` temporaries drop earlier. `cargo fix --edition` run locally
  handles most mechanical cases.
- **tonic 0.14.** The prost codec moved out of `tonic` into `tonic-prost`,
  and code generation moved from `tonic-build` to `tonic-prost-build`. Add
  `tonic-prost` to crates that include generated service code, and replace
  the codegen in `build.rs` (step 4). Check `tonic::transport` feature names
  and interceptor signatures against the new version.
- **axum 0.8.** Path parameters are `/{id}` and wildcards `/{*rest}`; the old
  `/:id` and `/*rest` panic at router construction, so a test that builds
  the router catches every one. `async_trait` is gone from extractor traits
  (`FromRequestParts` uses native async fn); `Option<T>` extractors need
  `OptionalFromRequestParts`.
- **sea-orm 2.0 / sea-orm-migration 2.0.** Follow the upstream migration
  notes for entity and query API changes; keep behaviour, do not redesign
  the schema.
- **sqlx 0.9.** Re-check feature names (sekvent uses
  `runtime-tokio` + `tls-rustls-ring-webpki`) and regenerate offline query
  data if the project uses `query!` macros with `SQLX_OFFLINE`. Remove
  `tls-rustls-aws-lc-rs` and `tls-native-tls` by hand: `deps sync` only
  appends features.
- **rustls with ring only.** Remove `native-tls`/`default-tls` and the
  aws-lc-backed `rustls` feature from reqwest (again by hand); the pin uses
  `rustls-no-provider`. Code that builds its own `reqwest::Client` must then
  supply a provider, or `build()`/`Client::new()` panics: prefer
  `sekvent::client::HttpClient`, otherwise start from
  `sekvent::client::reqwest_builder()` (feature `client`), which installs
  ring as the process-wide rustls provider unless one is installed and
  leaves every reqwest TLS setting in effect.

Commit nothing yet; get `cargo sekvent gate` green on the upgraded but
otherwise unchanged code before step 4, so later diffs are pure
replacements.

## 4. Replace hand-rolled pieces with sekvent modules

One concern at a time, each ending in a green `cargo sekvent test`. Enable
features on the crate's own `sekvent-api = { workspace = true, features = [...] }`
line. The `sekvent` skill has the recipes with real API names.

| Hand-rolled | Replace with |
|---|---|
| tracing-subscriber setup in `main` | `sekvent::telemetry::init(service, TelemetryOptions::new())`; request ids from the runtime's server |
| `std::env::var` readers, ad-hoc parsing, defaults in code | a config struct with `#[derive(EnvConfig)]`, `Secret` for credentials, `sekvent::config::from_env()` in `main`, `MapSource` in tests |
| per-crate error enums with manual `Status`/`IntoResponse` impls | `AppError` with an `ErrorCode` and a stable `with_reason(..)`; features `error-grpc`, `error-http` provide the mappings |
| argon2/bcrypt wrappers, login checks | `sekvent::auth::PasswordHasher` and `authenticate` (constant work for unknown users, rehash on login) |
| list endpoints building filters and pagination by hand | `sekvent::db::list` (`ListParams`, column filters) with sea-orm; pools via `PoolSpec` / `PoolRegistry` |
| reqwest wrappers with retry loops, token caches | `sekvent::client::HttpClient` with a `PolicySpec`; `client::oauth2::ClientCredentials` for OAuth 2.0 |
| semaphores or token buckets guarding an upstream quota | `sekvent::resilience::RateGate`; timeouts, bulkheads and breakers from the same module |
| `tonic-build`/`prost-build` calls in `build.rs` | `sekvent_proto_build::ProtoBuild` (`messages_only` / `services_only` for split proto crates) |
| custom lifecycle, signal handling, health routes | `Runtime::builder()` units and `Server::builder()`; `/livez`, `/readyz`, `/healthz` and `grpc.health.v1` come built in |
| shared-secret checks between services | `sekvent::link` tokens (`SEKVENT_LINK_*`) with `link-axum` / `link-tonic` |
| an internal module called through a trait, or two services that talk over hand-written gRPC clients | a component (`#[sekvent::component(…, proto = "…")]`, the `.proto` `service` as its contract): `local` in one binary, `grpc` across processes by configuration (`component-grpc`); keep existing public gRPC APIs as they are and move only internal calls |

Rules while replacing:

- Delete the old code in the same step; never leave two config readers or
  two error types side by side.
- Keep `main.rs` thin: telemetry, config, runtime. Logic moves into the
  library so the coverage gate sees it.
- Keep existing environment variable names. Map them with
  `#[config(key = "...")]` or a `prefix` instead of renaming variables the
  deployment already sets.
- Do not weaken a fail-closed check while porting it, and do not introduce
  a default secret outside tests.

## 5. Keep the wire compatible

Clients and other services must not notice the migration.

- **Header names.** sekvent's own headers are fixed (`x-request-id`,
  `traceparent`, `grpc-timeout`, `idempotency-key`, `x-sekvent-subject`,
  `x-sekvent-tenant`). If the service used different names, keep accepting
  and emitting the old ones in a small adapter layer at the edge, with the
  names in one constant or config value — do not rename them on the wire.
- **Error details.** sekvent emits the standard `grpc-status-details-bin`
  trailer. If clients read a project-specific detail trailer, keep sending
  it with `sekvent::error::grpc::with_legacy_detail(status, KEY, &detail)`
  (and read it with `legacy_detail`), where `KEY` is the existing trailer
  name held in one constant. Keep existing `reason` strings verbatim.
- **HTTP error bodies.** If the old JSON error shape differs from
  `{"error": {"code": ...}}`, keep a mapping at the edge rather than
  changing what clients parse, and tell the user.
- **Routes, proto packages, field numbers, status codes** stay as they were.
  An integration test that pins the old responses is the proof.

## 6. Leave deployment alone

Do not edit Dockerfiles, compose files, Helm charts, Kubernetes manifests,
CI deploy stages or environment files. If the migration needs a deployment
change (a new variable, a probe path), list it for the user instead.
Adding the gate-only CI template (`cargo sekvent ci generate github` or
`bitbucket`) is fine when the user wants it and nothing exists at that path.

## 7. Verify and commit

```sh
cargo fmt --all                       # local
cargo sekvent gate                    # remote: fmt --check, clippy -D warnings, tests
cargo sekvent coverage                # remote: per-package line floors
cargo sekvent deps check --strict     # exit 1 on older or missing pins
```

Report exactly what ran and what failed. A crate below its coverage floor
is reported with `cargo sekvent coverage --misses <package>`, not hidden by
lowering the floor. Then make one commit on the migration branch, with
`Cargo.lock`, whose message lists the upgraded dependencies, the replaced
pieces and any wire-compatibility shims. No push unless asked.

## Pitfalls

- **Big-bang diffs.** Upgrading and replacing in one step makes every test
  failure ambiguous. Upgrade first, then replace.
- **Renaming on the wire.** A tidier header or reason name breaks every
  client that reads it. Keep the old name, configurable in one place.
- **Secrets in logs.** Ported code often logs URLs or request bodies; route
  credentials through `Secret` and untrusted text through
  `sekvent::telemetry::truncate_for_log`.
- **Fixed sleeps in ported tests.** Replace them with completion signals or
  `tokio::time::pause`; see `rust-testing`.
- **Local builds.** Compiling on the laptop because the builder is down is
  not verification; stop and report instead.
