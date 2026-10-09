# Facade features

A service depends on one package, `sekvent-api`, whose library is named
`sekvent`. Every module sits behind a Cargo feature, so a service compiles
only what it uses.

```toml
[dependencies]
sekvent-api = { workspace = true, features = ["runtime-grpc-web", "db-sqlx-postgres", "db-migrate", "auth-tokio"] }

[dev-dependencies]
sekvent-testing = { workspace = true, features = ["postgres"] }

[build-dependencies]
sekvent-proto-build = { workspace = true }
```

The dependency key must be `sekvent-api` (or a rename with
`package = "sekvent-api"`): the procedural macros find the facade through
that key. See [getting started](getting-started.md#dependency-modes) for
git, path and submodule sources.

## Defaults

`default = ["config", "error", "context", "telemetry", "runtime"]`

The defaults give you configuration, the error model, the call context,
logging and the runtime with its server. Turn them off with
`default-features = false` only for libraries that must stay small (for
example an `-api` crate that only declares a component contract).

## Module features

| Feature | Module | Internal crate | Page |
|---|---|---|---|
| `config` | `sekvent::config` (`EnvConfig`, `Secret`, sources) | `sekvent-config` | [config](modules/config.md) |
| `error` | `sekvent::error` (`AppError`, `ErrorCode`, `WireError`) | `sekvent-error` | [error](modules/error.md) |
| `context` | `sekvent::context` (`CallContext`, `Clock`, headers) | `sekvent-context` | [context](modules/context.md) |
| `telemetry` | `sekvent::telemetry` (init, access log, request id) | `sekvent-telemetry` | [telemetry](modules/telemetry.md) |
| `runtime` | `sekvent::runtime` (lifecycle, health, server, jobs, downloads) | `sekvent-runtime` | [runtime](modules/runtime.md), [server](modules/server.md), [jobs](modules/jobs.md) |
| `resilience` | `sekvent::resilience` (retry, breaker, rate gate, TTL cache) | `sekvent-resilience` | [resilience](modules/resilience.md) |
| `auth` | `sekvent::auth` (passwords, JWT, login) | `sekvent-auth` | [auth](modules/auth.md) |
| `sso` | `sekvent::sso` (browser single sign-on, Bitbucket Cloud) | `sekvent-sso` | [sso](modules/sso.md) |
| `link` | `sekvent::link` (service-to-service tokens) | `sekvent-link` | [link](modules/link.md) |
| `client` | `sekvent::client` (outbound HTTP, OAuth 2.0) | `sekvent-client` | [client](modules/client.md) |
| `db` | `sekvent::db` (pools, migrations, filters, errors) | `sekvent-db` | [db](modules/db.md) |
| `component` | `sekvent::component` plus `sekvent::{App, ComponentError, component}` | `sekvent-component` | [components](modules/components.md) |

`component` also turns on `config`, `error` and `context`.

## Integration features

| Feature | Turns on | What you get |
|---|---|---|
| `error-http` | `error` | `AppError: IntoResponse` (axum) and the JSON error envelope, `WireError` serde |
| `error-grpc` | `error` | `From<AppError> for tonic::Status` and back, gRPC error details |
| `runtime-grpc-web` | `runtime` | gRPC-Web on the server's listener, next to native gRPC and REST |
| `runtime-cron` | `runtime` | `JobSpec::cron` schedules (UTC) |
| `auth-axum` | `auth` | Bearer-token extractor for axum |
| `auth-tonic` | `auth` | Bearer-token interceptor for tonic |
| `auth-tokio` | `auth` | `hash_async`, `verify_async`, `authenticate_async` on tokio's blocking pool |
| `link-axum` | `link` | Inbound link-token middleware for axum |
| `link-tonic` | `link` | Inbound link-token interceptor for tonic |
| `db-sqlx-postgres` | `db` | sqlx Postgres pools, error classification, probes |
| `db-sqlx-mysql` | `db` | sqlx MySQL pools, error classification, probes |
| `db-sea-orm-postgres` | `db` | sea-orm connections on Postgres (includes sqlx Postgres) |
| `db-sea-orm-mysql` | `db` | sea-orm connections on MySQL (includes sqlx MySQL) |
| `db-migrate` | `db` | `migrate_on_boot` with sqlx migrations |
| `db-sea-orm-migrate` | `db` | `run_sea_orm_migrations` with sea-orm-migration |
| `db-lease` | `db` | `LeaseStore`, fencing tokens; with `runtime`, `LeaseGuard` for singleton jobs |
| `component-grpc` | `component` | The `grpc` binding and serving components over gRPC |
| `full` | everything above | Every module and integration (handy for experiments, not for services) |

## Features that combine

Some APIs need two features at once. The facade wires them for you:

| You enable | And | You also get |
|---|---|---|
| `db-*` (a backend) | `runtime` (default) | `PoolProbe` and `PoolRegistry::probes()` for readiness |
| `db-lease` + a backend | `runtime` (default) | `LeaseGuard`, a `JobGuard` that makes a job run on one instance |
| `component` | `runtime` (default) | `App::register`, which runs the components as one runtime unit |

## Things that surprise people

- **gRPC-Web is opt-in through the facade.** `runtime` alone serves native
  gRPC and REST; `ServerBuilder::grpc_web(true)` has no effect until you
  enable `runtime-grpc-web`.
- **`error-http` and `error-grpc` are already on with `runtime`.** The
  runtime needs both mappings, so with default features they are implied.
  Enable them explicitly in crates that use `error` without `runtime`.
- **`sekvent-db`'s `serde` feature** (serde for `ListParams` and
  `ColumnFilter`) has no facade switch. Add `sekvent-db` from the same source
  with `features = ["serde"]` if you need it.
- **`sekvent-testing` and `sekvent-proto-build` are not re-exported.** Add
  them as dev- and build-dependencies; see [testing](modules/testing.md) and
  [proto-build](modules/proto-build.md).

## The prelude

`use sekvent::prelude::*;` brings in the names most files need, each behind
its feature:

| Feature | Names |
|---|---|
| `config` | `EnvConfig`, `FromConfig`, `Secret` |
| `error` | `AppError`, `ErrorCode` |
| `context` | `CallContext` |
| `runtime` | `Ctx`, `JobContext`, `JobSpec`, `Runtime`, `RuntimeBuilder`, `RuntimeHandle`, `Server`, `ServerBuilder`, `ShutdownTrigger`, `Stage`, `UnitContext`, `UnitPolicy` |
| `component` | `App`, `ComponentError`, `Lifecycle` |
