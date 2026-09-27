# sekvent — workspace rules

sekvent is a public, general-purpose Rust backend framework: configuration,
errors, call context, lifecycle, resilience, auth, service links, clients,
databases, a test harness and codegen helpers, plus the `cargo sekvent` CLI.
It is written from scratch. Never copy code, comments, names, hosts, labels
or business rules from any client project into this repository; describe
behaviour in your own words and use neutral examples (`billing`, `orders`).

- Code, comments, identifiers and docs are in English.
- Edition 2024, toolchain pinned in `rust-toolchain.toml`.
- Every crate has `#![forbid(unsafe_code)]` unless its manual says otherwise.

## Crate map

| Crate | Responsibility | Depends on (sekvent) |
|---|---|---|
| `sekvent-config` | `ConfigSource`, `Secret`, readers, `FromConfig`, reserved `SEKVENT_*` keys | macros (derive) |
| `sekvent-macros` | proc macros (`EnvConfig`; later `component`) | — |
| `sekvent-telemetry` | tracing init, log ring buffer, request-id layer, `truncate_for_log` | — |
| `sekvent-error` | `ErrorCode`, `AppError`, `WireError`, HTTP + gRPC mappings | — |
| `sekvent-context` | `CallContext`, `ServiceIdentity`, `Clock`, header codec | — |
| `sekvent-runtime` | staged lifecycle, supervision, health, combined server | config, error, context |
| `sekvent-resilience` | backoff, retry budget, rate gate, timeout, bulkhead, circuit breaker | config, error, context |
| `sekvent-auth` | argon2/bcrypt, JWT with the time passed in; runtime-free, wasm-compatible core | error |
| `sekvent-link` | service tokens, `TokenMap`, middleware, interceptor | config, context, error |
| `sekvent-client` | reqwest builder, resilience, context propagation, OAuth2 cache | config, context, error, resilience, telemetry |
| `sekvent-db` | named pools, migrations, distinct-target check, sea-orm filters | config, error |
| `sekvent-testing` | Postgres/MySQL containers, reaper, `await_until!` | — |
| `sekvent-proto-build` | build.rs codegen helpers | — |
| `sekvent` | facade re-exporting the above behind features | all |

Dependencies point down this table's "Depends on" column only; never add a
cycle, and keep `sekvent-error` and `sekvent-context` dependency-light.

## Dependencies

- All third-party versions are pinned once in the root `[workspace.dependencies]`;
  crates use `{ workspace = true }`. One TLS stack (rustls); never native-tls.
- A crate's heavy integrations sit behind features (`http`, `grpc`, `axum`,
  `tonic`, `sqlx-postgres`, `sea-orm-mysql`, …) so a consumer pays only for
  what it uses.

## Design rules

- Fail closed. Constructors and builders return `Result`; a missing binding,
  token or config key is a startup error that names the key, never the value.
- Never log or format secrets, tokens, passwords, database URLs or upstream
  bodies. Use `Secret` and `truncate_for_log`.
- Inject time (`Clock`) and randomness where behaviour depends on them.
- Public types that downstream code may match on are exhaustive only when the
  set is fixed by a spec (`ErrorCode`); otherwise `#[non_exhaustive]`.
- Keep `src/main.rs` files thin; logic lives in libraries.

## Tests

- Unit tests sit next to the code (`#[cfg(test)] mod tests`); integration
  tests in `tests/`. Tests are deterministic: completion signals, injected
  clocks and `tokio::time::pause`, never fixed sleeps.
- Tests that mutate process environment or global tracing live in their own
  test binary.
- Coverage gate: 95% lines per library crate (`rrb run coverage`).

## Verification (rtx via rrb)

Everything that compiles runs on rtx; `cargo fmt` (writing form) and in-tree
codegen stay on the Mac. See `~/.kodein/skills/build-on-rtx/SKILL.md`.

```sh
~/.kodein/skills/build-on-rtx/bin/rrb run check
~/.kodein/skills/build-on-rtx/bin/rrb run clippy
~/.kodein/skills/build-on-rtx/bin/rrb run gate       # fmt --check + clippy + test
~/.kodein/skills/build-on-rtx/bin/rrb run coverage
```

If rtx is unreachable, stop and report; there is no local build fallback.
