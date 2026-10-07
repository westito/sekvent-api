# P8 — service essentials: server, probes, password schemes, jobs and schedules, HTTP helpers

> Status: design for review. Builds on the implemented C1 and C2 specs
> ([`component-c1.md`](component-c1.md), [`component-c2.md`](component-c2.md))
> and implements milestone **C4 (Schedule)** of
> [`docs/component-model.md`](../component-model.md). Where this document is
> more specific than the model, it wins. Every cross-agent contract is spelled
> out here so the agents of section 11 can work without asking each other.

## 1. Scope

The pieces a tonic + axum + sea-orm backend writes by hand when it serves
many gRPC services, gRPC-Web and REST on one listener, keeps several database
pools, runs periodic and on-demand background work and shares a password
store with older applications.

### In P8

| # | Item | Crates | Section |
|---|---|---|---|
| 1 | Server essentials: CORS (REST, gRPC-Web), REST body limit, global layers, request ids and access log by default | `sekvent-runtime`, `sekvent-telemetry` | 2 |
| 2 | Readiness probes per pool; permission errors become `PERMISSION_DENIED` | `sekvent-db` | 3 |
| 3 | bcrypt as a hashing scheme next to argon2id, timing-uniform; async helpers | `sekvent-auth` | 4 |
| 4 | Interval, cron and manual jobs: fixed cadence, overlap skip, misfire grace, jitter, run-now, a guard seam | `sekvent-runtime` | 5 |
| 5 | C4 Schedule: leases with fencing tokens, singleton jobs, exclusive on-demand work | `sekvent-db` (`lease`) | 6 |
| 6 | Downloads, upload guidance, a TTL cache with single flight and stale-on-error | `sekvent-runtime`, `sekvent-resilience` | 7 |
| 7 | Serving-boundary error log: `From<AppError> for Status`, `IntoResponse` and the component gRPC binding log `UNKNOWN`/`INTERNAL`/`DATA_LOSS` once with the capped source chain (target `sekvent::error`) | `sekvent-error`, `sekvent-component` | — |

JWT stays as it is (strict, `kid` required).

### Out of scope

| Item | Note |
|---|---|
| Server-wide gRPC message-size limits | tonic keeps them on each generated server (`max_decoding_message_size`, `max_encoding_message_size`); sekvent cannot set them on a service it did not build (decision 2.1.4) |
| Reserved `SEKVENT_SERVER_*`, `SEKVENT_JOB_*`, `SEKVENT_SCHEDULE_*` keys | settings come from code or app-prefixed readers (`Cors::from_config`) |
| Cron time zones other than UTC | fixed offsets mishandle DST and IANA zones need `chrono-tz`; later |
| `cargo sekvent schedule` | the subcommand stays reserved (exit 2) |
| Range requests, `ETag`, `Last-Modified` on downloads | later |
| Wildcard origins (`https://*.example.com`) | explicit lists or any origin only |
| Metrics, OpenTelemetry export | spans and events only, as in C2 |
| Queue-backed jobs, retries of failed runs | C3 (bus); a failed run waits for the next tick |

### Breaking changes

- `PasswordHasher::params()` is replaced by `PasswordHasher::scheme()`; its
  `Debug` output changes. No caller outside `sekvent-auth` exists.
- New bcrypt hashes refuse passwords longer than 72 bytes; verification
  keeps reading their first 72 bytes, as the writers of stored hashes did.
- Health endpoints no longer carry a `CallContext`; every response carries
  `x-request-id`; every request logs one event at `info` unless disabled.
- `sekvent_db::classify` maps statement and grant failures to
  `PERMISSION_DENIED` (was `INTERNAL`), with the public message
  `permission denied`; rejected connection credentials stay `INTERNAL`,
  now with reason `DB_CREDENTIALS_REJECTED`.

## 2. Server essentials

### 2.1 Decisions

1. **CORS is sekvent's own validated `Cors` value**, applied by sekvent's own
   middleware: it answers real preflights only (`Origin` plus
   `Access-Control-Request-Method`) and strips inner `Access-Control-*`
   headers so the configured policy is authoritative. Rejected: tower-http's
   `CorsLayer` (it treats every `OPTIONS` as a preflight and keeps headers set
   inside) and accepting a raw `CorsLayer` (it panics at run time on
   credentials with a wildcard and offers `mirror_request`).
2. **One stack for every protocol** (2.2); the health endpoints sit outside
   the call context and the user layers so a global auth layer can never
   fail an orchestrator's probe. Rejected: layers per protocol.
3. **The REST body limit is axum's `DefaultBodyLimit`** set on the REST
   router, so one upload route can raise it with its own
   `DefaultBodyLimit::max`. Rejected: a hard `RequestBodyLimitLayer` (a route
   could only lower it).
4. **gRPC message sizes stay per generated service.** `add_service` and
   `grpc_routes` receive finished services whose limits are inherent methods
   of tonic's generated types, not a trait. Rejected: a listener-wide frame
   guard (it could only lower tonic's own 4 MiB default, and duplicates its
   check). The recipe goes into the `ServerBuilder` docs and the skill.
5. **`sekvent-runtime` depends on `sekvent-telemetry`**, which depends on no
   sekvent crate, so the DAG gains no cycle (`sekvent-client` already does
   the same). The request id and the access log are one telemetry layer,
   `AccessLogLayer`, with exactly `RequestIdLayer`'s id rules, so there is
   one `request` span per request. Rejected: stacking `RequestIdLayer` with
   a second span layer (two spans), moving request ids into the runtime
   (other crates use telemetry alone).
6. **No reserved keys.** `Cors::from_config(source, prefix)` reads keys under
   an app-chosen prefix, the way `PoolSpec::from_config` does. Rejected:
   `SEKVENT_SERVER_*` keys (a second place to configure one server).
7. `sekvent-runtime` also gains the `sekvent-config` dependency the crate map
   already lists for it (today's manifest has none).

### 2.2 The request stack

Outermost first; `Server::router(&health)` returns exactly this:

```text
AccessLogLayer (telemetry)   keep a valid x-request-id or mint a UUID v7; `request` span; one completion event
└─ CORS                      only with .cors(…): answers preflights, sets the CORS headers of every response
   ├─ /livez /readyz /healthz, grpc.health.v1   health: no call context, no authenticator, no user layers
   └─ context layer          authenticator → CallContext (its request id is the header set above)
      └─ user layers         .layer(…); a later call wraps the earlier ones, as Router::layer
         └─ Dispatch         prefix strip; gRPC by content type (+ GrpcWebLayer) | REST
                             REST router: DefaultBodyLimit::max(rest_body_limit), route recorder
```

Consequences: a preflight never reaches authentication or handlers; the
`CallContext` request id, the `RequestId` extension and the response's
`x-request-id` are the same value; global layers can read the `CallContext`
and see every REST, gRPC and gRPC-Web request, **component gRPC calls
included** (an end-user auth layer must let link-authenticated component
paths through or be applied per service instead).

### 2.3 API (`sekvent-runtime`)

```rust
// src/server.rs — additions to ServerBuilder
impl ServerBuilder {
    /// Answer browsers' cross-origin requests (REST and gRPC-Web). Off by default.
    #[must_use] pub fn cors(self, cors: Cors) -> Self;
    /// Largest body REST extractors accept unless a route sets its own
    /// `axum::extract::DefaultBodyLimit` (default 2 MiB, axum's own default).
    #[must_use] pub fn rest_body_limit(self, bytes: usize) -> Self;
    /// Wrap every REST, gRPC and gRPC-Web request (inside the call context,
    /// outside routing; never the health endpoints). Same bounds as
    /// `axum::Router::layer`; the layer must be `Clone`.
    #[must_use] pub fn layer<L>(self, layer: L) -> Self;
    /// Whether to log one event per request (default on). Request ids and
    /// the `request` span stay on either way.
    #[must_use] pub fn access_log(self, enabled: bool) -> Self;
}
```

Layers are stored as `Arc<dyn Fn(Router) -> Router + Send + Sync>` and
applied in `router()`, which a restarted unit calls again. `Debug` shows
`cors`, `rest_body_limit`, the number of layers and `access_log`.
Validation in `bind` / `from_listener`, alongside the existing checks (each
`INVALID_ARGUMENT`): a `rest_body_limit` of 0; `Cors::validate`.

```rust
// src/cors.rs
/// Cross-origin rules for browsers calling the server (REST and gRPC-Web).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cors { /* origins, credentials, methods, allow/expose headers, max_age */ }

impl Cors {
    /// Every origin (`Access-Control-Allow-Origin: *`).
    pub fn any_origin() -> Self;
    /// Exactly these origins, normalized (2.4).
    pub fn origins<I, S>(origins: I) -> Result<Self, AppError>
    where I: IntoIterator<Item = S>, S: AsRef<str>;
    /// `Ok(None)` when `<prefix>ORIGINS` is unset or blank (CORS off).
    pub fn from_config(source: &dyn ConfigSource, prefix: &str) -> Result<Option<Self>, ConfigError>;
    /// Every key `from_config` reads under `prefix`.
    pub fn config_keys(prefix: &str) -> Vec<String>;
    #[must_use] pub fn allow_credentials(self, allowed: bool) -> Self;
    /// Replace the default methods.
    #[must_use] pub fn methods(self, methods: &[http::Method]) -> Self;
    /// Add to the default request headers.
    #[must_use] pub fn allow_headers(self, names: &[&str]) -> Self;
    /// Add to the default exposed headers.
    #[must_use] pub fn expose_headers(self, names: &[&str]) -> Self;
    #[must_use] pub fn max_age(self, max_age: Duration) -> Self;
    /// The checks `ServerBuilder::bind` runs.
    pub fn validate(&self) -> Result<(), AppError>;
}
```

### 2.4 CORS rules

Defaults:

| Setting | Default |
|---|---|
| methods | `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE` |
| request headers | `authorization`, `content-type`, `grpc-timeout`, `idempotency-key`, `traceparent`, `x-grpc-web`, `x-request-id`, `x-user-agent` |
| exposed headers | `content-disposition`, `grpc-message`, `grpc-status`, `grpc-status-details-bin`, `x-request-id` |
| max age | 1 h |
| credentials | off |

- **Origins** are `http://` or `https://` + host + optional port. They are
  normalized to the form browsers send (lowercase scheme and host, default
  port removed, no trailing slash); IPv6 hosts are parsed and written in
  canonical form (`http://[::1]:8080`). Rejected with `INVALID_ARGUMENT`,
  naming the key and the entry's index, never the entry text (a
  misconfigured value may hold anything): a path, query, fragment or
  userinfo; `*` inside a list; `null`; another scheme; an empty list.
- **Fail closed** in `validate`: credentials with `any_origin()` ("CORS
  credentials need an explicit origin list"); an invalid header name. tower-
  http's own panics are unreachable after `validate`.
- Preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`) is
  answered 200 by the layer; a disallowed origin gets no CORS headers (the
  browser blocks it). A list adds `Vary: Origin`. Any other `OPTIONS`
  request reaches the application.
- **Configured CORS is authoritative**: `Access-Control-*` headers set by
  inner layers or handlers are stripped before the layer adds its own.
- `from_config` keys: `<P>ORIGINS` (`*` or a comma list), `<P>CREDENTIALS`
  (bool, default false), `<P>MAX_AGE` (duration, default `1h`),
  `<P>ALLOW_HEADERS` and `<P>EXPOSE_HEADERS` (comma lists added to the
  defaults). Errors are `ConfigError`s naming the key; `*` with credentials
  is `Invalid { key: <P>CREDENTIALS }`. All problems are reported together.

### 2.5 Limits

- REST: `DefaultBodyLimit::max(rest_body_limit)` is layered on the merged
  REST router before it is nested, so a route-level
  `DefaultBodyLimit::max(16 * 1024 * 1024)` on an upload route wins for that
  route. Extractors answer 413. A handler that reads `Body` raw must bound it
  itself (`http_body_util::Limited`); the docs say so.
- gRPC: per service, documented on `add_service`:
  `OrdersServer::new(svc).max_decoding_message_size(16 << 20)`. The
  component service keeps its fixed 4 MiB (C2 2.4).

### 2.6 Request id, span and access log (`sekvent-telemetry`)

New module `access_log` (public, like `request_id`):

```rust
/// Request ids, the per-request span and one completion event; see the module docs.
#[derive(Debug, Clone)]
pub struct AccessLogLayer { /* quiet paths, events */ }
impl AccessLogLayer {
    /// Events on, no quiet paths; also `Default`.
    pub fn new() -> Self;
    /// Log these paths at `debug`; an entry ending in `/` is a prefix.
    #[must_use] pub fn quiet<I, S>(self, paths: I) -> Self where I: IntoIterator<Item = S>, S: Into<String>;
    /// Whether to emit the completion event (default true).
    #[must_use] pub fn events(self, enabled: bool) -> Self;
}
impl<S> tower::Layer<S> for AccessLogLayer { type Service = AccessLogService<S>; /* … */ }
pub struct AccessLogService<S> { /* … */ }   // Response<AccessLogBody<B>>
pub struct AccessLogBody<B> { /* pin-projected */ }
/// Put into a response's extensions by inner routing: the matched route template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTemplate(pub String);
```

- Ids: `request_id::ensure_request_id` becomes `pub(crate)` and is shared, so
  the rules (1–128 visible ASCII kept, anything else replaced by a UUID v7,
  repeated headers collapsed) are `RequestIdLayer`'s; the `RequestId`
  extension is inserted and the id echoed unless the handler set its own.
  `RequestIdLayer` itself is unchanged.
- Span: `info_span!("request", request_id, method, path, status = Empty,
  grpc_status = Empty, latency_ms = Empty)`, entered for the inner call and
  instrumenting its future; the empty fields are recorded at completion.
- Event, target `sekvent::access`, message `request completed`, emitted once
  when the response body ends, errors, or is dropped: `method`, `path`
  (`uri.path()`, never the query, through `truncate_for_log(path, 256)`),
  `route` (the `RouteTemplate`; for gRPC the path; empty when unmatched),
  `protocol` (`grpc-web` for `application/grpc-web*`, `grpc` for
  `application/grpc*`, else `http`), `status`, `grpc_status` (gRPC only:
  from the trailers, or from the headers of a trailers-only answer;
  `grpc-web-text` bodies are decoded to find it),
  `latency_ms` (`std::time::Instant`, to the end of the body), `aborted`
  (dropped before the end; a `HEAD` answer, which has no body, is not
  aborted) and `request_id` (on the event itself, not only on the span).
  Never headers, query, bodies, peer address or user agent; paths are
  logged, so secrets must not travel in paths.
- Level: `debug` for quiet paths; `warn` for HTTP 5xx or `grpc_status` 2,
  13 or 15 (`UNKNOWN`, `INTERNAL`, `DATA_LOSS`); `info` otherwise.
  `SEKVENT_LOG=info,sekvent::access=warn` silences routine requests.
- The runtime adds `.quiet(["/livez", "/readyz", "/healthz",
  "/grpc.health.v1.Health/"])` when the respective health routes are on, and
  a REST `route_layer` that copies axum's `MatchedPath` into `RouteTemplate`
  (prefixed with the server prefix when nested; the tests pin
  `/api/orders/{id}` for a route `/orders/{id}` under `/api`).

### 2.7 Tests

`sekvent-runtime` (router tests through `server.router(&health)` and
`oneshot`; one socket test):

- `cors`: defaults; normalization (`HTTPS://App.Example.COM:443` →
  `https://app.example.com`); every rejected origin shape; `from_config`
  (unset, blank, `*`, list, `*` + credentials, malformed bool and duration,
  extra headers, invalid header name), messages naming the key.
- Preflight to `/api/echo` → 200 with allow-origin, methods, headers and
  `access-control-max-age: 3600`, handler and authenticator not called;
  a REST answer and a gRPC-Web answer carry allow-origin and expose
  `grpc-status`; a disallowed origin gets no CORS headers; a list with
  credentials sends `access-control-allow-credentials: true` and
  `vary: origin`; `bind` fails for credentials with any origin.
- Body limit: 3 MiB JSON → 413 by default; `rest_body_limit(4 MiB)` → 200; a
  route-level `DefaultBodyLimit::max(16 MiB)` accepts 10 MiB while a
  sibling route still refuses 3 MiB; `rest_body_limit(0)` fails `bind`.
- Layers: a header-adding layer shows on REST, native gRPC and gRPC-Web
  answers and not on `/readyz`; a layer reads the `CallContext`; two layers
  record their order.
- Ids: the response `x-request-id` equals `Ctx`'s request id (REST) and a
  tonic handler's `CallContext` (gRPC); an invalid incoming id is replaced;
  health answers carry one.
- Access log (a `LogBuffer` layer installed with `set_default`): one event
  per request with every field; `/api/echo?token=s3cret` logs path
  `/api/echo` and no record contains `s3cret`; an `authorization` value
  appears nowhere; `/readyz` at `debug`; a 500 at `warn`; `grpc_status` 0
  from trailers and 5 from a trailers-only answer; `access_log(false)` →
  no event, id and span still present.
- Socket (`127.0.0.1:0`, real clock, 30 s guard): a browser-shaped preflight
  then a gRPC-Web call through the running unit.

`sekvent-telemetry` (`access_log` unit tests): protocol classification;
quiet exact and prefix paths; `RouteTemplate` read from the response;
`aborted` on drop; latency field present; `RequestId` extension set; the
existing `request_id` tests stay green.

## 3. Database probes and error classification (`sekvent-db`)

### 3.1 Decisions

1. **Feature `runtime` on `sekvent-db`** adds an optional dependency on
   `sekvent-runtime` (path, `default-features = false`), exactly as
   `sekvent-component` does; the facade's `runtime` feature turns on
   `sekvent-db?/runtime`. `sekvent-runtime` never depends on `sekvent-db`,
   so there is no cycle. Rejected: moving `DependencyProbe` into a lower
   crate (`sekvent-context` and `sekvent-error` stay light; a new crate for
   three types is churn), and glue in the facade (it only re-exports).
2. **A probe acquires a connection and pings it.** `required` follows the
   pool's `PoolSpec::required`; `lazy` only means "no connection at
   startup". A lazy optional pool that is down is reported down and never
   makes the service unready. Unconfigured optional pools get no probe.
3. **A saturated pool reuses a fresh `Up`**: when every connection is in
   use (`size() == max_connections` and `num_idle() == 0`), the probe
   reports its last `Up` while that is at most about 30 s old, instead of
   queueing behind the load, so readiness does not flap under load. Without
   a fresh `Up` it acquires as usual; a closed pool is `Down`.
4. **Statement and grant failures are classified by MySQL errno and by
   SQLSTATE `42501`**. SQLSTATE `42000` alone is **not** enough: MySQL
   reports syntax errors (1064) and many others with it. Rejected
   connection credentials (1045, `28000`, `28P01`) are the service's own
   misconfiguration, not the caller's lack of rights: `INTERNAL` with
   reason `DB_CREDENTIALS_REJECTED`, not retryable, as `sekvent-client`
   treats rejected upstream credentials.

### 3.2 API

```rust
// src/probe.rs — #[cfg(all(feature = "runtime", any(feature = "sqlx-postgres", feature = "sqlx-mysql")))]
/// Readiness probe of one pool: acquire a connection and ping it.
#[derive(Debug, Clone)]
pub struct PoolProbe { /* name, pool, required, last: Arc<Mutex<Option<ProbeStatus>>> */ }
impl PoolProbe {
    /// A required probe named `name`.
    pub fn new(name: impl Into<String>, pool: Pool) -> Self;
    /// Report it without gating readiness.
    #[must_use] pub fn optional(self) -> Self;
}
impl sekvent_runtime::DependencyProbe for PoolProbe { /* name(), required(), probe() */ }

impl PoolRegistry {
    /// One probe per connected pool, named after it, required exactly when
    /// its spec was. Unconfigured optional pools get none.
    pub fn probes(&self) -> Vec<PoolProbe>;
}
```

`PoolRegistry` keeps per-pool metadata (`required`, `lazy`) from the specs;
`insert` records a required pool. Wiring:
`registry.probes().into_iter().fold(Runtime::builder(), |b, p| b.probe(p))`.
The runtime's `probe_timeout` bounds each probe; a probe adds no timeout.

| Failure (`sqlx::Error` while acquiring or pinging) | `ProbeStatus` |
|---|---|
| `classify_connect` = `Auth` | `Down(Rejected("credentials rejected"))` |
| `classify` = `INTERNAL` / `DB_CREDENTIALS_REJECTED` | `Down(Rejected("credentials rejected"))` |
| `classify` = `PERMISSION_DENIED` | `Down(Rejected("access denied"))` |
| `PoolTimedOut` | `Down(Unreachable("no connection available in time"))` |
| other `UNAVAILABLE` (I/O, TLS, closed pool, `08xxx`) | `Down(Unreachable("unreachable"))` |
| anything else | `Down(Unreachable("ping failed"))` |

### 3.3 Classification

`classify_database` (shared by `classify` and, through the wrapped sqlx
errors, `classify_db_err`) gains, before the existing rules:

| Error | Code |
|---|---|
| MySQL errno 1044 (database access denied), 1142 (command denied on table), 1143 (column), 1227 (needs a privilege), 1370 (routine) | `PERMISSION_DENIED` |
| SQLSTATE `42501` (Postgres insufficient privilege) | `PERMISSION_DENIED` |
| MySQL errno 1045 (access denied for user), SQLSTATE `28000`, `28P01` (invalid authorization) | `INTERNAL` / `DB_CREDENTIALS_REJECTED` |
| SQLSTATE `42000` with any other errno, or none | unchanged (`INTERNAL`) |

- The errno comes from
  `error.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>().map(|e| e.number())`
  (feature `sqlx-mysql`); the table is a pure function
  `fn mysql_errno_code(number: u16) -> Option<ErrorCode>` so it is tested
  without a server.
- `classify_connect`: errno 1044 is `ConnectFailure::Auth` too (its doc
  becomes "rejected the credentials or denied access to the database").
- `public_message(ErrorCode::PermissionDenied)` is `permission denied`.
  Table and column names never reach the message.

### 3.4 Tests

- Unit, no server: `FakeDbError` with SQLSTATE `42501` →
  `PERMISSION_DENIED`; `28000`, `28P01` → `INTERNAL` /
  `DB_CREDENTIALS_REJECTED`; `42000` → `INTERNAL`; `mysql_errno_code` for each
  number above and `None` for 1064; `public_message`; `to_app_error` keeps
  the message generic.
- `probes()`: required and optional flags from specs; an unconfigured
  optional pool has none; `PoolProbe::optional`; a lazy pool on
  `127.0.0.1:1` with a 200 ms acquire timeout probes `Down(Unreachable(…))`
  (real clock, 30 s guard); with the runtime, readiness stays true while
  that optional pool is down and turns false for a required one.
- Docker (`tests/permissions.rs`, `#[ignore]`, `SEKVENT_DOCKER_TESTS=1`, the
  `tests/migrate.rs` pattern): a MySQL user granted `SELECT` on one table
  gets 1142 on another → `PERMISSION_DENIED`, message `permission denied`;
  a wrong password → `classify_connect` `Auth`; a database without grants →
  1044 → `Auth`; a Postgres role without privileges → `42501` →
  `PERMISSION_DENIED`; probes `Up` against both servers and
  `Down(Rejected("credentials rejected"))` with a wrong password; a pool of
  one held connection reuses a fresh `Up`.

## 4. Password schemes (`sekvent-auth`)

### 4.1 Decisions

1. **A `PasswordScheme` enum chooses how new hashes are written**; the
   argon2 `PasswordParams` struct stays as it is. Rejected: turning
   `PasswordParams` into the enum (breaks every struct literal and
   `PasswordParams::OWASP`).
2. **"Current" means the configured scheme**: under bcrypt, a bcrypt hash
   in the configured form at or above the configured cost is `Valid`, and
   argon2 hashes are `ValidNeedsRehash` (so a shared store converges on what
   the other readers understand). The bcrypt version tag (`2a`, `2b`, `2y`)
   never triggers a rehash.
3. **`dummy_verify` runs the configured scheme** at the configured cost, so
   `authenticate()` costs the same for unknown accounts as for real ones.
4. **Passwords over 72 bytes cannot be hashed with bcrypt**
   (`INVALID_ARGUMENT`, `PASSWORD_TOO_LONG`): bcrypt ignores the rest.
   Verification reads their first 72 bytes, as every implementation that
   wrote the stored hashes did, so those users are not locked out; under
   argon2id the match asks for a rehash (the whole password then goes into
   argon2id), under bcrypt it never does (the rehash would be refused).
   `dummy_verify` truncates the same way, so timing stays uniform.
5. **Async helpers behind a new `tokio` feature** run hashing on tokio's
   blocking pool; the default build stays wasm-compatible and spawns
   nothing.

### 4.2 API

```rust
// src/password.rs
/// How `PasswordHasher::hash` writes new hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PasswordScheme {
    /// argon2id PHC strings (`$argon2id$v=19$…`), the default.
    Argon2id(PasswordParams),
    /// bcrypt (`$2b$10$…`, or `{bcrypt}$2b$10$…` when prefixed).
    Bcrypt(BcryptParams),
}
impl Default for PasswordScheme { /* Argon2id(PasswordParams::OWASP) */ }

/// bcrypt settings for new hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BcryptParams {
    /// log2 of the rounds, 4..=MAX_BCRYPT_COST.
    pub cost: u32,
    /// Version tag written into new hashes.
    pub version: BcryptVersion,
    /// Write the `{bcrypt}` delegating-encoder prefix.
    pub prefixed: bool,
}
impl BcryptParams {
    /// `cost`, `$2b$`, no prefix.
    pub const fn new(cost: u32) -> Self;
    /// With the `{bcrypt}` prefix.
    #[must_use] pub const fn prefixed(self) -> Self;
    #[must_use] pub const fn version(self, version: BcryptVersion) -> Self;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BcryptVersion { TwoA, #[default] TwoB, TwoY }

/// Longest password bcrypt reads, in bytes; new hashes refuse longer ones.
pub const BCRYPT_MAX_PASSWORD_BYTES: usize = 72;

impl PasswordHasher {
    pub fn new(params: PasswordParams) -> Result<Self, AppError>;       // unchanged: argon2id
    pub fn bcrypt(params: BcryptParams) -> Result<Self, AppError>;      // new
    pub fn with_scheme(scheme: PasswordScheme) -> Result<Self, AppError>; // new; the two above delegate
    pub fn scheme(&self) -> PasswordScheme;                             // new; replaces params()
}
```

A service sharing a store with `{bcrypt}` readers:
`PasswordHasher::bcrypt(BcryptParams::new(10).prefixed())?`.
`bcrypt(…)` fails with `INVALID_ARGUMENT` for a cost outside `4..=14`.
`Default` stays argon2id OWASP. `hash` under bcrypt: a 16-byte salt from
`getrandom`, `bcrypt::hash_with_salt`, `format_for_version`, then the
prefix.

The bcrypt dummy is a well-formed hash at the configured cost that no
password matches, built without hashing at construction:
`format!("$2b${cost:02}${}", ".".repeat(53))` (zero salt, zero digest, the
same trick as the argon2 dummy).

### 4.3 Verification

| Stored value (password matches) | argon2id scheme | bcrypt scheme |
|---|---|---|
| `$argon2id$…` at or above the configured cost | `Valid` | `ValidNeedsRehash` |
| other argon2 variants, weaker parameters, `{argon2}` prefix | `ValidNeedsRehash` | `ValidNeedsRehash` |
| bcrypt in the configured form (prefix or not), cost ≥ configured | `ValidNeedsRehash` | `Valid` |
| bcrypt in the other form, or cost below the configured | `ValidNeedsRehash` | `ValidNeedsRehash` |
| bcrypt in any form, and a password over 72 bytes (checked on its first 72) | `ValidNeedsRehash` | `Valid` |

Every other row of the existing module docs (malformed, `$2x$`, over the
limits, empty) is unchanged: `Invalid` after exactly one dummy verification.

### 4.4 Async helpers (feature `tokio`)

```rust
impl PasswordHasher {
    /// `hash` on tokio's blocking pool.
    pub async fn hash_async(&self, password: Secret) -> Result<String, AppError>;
    /// `verify` on tokio's blocking pool; `Invalid` if the task fails.
    pub async fn verify_async(&self, password: Secret, stored: String) -> Verification;
}
/// `authenticate` on tokio's blocking pool.
pub async fn authenticate_async<U: Send + 'static>(
    hasher: &PasswordHasher, lookup: Option<(U, String, bool)>, password: Secret,
) -> LoginOutcome<U>;
```

The hasher is cloned into the task (it holds no secrets); the password
travels as `sekvent_config::Secret` and is zeroed on drop. A failed task
(`JoinError`) is logged at `error!` without payload and maps through one
private `fn join_failed(JoinError) -> AppError` (`INTERNAL`). Facade:
`auth-tokio`.

### 4.5 Tests

Round trips for both prefix forms and the three versions; every row of
4.3 under both schemes; cost 3 and 15 rejected, 4 and 14 accepted; a
73-byte password refused by `hash` (`PASSWORD_TOO_LONG`); a long password
verified for real (no dummy run, the existing `DUMMY_RUNS` counter) against
a hash written by a truncating writer, with the rehash rule of 4.3; the bcrypt
dummy is a bcrypt string at the configured cost and never matches;
`authenticate` with an unknown user runs one bcrypt dummy under the bcrypt
scheme; `scheme()`, `Debug`; the async helpers under `#[tokio::test]`;
`join_failed` with the `JoinError` of a panicking spawned task.

## 5. Jobs (`sekvent-runtime`)

### 5.1 Decisions

1. **A job is one runtime unit** registered with `RuntimeBuilder::job`; the
   unit owns the schedule loop and never fails because a run failed.
   Rejected: running jobs under `UnitPolicy::Restart` (a failed run would
   restart the schedule and lose its cadence).
2. **One API for every case**: in-process, cron, manual and singleton jobs
   share `JobSpec` and the driver; a singleton is the same job with a
   `JobGuard` (section 5.4), which `sekvent-db` implements with a lease.
3. **Fixed cadence**: in-process interval ticks are
   `start + initial_delay + k × period` on tokio's clock, whatever the runs
   take. **Singleton intervals are aligned to the Unix epoch** on the wall
   clock instead, so every instance computes the same ticks.
4. **Cron runs in UTC**, parsed with `croner` behind the `cron` feature
   (`CronParser::builder().seconds(Seconds::Optional).build().parse(p)`:
   five fields, or six with leading seconds).
5. **Time and randomness are injected**: the wall clock (`Arc<dyn Clock>`,
   default `SystemClock`) and the jitter generator (`rand::Rng`).
   `TokioWallClock` makes wall-clock schedules follow `tokio::time::pause`.
6. **Runs are at most once per tick.** A run that died with its process is
   not retried; the next tick runs.

### 5.2 API

```rust
// src/job/mod.rs (re-exported at the crate root)
/// When and how a job runs.
pub struct JobSpec { /* schedule, jitter, rng, initial_delay, timeout, misfire_grace, guard, clock, shared */ }

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Schedule {
    Interval(Duration),
    #[cfg(feature = "cron")]
    Cron(String),            // the pattern, parsed when the spec was made
    Manual,
}

impl JobSpec {
    pub fn interval(period: Duration) -> Self;
    /// `INVALID_ARGUMENT` naming the pattern when croner rejects it.
    #[cfg(feature = "cron")]
    pub fn cron(pattern: &str) -> Result<Self, AppError>;
    /// Runs only when triggered through its handle.
    pub fn manual() -> Self;
    /// Delay each scheduled run by a uniform amount in `[0, max]`.
    #[must_use] pub fn jitter(self, max: Duration) -> Self;
    /// The generator for jitter (default: seeded from the thread RNG).
    #[must_use] pub fn jitter_rng(self, rng: impl rand::Rng + Send + 'static) -> Self;
    /// Earliest start of a run after the unit starts; anchors in-process intervals.
    #[must_use] pub fn initial_delay(self, delay: Duration) -> Self;
    /// Cancel and drop a run that takes longer.
    #[must_use] pub fn timeout(self, limit: Duration) -> Self;
    /// How late a tick may start before it is skipped (default 1 min, at least 1 s).
    #[must_use] pub fn misfire_grace(self, grace: Duration) -> Self;
    /// Run on one instance at a time, as `guard` decides (section 5.4).
    #[must_use] pub fn singleton(self, guard: impl JobGuard) -> Self;
    /// The wall clock for cron and singleton ticks (default `SystemClock`).
    #[must_use] pub fn clock(self, clock: Arc<dyn Clock>) -> Self;
    /// A handle for triggers and status, usable before the job is registered.
    pub fn handle(&self) -> JobHandle;
    pub fn schedule(&self) -> &Schedule;
}

// src/runtime.rs
impl RuntimeBuilder {
    /// Register `run` as job `name` in `stage` (one unit, policy Critical).
    #[must_use]
    pub fn job<F, Fut>(self, name: impl Into<String>, stage: Stage, spec: JobSpec, run: F) -> Self
    where F: Fn(JobContext) -> Fut + Send + Sync + 'static,
          Fut: Future<Output = Result<(), AppError>> + Send + 'static;
}

/// What one run gets.
#[derive(Clone)]
pub struct JobContext { /* … */ }
impl JobContext {
    pub fn job(&self) -> &str;
    /// A fresh request id per run (UUID v7), also on the run's span.
    pub fn run_id(&self) -> &str;
    pub fn trigger(&self) -> RunTrigger;
    /// The scheduled time of a scheduled or catch-up run.
    pub fn tick(&self) -> Option<SystemTime>;
    /// The guard's fencing token, for a singleton run.
    pub fn fence(&self) -> Option<u64>;
    pub fn is_cancelled(&self) -> bool;
    pub async fn cancelled(&self);
    /// A child token for tasks the run spawns.
    pub fn cancel_token(&self) -> CancellationToken;
    pub fn cancel_reason(&self) -> Option<CancelReason>;
    /// A root `CallContext`: the run id, the run's cancellation, the timeout as deadline.
    pub fn call_context(&self) -> CallContext;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)] #[non_exhaustive]
pub enum RunTrigger { Schedule, CatchUp, Manual }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] #[non_exhaustive]
pub enum CancelReason { Shutdown, Timeout, LeaseLost }

/// Triggers and status of one job. Cheap to clone.
#[derive(Clone)]
pub struct JobHandle { /* Arc<shared state> */ }
impl JobHandle {
    /// Start a run now; resolves once it started, or with why not.
    pub async fn trigger(&self) -> Result<RunStarted, TriggerError>;
    pub fn status(&self) -> JobStatus;
    /// `None` until the spec is registered.
    pub fn name(&self) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)] #[non_exhaustive]
pub struct RunStarted { pub run_id: String, pub fence: Option<u64> }

#[derive(Debug, Clone, PartialEq, Eq)] #[non_exhaustive]
pub struct JobStatus {
    pub state: JobState,
    pub current: Option<JobRun>,
    pub last: Option<JobRunOutcome>,
    /// Next scheduled tick on the job's wall clock.
    pub next_tick: Option<SystemTime>,
    pub runs: u64,
    pub failures: u64,
    pub skipped: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)] #[non_exhaustive]
pub enum JobState { NotStarted, Idle, Running, Stopped }
#[derive(Debug, Clone, PartialEq, Eq)] #[non_exhaustive]
pub struct JobRun { pub run_id: String, pub trigger: RunTrigger, pub tick: Option<SystemTime>,
                    pub started_at: SystemTime, pub fence: Option<u64> }
#[derive(Debug, Clone, PartialEq, Eq)] #[non_exhaustive]
pub struct JobRunOutcome { pub run: JobRun, pub finished_at: SystemTime, pub error: Option<ErrorCode> }

/// Why a trigger did not start a run. `Display`: "job orders-sync is already running", …
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerError { /* job, kind */ }
impl TriggerError { pub fn job(&self) -> &str; pub fn kind(&self) -> TriggerErrorKind; }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] #[non_exhaustive]
pub enum TriggerErrorKind { AlreadyRunning, HeldElsewhere, NotRunning, GuardFailed(ErrorCode) }
impl From<TriggerError> for AppError { /* table below */ }

/// A wall clock that moves with tokio's clock: `start` plus tokio time elapsed since `new`.
#[derive(Debug, Clone)]
pub struct TokioWallClock { /* start, origin */ }
impl TokioWallClock { pub fn new(start: SystemTime) -> Self; }
impl Clock for TokioWallClock { /* … */ }

// src/reasons.rs — `pub mod reasons`
pub const JOB_ALREADY_RUNNING: &str = "JOB_ALREADY_RUNNING";
pub const JOB_HELD_ELSEWHERE: &str = "JOB_HELD_ELSEWHERE";
pub const JOB_NOT_RUNNING: &str = "JOB_NOT_RUNNING";
pub const JOB_GUARD_FAILED: &str = "JOB_GUARD_FAILED";
pub const JOB_TIMED_OUT: &str = "JOB_TIMED_OUT";
pub const JOB_PANICKED: &str = "JOB_PANICKED";
pub const GUARD_PANICKED: &str = "GUARD_PANICKED";
pub const LEASE_LOST: &str = "LEASE_LOST";
```

| `TriggerErrorKind` | `AppError` (metadata `job = <name>`) |
|---|---|
| `AlreadyRunning` | `FAILED_PRECONDITION` / `JOB_ALREADY_RUNNING` |
| `HeldElsewhere` | `FAILED_PRECONDITION` / `JOB_HELD_ELSEWHERE` |
| `NotRunning` | `UNAVAILABLE` / `JOB_NOT_RUNNING` |
| `GuardFailed(code)` | `UNAVAILABLE` / `JOB_GUARD_FAILED`, `guard_code = <code>` |

Validation in `RuntimeBuilder::build`, each `INVALID_ARGUMENT` naming the
job: a name outside `[A-Za-z0-9._:-]{1,100}`; a zero interval; a singleton
interval below 1 s or not whole milliseconds; jitter not below the interval
(interval schedules) or above 1 h (cron); a cron pattern with no next
occurrence; a zero timeout; a `misfire_grace` below 1 s; `jitter`,
`initial_delay` or `misfire_grace` set on a manual job.

### 5.3 Semantics

**Ticks.**

| Schedule | Clock | Ticks |
|---|---|---|
| `interval(p)`, no guard | tokio | `start + initial_delay + k·p`; `initial_delay` defaults to `p` |
| `interval(p)`, guard | wall | `UNIX_EPOCH + k·p`, from the first at or after `start + initial_delay` (default 0) |
| `cron(pattern)` | wall, UTC | croner's next occurrence after the previous tick, from `start + initial_delay` |
| `manual()` | — | none; triggers only |

Wall-clock waits sleep on tokio time for at most 1 min at a time and re-read
the clock, so corrections are noticed; an early wake sleeps again.

**At a tick** (after the drawn jitter; lateness = now − (tick + jitter)):

| Situation | Outcome |
|---|---|
| a run of this job is in progress here | skipped (overlap), `skipped += 1`, `debug!` |
| lateness above `misfire_grace` | skipped (misfire), `skipped += 1`, `info!`; later ticks proceed normally |
| guard answers `Ok(None)` | skipped (held elsewhere or tick already run), `debug!` |
| guard errors or panics | retried every `clamp(grace / 4, 100 ms, 5 s)` until `tick + grace`, then skipped with `warn!` (never run without the guard); a panic is a guard error with reason `GUARD_PANICKED`, never a crash of the runtime |
| guard grants a permit after `tick + grace` | the permit is released at once and the tick skipped |
| otherwise | run, `RunTrigger::Schedule` |

Every `acquire` and `last_tick` call is bounded by the tick's remaining
grace, so a hung guard cannot stall the driver.

Ticks already past the grace when the driver wakes are passed over in one
step. Once a tick is handled, every tick due strictly before now minus that
tick's jitter is passed over as well, so a clock jump or a stalled process
yields one run per stall, never a burst. Passed-over ticks (misfire and
coalescing alike) count in `skipped`; each pass-over is logged once at
`info!` with the number of ticks.
**Catch-up** (guarded jobs only; the guard remembers the last tick): after
the unit reports ready, `prev` = the latest tick at or before now; when
`guard.last_tick(job)` is older than `prev` (or `None`) and
`now − prev ≤ misfire_grace`, one run for `prev` starts at once
(`RunTrigger::CatchUp`, acquired with `tick = Some(prev)` so the dedupe of
section 6 still holds). Older missed ticks never run. In-process jobs have
no catch-up across restarts.

**Jitter** applies to scheduled runs only (not catch-up, not manual) and
never moves the grid.

**A run**: a `JobContext` with a new run id; the future is polled inside
the unit under `info_span!("job", job, run_id, trigger)` and
`catch_unwind`. Start and end are logged at `info!` with `duration_ms`;
`Err(e)` at `warn!` with `code`, `reason` and the caller-visible message
(never the source chain); a panic at `error!` with the payload text, as
units do, recorded as `INTERNAL` / `JOB_PANICKED`. Failures update
`JobStatus` and nothing else: the next tick runs. A guard permit is
released after the run, waiting at most 5 s. Before the closure is
called the run checks that its permit is not lost and its timeout has not
passed; if either has, the closure never runs. When cancellation and the
run's completion are ready at the same moment, cancellation wins.

**Cancellation**: the run's token fires first in every case.

| Reason | What happens |
|---|---|
| `Shutdown` (the stage drains) | the run is awaited until `UnitContext::stop_deadline()`; the runtime aborts the unit there |
| `Timeout` (`timeout` passed) | the run is dropped at once; outcome `DEADLINE_EXCEEDED` / `JOB_TIMED_OUT` |
| `LeaseLost` (the permit's `lost` fired) | the run is dropped at once; outcome `ABORTED` / `LEASE_LOST` |

**Triggers**: `trigger()` sends a request to the running unit. Running here →
`AlreadyRunning`; with a guard, `acquire(job, None)`, bounded by
`TRIGGER_ACQUIRE_WAIT` (5 s; past it `GuardFailed(DEADLINE_EXCEEDED)`):
`Ok(None)` → `HeldElsewhere`, `Err(e)` → `GuardFailed(e.code())`, a panic →
`GuardFailed(INTERNAL)`; otherwise the run starts (`RunTrigger::Manual`, no jitter) and `RunStarted` comes back. Before
the unit starts, while it drains and after it stopped: `NotRunning`. A
manual run never shifts the grid; a tick falling inside it is an overlap.

**The unit** reports ready at once (catch-up checks run after), uses
`UnitPolicy::Critical`, returns `Ok(())` on drain, and sets
`JobState::Stopped`.

### 5.4 Guards

```rust
// src/job/guard.rs
/// Decides whether this process may run a job, outside the process (a
/// database lease). Without a guard a job runs on every instance.
pub trait JobGuard: Send + Sync + 'static {
    /// Take `job` for one run. `tick` is the scheduled time of a scheduled or
    /// catch-up run, `None` for a manual run. `Ok(None)`: another instance
    /// holds the job, or `tick` already ran.
    fn acquire<'a>(&'a self, job: &'a str, tick: Option<SystemTime>)
        -> BoxFuture<'a, Result<Option<JobPermit>, AppError>>;
    /// The scheduled time of the last tick any instance started.
    fn last_tick<'a>(&'a self, job: &'a str) -> BoxFuture<'a, Result<Option<SystemTime>, AppError>>;
}

/// The right to run once.
pub struct JobPermit { /* fence, lost, release */ }
impl JobPermit {
    /// `lost` fires when the right is withdrawn; `release` runs once after the run.
    pub fn new(
        fence: Option<u64>,
        lost: CancellationToken,
        release: impl FnOnce() -> BoxFuture<'static, ()> + Send + 'static,
    ) -> Self;
    pub fn fence(&self) -> Option<u64>;
}
```

Dropping a `JobPermit` without releasing drops the closure; the
implementation's own `Drop` cleans up (section 6.4).

### 5.5 Tests (`tests/jobs.rs`, unit tests in `job/*.rs`)

All on `#[tokio::test(start_paused = true)]` with a runtime built
`without_signals()`; runs report through channels; times are asserted
exactly against `tokio::time::Instant`.

- Pure schedule functions: the in-process grid, epoch-aligned ticks, cron
  next and previous (UTC, five and six fields), the at-a-tick table, the
  catch-up rule.
- Cadence: a 3 s run on `interval(10s)` starts at 10, 20, 30 s;
  `initial_delay(ZERO)` starts at 0; a 25 s run skips the 20 and 30 s ticks
  and runs again at 40 s, `skipped == 2`.
- Failures: an `Err` and a panic are recorded (`failures`, `last.error`)
  and the next tick still runs; the unit never restarts.
- Jitter with a fixed generator: runs at tick + the drawn delay; the grid
  is unchanged.
- Timeout drops a never-ending run (drop guard fires) with
  `DEADLINE_EXCEEDED`; drain cancels a running job, which returns within
  the grace, and `wait()` is `Ok`.
- Triggers: idle → immediate `Manual` run; during a run →
  `AlreadyRunning`; before start and after stop → `NotRunning`; a
  `manual()` job runs only on triggers; `AppError` mapping per row.
- Cron through `TokioWallClock` (`0 */15 * * * *` fires at :00, :15, …).
- A scripted `FakeGuard`: `Ok(None)` skips; errors retry then skip at the
  grace; `lost` drops the run with `LeaseLost`; `release` runs exactly once
  per permit; catch-up from `last_tick`; `fence` reaches the context;
  `HeldElsewhere` and `GuardFailed` triggers.
- `call_context()` carries the run id, cancellation and deadline; every
  validation rule fails `build` naming the job.

## 6. Schedule C4: leases and singleton jobs (`sekvent-db` feature `lease`)

### 6.1 Decisions

1. **No new crate.** The scheduler is `sekvent-runtime`'s job driver; the
   lease is SQL and lives with the pools in `sekvent-db` (feature `lease`,
   needing a sqlx backend), next to its Postgres and MySQL container tests.
   The singleton guard, `LeaseGuard`, needs `lease` and `runtime`. Rejected:
   a `sekvent-schedule` crate (it would only glue the two), the lease inside
   the runtime (it would pull sqlx into every service).
2. **One lease row per name, expiry from the database clock** (`now()` in
   Postgres, `clock_timestamp()` for the fence check inside a caller's
   transaction, where `now()` is frozen at the transaction's start;
   `UTC_TIMESTAMP(6)` in MySQL 8, never `NOW()`, which follows the session
   time zone). Rows are never deleted, so the **fence**
   (incremented on every acquisition) only grows.
3. **Per-run leases**: a singleton job takes the lease at each tick and
   releases it after the run. The row remembers the last tick
   (`last_tick_us`), so each tick runs once even across clock skew.
4. **The app owns the schema**: `schema_sql()` returns the DDL for its own
   migrations (recommended), `ensure_schema()` runs it where the database
   user may, and `verify_schema()` fails startup clearly when it is missing.
   Rejected: a migration file shipped by sekvent (sqlx migrations live in the
   app's directory with checksums; it would be copied anyway).
5. **No reserved keys**: table name, holder label and TTL are code.

### 6.2 Table and SQL

Postgres (`{t}` is a validated identifier, default `sekvent_leases`):

```sql
CREATE TABLE IF NOT EXISTS {t} (
    name         VARCHAR(200) PRIMARY KEY,
    owner        VARCHAR(64)  NOT NULL DEFAULT '',
    holder       VARCHAR(200) NOT NULL DEFAULT '',
    fence        BIGINT       NOT NULL DEFAULT 0,
    expires_at   TIMESTAMPTZ  NOT NULL,
    acquired_at  TIMESTAMPTZ  NULL,
    last_tick_us BIGINT       NULL
);
```

MySQL 8:

```sql
CREATE TABLE IF NOT EXISTS {t} (
    name         VARCHAR(200) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
    owner        VARCHAR(64)  CHARACTER SET ascii COLLATE ascii_bin NOT NULL DEFAULT '',
    holder       VARCHAR(200) CHARACTER SET ascii NOT NULL DEFAULT '',
    fence        BIGINT       NOT NULL DEFAULT 0,
    expires_at   DATETIME(6)  NOT NULL,
    acquired_at  DATETIME(6)  NULL,
    last_tick_us BIGINT       NULL
) ENGINE = InnoDB;
```

Statements (Postgres shown; MySQL uses `?`, `UTC_TIMESTAMP(6)`,
`+ INTERVAL ? MICROSECOND` and no `RETURNING`). `owner` is a fresh random
128-bit hex string per acquisition; the TTL and tick are integer
microseconds. Parameters appear in the same order in both dialects (the
tick is bound three times), so callers bind them once.

```sql
-- acquire (tick_us is NULL for a manual or direct acquisition)
-- binds owner, holder, ttl_us, tick_us, name, tick_us, tick_us
UPDATE {t} SET owner = $1, holder = $2, fence = fence + 1, acquired_at = now(),
       expires_at = now() + $3::double precision * interval '1 microsecond',
       last_tick_us = COALESCE($4, last_tick_us)
 WHERE name = $5 AND expires_at <= now()
   AND ($6::bigint IS NULL OR last_tick_us IS NULL OR last_tick_us < $7)
RETURNING fence;
-- when the UPDATE matched nothing: create the row expired; only if this inserted it, retry the UPDATE once
INSERT INTO {t} (name, expires_at) VALUES ($1, TIMESTAMPTZ '1970-01-01 00:00:00+00')
    ON CONFLICT (name) DO NOTHING;
-- MySQL: INSERT IGNORE INTO {t} (name, expires_at) VALUES (?, '1970-01-01 00:00:00');
--   (values are validated first, so IGNORE can only skip the duplicate; the row count is 1 or 0
--   whatever the driver's found-rows flag, unlike ON DUPLICATE KEY UPDATE)
-- MySQL reads the fence after the UPDATE, in the same transaction:
--   SELECT fence FROM {t} WHERE name = ? AND owner = ?;
-- renew (binds ttl_us, name, owner)
UPDATE {t} SET expires_at = now() + $1::double precision * interval '1 microsecond'
 WHERE name = $2 AND owner = $3 AND expires_at > now();
-- release (the fence stays)
UPDATE {t} SET expires_at = now(), owner = '' WHERE name = $1 AND owner = $2;
-- fence check inside the caller's transaction, where now() would be the transaction's start
-- (MySQL: UTC_TIMESTAMP(6) and LOCK IN SHARE MODE, which MariaDB also accepts)
SELECT 1 FROM {t} WHERE name = $1 AND fence = $2 AND expires_at > clock_timestamp() FOR SHARE;
```

An acquisition (the `UPDATE` and, on MySQL, the fence read) runs in one
transaction, so the fence returned is the one this acquisition wrote.
Concurrent acquirers serialize on the row lock and the loser re-evaluates
`expires_at` and matches nothing. An acquisition that may have committed
but returned no lease (an error or a cancellation after the `UPDATE` was
sent) is released in the background by its owner id, so the row is not
left held by nobody for a full TTL. Every successful `UPDATE` changes at
least one column (a new `owner`, a later `expires_at`), so MySQL's row
count means "matched" whether the driver reports found or changed rows.
`info` reads the remaining time
in SQL (`EXTRACT(EPOCH FROM expires_at - now())`,
`TIMESTAMPDIFF(MICROSECOND, UTC_TIMESTAMP(6), expires_at)`), so no
timestamps are decoded and `sekvent-db` needs no `chrono`.

### 6.3 API

```rust
// src/lease/mod.rs — #[cfg(all(feature = "lease", any(feature = "sqlx-postgres", feature = "sqlx-mysql")))]
pub const DEFAULT_LEASE_TABLE: &str = "sekvent_leases";
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);

/// Fencing token: grows with every acquisition of a lease name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FencingToken(u64);
impl FencingToken { pub const fn new(value: u64) -> Self; pub const fn get(self) -> u64; }
impl fmt::Display for FencingToken { /* the number */ }

/// Lease rows in one table of one pool. Cheap to clone.
#[derive(Debug, Clone)]
pub struct LeaseStore { /* pool, table, holder */ }
impl LeaseStore {
    /// Table `sekvent_leases`, holder label `pid-<process id>`.
    pub fn new(pool: &Pool) -> Self;
    /// `INVALID_ARGUMENT` unless `table` is a plain identifier.
    pub fn with_table(self, table: &str) -> Result<Self, AppError>;
    /// A label shown in `info` (hostname, pod); visible ASCII, at most 200 bytes.
    pub fn with_holder(self, holder: &str) -> Result<Self, AppError>;
    /// The DDL for this backend and table.
    pub fn schema_sql(&self) -> String;
    pub async fn ensure_schema(&self) -> Result<(), AppError>;
    /// `FAILED_PRECONDITION` / `LEASE_SCHEMA_MISSING`, naming the table, when absent or incomplete
    /// (only an undefined table or column; other failures go through `to_app_error`).
    pub async fn verify_schema(&self) -> Result<(), AppError>;
    /// Take `name` unless someone holds it.
    pub async fn try_acquire(&self, name: &str, ttl: Duration) -> Result<Option<Lease>, AppError>;
    /// Take `name` for the scheduled `tick`, unless held or `tick` already ran.
    pub async fn try_acquire_tick(&self, name: &str, tick: SystemTime, ttl: Duration)
        -> Result<Option<Lease>, AppError>;
    pub async fn last_tick(&self, name: &str) -> Result<Option<SystemTime>, AppError>;
    /// Who holds `name` now; `None` when free.
    pub async fn info(&self, name: &str) -> Result<Option<LeaseInfo>, AppError>;
    /// In the caller's transaction: `ABORTED` / `LEASE_LOST` unless `name` is
    /// still held under `fence`. Takes a shared lock on the row, so no new
    /// holder can take over before the transaction ends.
    #[cfg(feature = "sqlx-postgres")]
    pub async fn check_fence_postgres(&self, conn: &mut sqlx::PgConnection, name: &str, fence: FencingToken)
        -> Result<(), AppError>;
    #[cfg(feature = "sqlx-mysql")]
    pub async fn check_fence_mysql(&self, conn: &mut sqlx::MySqlConnection, name: &str, fence: FencingToken)
        -> Result<(), AppError>;
}

#[derive(Debug, Clone, PartialEq, Eq)] #[non_exhaustive]
pub struct LeaseInfo { pub holder: String, pub fence: FencingToken, pub remaining: Duration }

/// A held lease, renewed by hand.
#[derive(Debug)]
pub struct Lease { /* store, name, owner, fence, ttl, valid_until */ }
impl Lease {
    pub fn name(&self) -> &str;
    pub fn fence(&self) -> FencingToken;
    /// Until when this process may assume it holds the lease (tokio clock).
    pub fn valid_until(&self) -> tokio::time::Instant;
    /// `ABORTED` / `LEASE_LOST` when it is no longer ours.
    pub async fn renew(&mut self) -> Result<(), AppError>;
    pub async fn release(self) -> Result<(), AppError>;
    /// Renew in the background until released or lost.
    pub fn keep_alive(self) -> HeldLease;
}

/// A lease renewed by a background task.
#[derive(Debug)]
pub struct HeldLease { /* … */ }
impl HeldLease {
    pub fn name(&self) -> &str;
    pub fn fence(&self) -> FencingToken;
    /// Fires when the lease is lost.
    pub fn lost(&self) -> CancellationToken;
    pub fn is_lost(&self) -> bool;
    /// Stop renewing, then release.
    pub async fn release(self) -> Result<(), AppError>;
}

// src/reasons.rs (feature lease) — `pub mod reasons`
pub const LEASE_LOST: &str = "LEASE_LOST";          // same value as sekvent_runtime::reasons::LEASE_LOST
pub const LEASE_SCHEMA_MISSING: &str = "LEASE_SCHEMA_MISSING";
```

Rules:

- **Names**: 1–200 bytes of `[A-Za-z0-9._:/-]`; **TTL**: 1 s to 24 h;
  **ticks**: at or after the Unix epoch. Violations are `INVALID_ARGUMENT`.
  Database failures go through `to_app_error`.
- **Validity**: an acquisition or renewal whose `UPDATE` is sent at local
  instant `t0` (taken just before the statement, not before connecting or
  beginning the transaction) that succeeds makes the lease valid until
  `t0 + ttl − ttl / 10` on tokio's clock (the database's expiry is at
  least `ttl` after `t0`; the tenth covers clock-rate drift).
- **Heartbeat** (`keep_alive`): renew every `ttl / 3`; a database error is
  retried after `max(ttl / 10, 100 ms)`; a renewal that matches no row, or
  validity passing without a successful renewal, fires `lost` and ends the
  task. The loop talks to a private `Renew` trait so it is unit-tested
  without a database.
- `keep_alive()` on a lease whose validity has already passed returns a
  `HeldLease` whose `lost` has fired, with no heartbeat.
- **Drop** of a `HeldLease` stops the heartbeat and, when a tokio runtime
  is present, spawns a best-effort release (failures at `debug!`);
  otherwise the lease simply expires. A `release()` cancelled before its
  statement completed keeps this fallback armed.
- `owner` is a capability: never logged. Logs name the lease, fence and
  holder label.

### 6.4 `LeaseGuard`: singleton jobs

```rust
// src/lease/guard.rs — also needs feature `runtime`
/// A `JobGuard` over a `LeaseStore`: one instance per tick, lost lease cancels the run.
#[derive(Debug, Clone)]
pub struct LeaseGuard { /* store, ttl, lease_name */ }
impl LeaseGuard {
    /// Lease named after the job, TTL 30 s.
    pub fn new(store: LeaseStore) -> Self;
    pub fn with_ttl(self, ttl: Duration) -> Result<Self, AppError>;
    /// Use this lease name instead of the job's, so several jobs (or direct
    /// lease users) exclude each other.
    pub fn with_lease_name(self, name: &str) -> Result<Self, AppError>;
}
impl sekvent_runtime::JobGuard for LeaseGuard { /* … */ }
```

`acquire(job, Some(tick))` is `try_acquire_tick`, `acquire(job, None)` is
`try_acquire`; a lease becomes `held = lease.keep_alive()` and the permit
`JobPermit::new(Some(fence), held.lost(), move || Box::pin(async move { … held.release() … }))`.
`last_tick` is `store.last_tick`. Manual acquisitions never touch
`last_tick_us`.

Resulting C4 semantics, matching the model: one instance runs a tick
(`last_tick_us` dedupe plus the lease); a run still going at the next tick
holds the lease, so every instance skips it (overlap); losing the lease
cancels the run; after downtime at most one catch-up within the grace;
the job receives the fence for guarded writes.

### 6.5 On-demand exclusive work

Two shapes, both documented in the skill:

```rust
// 1. A manual (or scheduled) job: the runtime drives lease, heartbeat, drain and panics.
let store = LeaseStore::new(registry.get("orders_db")?);
store.verify_schema().await?;
let spec = JobSpec::manual().singleton(LeaseGuard::new(store.clone()).with_lease_name("orders-sync")?);
let sync = spec.handle();                      // give it to the RPC service
let builder = builder.job("orders-full-sync", Stage::Workers, spec, move |cx| full_sync(cx));
// in the RPC: sync.trigger().await.map_err(AppError::from)?  → FAILED_PRECONDITION / JOB_ALREADY_RUNNING …

// 2. Direct: code that is not a job holds the lease itself.
let Some(lease) = store.try_acquire("orders-sync", DEFAULT_LEASE_TTL).await? else {
    return Err(AppError::failed_precondition("a sync is already running").with_reason("SYNC_RUNNING"));
};
let held = lease.keep_alive();
let fence = held.fence();
let lost = held.lost();                        // owned: select! must not borrow a temporary
tokio::select! {
    result = sync_all(fence) => { held.release().await?; result }
    () = lost.cancelled() => Err(AppError::new(ErrorCode::Aborted, "the sync lost its lease")),
}
```

A periodic job and an on-demand one sharing `with_lease_name("orders-sync")`
exclude each other. Writes that must not come from a stale holder call
`check_fence_postgres` / `check_fence_mysql` inside their transaction, or
store the fence in the target rows (`WHERE last_fence <= $fence`). Fenced
transactions must stay shorter than `ttl / 3`: their shared lock delays the
heartbeat.

### 6.6 Tests

- Heartbeat loop with a scripted `Renew` (paused clock): cadence `ttl / 3`;
  errors retried at `ttl / 10`; validity end fires `lost`; no-row fires
  `lost` at once; drop spawns one release.
- Validation (no server): names, TTLs, tables, holders, pre-epoch ticks;
  `schema_sql` text per backend, compared as text.
- Docker, each of Postgres and MySQL 8 (`tests/lease.rs`): `ensure_schema`
  twice; `verify_schema` on a missing table; acquire, second acquire `None`;
  fence grows across holders (after `release`, and after an expiry forced
  with `UPDATE … SET expires_at = <past>`); `renew` after a forced expiry →
  `LEASE_LOST`; tick dedupe (the same tick twice is `None` even after
  release; a later tick wins); `last_tick`; `info`; `check_fence_*` inside a
  transaction before and after a takeover; eight tasks racing → exactly one
  lease; `keep_alive` with a 3 s TTL keeps the lease across two renewals
  (observed through `info`, `await_until!`, 30 s guard); stealing the row
  fires `lost`.
- Docker, with the runtime: two runtimes on one database run a singleton
  `interval(1s)` job; over three ticks every tick runs once, fences grow,
  and a manual trigger while held answers `HeldElsewhere`; a job whose lease
  is stolen is cancelled with `LeaseLost`.

## 7. HTTP helpers

### 7.1 Downloads (`sekvent-runtime`, `src/download.rs`)

Decision: the runtime, which already owns axum, the REST routes and
`IntoResponse`. Rejected: `sekvent-error` (must stay light),
`sekvent-client` (outbound), a new crate.

```rust
/// A file sent to the client with safe headers.
/// `Debug` never prints the body.
pub struct Download { /* body, filename, disposition, content_type, cache */ }
impl Download {
    pub fn bytes(body: impl Into<Bytes>) -> Self;
    /// A streamed body; `len` sets `Content-Length` when known.
    pub fn stream(body: axum::body::Body, len: Option<u64>) -> Self;
    #[must_use] pub fn filename(self, name: &str) -> Self;
    /// `attachment` (the default) or `inline`.
    #[must_use] pub fn disposition(self, disposition: Disposition) -> Self;
    /// `INVALID_ARGUMENT` unless `type/subtype[; params]` with token characters.
    pub fn content_type(self, value: &str) -> Result<Self, AppError>;
    #[must_use] pub fn cache(self, cache: CacheControl) -> Self;
}
impl IntoResponse for Download { /* 200 + headers below */ }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Disposition { #[default] Attachment, Inline }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheControl {
    /// `no-store`.
    NoStore,
    /// `private, no-cache` for zero (the default), else `private, max-age=<s>`.
    Private(Duration),
    /// `public, max-age=<s>`.
    Public(Duration),
}

/// The media type of `head` by magic bytes; `filename` only refines ZIP and OLE containers.
pub fn sniff_content_type(head: &[u8], filename: Option<&str>) -> &'static str;
/// A name safe to store and to send back.
pub fn sanitize_filename(name: &str) -> String;
/// `attachment; filename="…"; filename*=UTF-8''…` (RFC 6266 and 8187).
pub fn content_disposition(disposition: Disposition, filename: Option<&str>) -> HeaderValue;
```

Headers: `Content-Type`, `Content-Disposition`, `Content-Length` (bytes, or
the known stream length), `Cache-Control`, `X-Content-Type-Options: nosniff`.

- **Content type**: explicit, else sniffed from a `bytes` body, else
  `application/octet-stream`. Never guessed from text: CSV, JSON and plain
  text must be set explicitly.

  | Magic | Type |
  |---|---|
  | `%PDF-` | `application/pdf` |
  | `89 50 4E 47 0D 0A 1A 0A` | `image/png` |
  | `FF D8 FF` | `image/jpeg` |
  | `GIF87a`, `GIF89a` | `image/gif` |
  | `RIFF????WEBP` | `image/webp` |
  | `????ftypavif` | `image/avif` |
  | `????ftyp` (other brands) | `video/mp4` |
  | `OggS` | `audio/ogg` |
  | `ID3` | `audio/mpeg` |
  | `1F 8B` | `application/gzip` |
  | `PK 03 04` | `application/zip`; with extension `docx`, `xlsx`, `pptx`, `odt`, `ods`, `odp` the OOXML / ODF type |
  | `D0 CF 11 E0 A1 B1 1A E1` | with extension `doc`, `xls`, `ppt` the legacy Office type, else `application/octet-stream` |

- **Inline only when safe**: `inline` is kept for `application/pdf`,
  `image/png|jpeg|gif|webp|avif`, `text/plain`, `audio/*` and `video/*`;
  anything else (HTML, SVG, XML, unknown) is sent as `attachment`
  (`debug!`), which closes stored-XSS through downloads.
- **Filename**: `sanitize_filename` keeps the part after the last `/` or
  `\`; drops control characters (U+0000–U+001F, U+007F–U+009F) and bidi
  overrides (U+202A–U+202E, U+2066–U+2069); collapses whitespace; trims
  spaces and dots at both ends; caps at 200 UTF-8 bytes on a character
  boundary, keeping an extension of up to 16 bytes; empty, `.` or `..`
  become `download`. The ASCII fallback replaces non-ASCII, `"`, `\` and `%`
  with `_`; `filename*` (UTF-8, percent-encoded except RFC 8187 attr-chars)
  is added only when it differs from the fallback. `inline` without a name
  is `inline`.

### 7.2 Uploads (guidance only)

The skill and the `ServerBuilder` docs: raise the limit on the upload route
alone (`.route_layer(DefaultBodyLimit::max(16 * 1024 * 1024))`); enable
axum's `multipart` feature in the service's manifest; read fields with
`field.chunk()` and cap each field; decide the type from the first bytes
with `sniff_content_type`, never from the client's `Content-Type`; store
under a server-chosen name and keep `sanitize_filename(original)` only as
metadata.

### 7.3 `TtlCache` (`sekvent-resilience`, `src/cache.rs`)

Decision: `sekvent-resilience`, which already has the injectable
`MonotonicClock` (TTLs need monotonic time; `TokioClock` follows
`tokio::time::pause`). Rejected: the wall `Clock` (NTP steps would stretch
or cut TTLs).

```rust
/// A small in-memory cache with per-key single flight. Cheap to clone; clones share entries.
pub struct TtlCache<K, V> { /* Arc<Inner> */ }
impl<K, V> TtlCache<K, V>
where K: Eq + Hash + Clone + Send + Sync + 'static, V: Clone + Send + Sync + 'static
{
    /// `ttl` > 0, at most 10 000 entries, no stale serving.
    pub fn new(ttl: Duration) -> Result<Self, PolicyError>;
    pub fn builder(ttl: Duration) -> TtlCacheBuilder<K, V>;
    /// A fresh value; never a stale one.
    pub fn get(&self, key: &K) -> Option<V>;
    pub fn insert(&self, key: K, value: V);
    pub fn invalidate(&self, key: &K) -> bool;
    pub fn clear(&self);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    /// The fresh value, or `load`'s, loading once per key however many wait.
    pub async fn get_or_try_insert<F, Fut>(&self, key: K, load: F) -> Result<V, AppError>
    where F: FnOnce() -> Fut, Fut: Future<Output = Result<V, AppError>>;
}
pub struct TtlCacheBuilder<K, V> { /* … */ }
impl<K, V> TtlCacheBuilder<K, V> {
    #[must_use] pub fn max_entries(self, max: usize) -> Self;
    /// Serve an expired value up to `window` past its expiry when reloading fails transiently.
    #[must_use] pub fn stale_if_error(self, window: Duration) -> Self;
    #[must_use] pub fn clock(self, clock: Arc<dyn MonotonicClock>) -> Self;
    pub fn build(self) -> Result<TtlCache<K, V>, PolicyError>;
}
```

- **Single flight**: one load per key at a time; waiters receive the
  value, or the error as `AppError::from_wire(error.to_wire())` (code,
  message, reason and metadata; `AppError` is not `Clone`), while the leader
  gets the original. A leader dropped mid-load ends its flight and one
  waiter becomes the new leader; so does a leader that panics while storing
  its result (the flight stays claimed until it is removed and its outcome
  published). Locks are never held across an await.
- **Stale on error**: only for `is_transient()` errors and within the
  window; the stale value goes to the leader and the waiters, a `warn!`
  names the code and reason (never the message), and the key is not
  reloaded again for `min(ttl, 5 s)`. Other errors are returned.
- **Capacity**: inserting at `max_entries` first drops entries past their
  stale window, then the one expiring soonest. Meant for small hot sets
  (tokens, permissions, settings), not as a general cache.
- `PolicyError`s: `cache.ttl` and `cache.max_entries` must be positive.

### 7.4 Tests

Downloads (unit, through `into_response`): every header; `Content-Length`
for bytes and a known stream; the sniff table row by row, OOXML refinement,
text never sniffed; the inline downgrade for HTML and SVG; the
`content_disposition` table (plain ASCII, quotes, backslashes, `%`, path
separators, control and bidi characters, non-ASCII with `filename*`, long
names keeping the extension, empty); invalid `content_type` strings.

`TtlCache` (paused clock): hit, miss, expiry; 20 concurrent callers and one
load (a `Notify` holds it); waiters share the error's code and reason;
dropping the leader hands over to a waiter, and so does a leader whose
value panics while being stored; stale within the window for
`UNAVAILABLE`, not for `NOT_FOUND`, not past the window; the reload pause;
eviction order; `invalidate`, `clear`; `WallClock(ManualClock)` injected.

## 8. Manifests, features and the crate map

| Manifest | Change |
|---|---|
| `crates/sekvent-runtime/Cargo.toml` | deps `sekvent-config`, `sekvent-telemetry`, `rand`, `bytes` (from dev), optional `croner`, `chrono`; feature `cron = ["dep:croner", "dep:chrono"]`; dev `tracing-subscriber` |
| `crates/sekvent-telemetry/Cargo.toml` | dep `http-body`; dev `http-body-util`, `bytes` |
| `crates/sekvent-db/Cargo.toml` | features `lease = ["dep:tokio", "dep:tokio-util", "dep:uuid"]`, `runtime = ["dep:sekvent-runtime", "dep:futures"]`; `sekvent-runtime = { path = "../sekvent-runtime", default-features = false, optional = true }` |
| `crates/sekvent-auth/Cargo.toml` | feature `tokio = ["dep:tokio"]`, optional `tokio` |
| `crates/sekvent/Cargo.toml` | `runtime` adds `"sekvent-db?/runtime"`; new `runtime-cron = ["runtime", "sekvent-runtime/cron"]`, `db-lease = ["db", "sekvent-db/lease"]`, `auth-tokio = ["auth", "sekvent-auth/tokio"]`, all three in `full` |
| root `Cargo.toml` | **none**: every dependency is already pinned (`croner = "4.0.0"`, `chrono`, `tower-http` with `cors`, `http-body`, `rand`, `uuid`, `tokio-util`) |
| `Cargo.lock` | refreshed on the Mac in phase 2 (`croner` enters the lock) |

All new dependencies use `{ workspace = true }`. Crate map rows (AGENTS.md):

| Crate | Responsibility | Depends on (sekvent) |
|---|---|---|
| `sekvent-telemetry` | tracing init, log ring buffer, request-id and access-log layers, `truncate_for_log` | — |
| `sekvent-runtime` | staged lifecycle, supervision, health, combined server (CORS, limits, layers, access log), jobs, downloads | config, error, context, telemetry |
| `sekvent-resilience` | backoff, retry budget, rate gate, timeout, bulkhead, circuit breaker, TTL cache | config, error, context |
| `sekvent-auth` | argon2id and bcrypt schemes, JWT with the time passed in; runtime-free, wasm-compatible core | error (config, context optional as today) |
| `sekvent-db` | named pools, migrations, distinct-target check, sea-orm filters, probes, leases | config, error, runtime (optional) |

`sekvent::prelude` gains `JobContext` and `JobSpec`.

## 9. Docs to update

- **README.md**: crate table rows for runtime (CORS, access log, limits,
  jobs, downloads; feature `runtime-cron`), resilience (TTL cache), auth
  (bcrypt scheme; `auth-tokio`), db (probes, leases; `db-lease`); the
  component-model paragraph says C1, C2 and C4 are implemented.
- **AGENTS.md**: the five crate-map rows of section 8; the Components
  paragraph becomes "Milestones C1, C2 and C4 are done (C4: jobs in
  `sekvent-runtime`, leases in `sekvent-db`); C3 and C5 are not started".
- **docs/component-model.md**: status block (C4 implemented, link to this
  note, `schedule` still a reserved subcommand); the Schedule section
  rewritten to the implemented rules (UTC cron, epoch-aligned singleton
  intervals, at-most-once per tick, `LeaseStore`/`LeaseGuard`); roadmap row
  `C4 (implemented) | Schedule: interval, cron and manual jobs
  (sekvent-runtime); singleton jobs over a database lease with fencing
  tokens (sekvent-db)`.
- **skills/sekvent/SKILL.md**: crate-map lines; recipes "Server: CORS,
  limits, layers, access log", "Jobs", "Singleton jobs and leases",
  "Database probes", "bcrypt-compatible passwords", "Downloads and uploads",
  "TTL cache"; pitfalls: global layers see component calls, cron is UTC,
  fenced transactions stay short, gRPC limits are per service, never put
  secrets in paths.
- **skills/sekvent-migrate/SKILL.md**, section 4 table: tower-http
  `CorsLayer` → `ServerBuilder::cors`; hand-rolled request-id and trace
  layers → built in; `tokio::time::interval` loops → `RuntimeBuilder::job`;
  lease rows with heartbeats → `LeaseStore` / `LeaseGuard`; direct `bcrypt`
  calls → `PasswordHasher::bcrypt(BcryptParams::new(10).prefixed())`;
  download handlers → `Download`; hand-made caches → `TtlCache`; DB health
  checks → `PoolRegistry::probes`. Section 5 (wire): keep `{bcrypt}` hashes
  readable by the other applications.
- **templates/agents/section.md.tmpl**: one bullet: use jobs, leases,
  probes and `Download` instead of hand-written loops, lease rows, health
  checks and download headers.
- Crate docs: `sekvent-runtime` (step 0), `sekvent-telemetry`,
  `sekvent-db` feature table, `sekvent-auth`, `sekvent-resilience`, the
  facade's feature list (their owners).

## 10. Test and coverage rules

C1/C2 rules apply: completion signals, `tokio::time::pause` for in-process
timing (jobs, heartbeat loop, cache), no fixed sleeps. Sockets bind
`127.0.0.1:0` on the real clock with lower-bound assertions and a 30 s
`timeout` guard; an address nobody listens on is `127.0.0.1:1`. Docker
tests are `#[ignore]`d, return early without `SEKVENT_DOCKER_TESTS=1` and
run in the gate and in coverage (`--include-ignored`), which is what covers
the lease SQL. Tests installing a tracing subscriber use `set_default`
(thread-local), never a global one. Every library crate stays at ≥ 95 %
lines under `--all-features` (so `cfg(not(feature = "cron"))` branches are
compiled out); the job driver reaches its guard branches through the
`FakeGuard`, the heartbeat through the scripted `Renew`, and the async
password helpers through `#[tokio::test]`.

## 11. Work split

**Step 0 (parent, alone, before fan-out):** `crates/sekvent-runtime/Cargo.toml`
as in section 8, and `crates/sekvent-runtime/src/lib.rs`: crate docs (a
"Jobs" section; the server section mentions CORS, limits, layers and the
access log), `mod cors; mod download; mod job; pub mod reasons;` and
exactly these re-exports next to the existing ones:

```rust
pub use cors::Cors;
pub use download::{CacheControl, Disposition, Download, content_disposition, sanitize_filename, sniff_content_type};
pub use job::{
    CancelReason, JobContext, JobGuard, JobHandle, JobPermit, JobRun, JobRunOutcome, JobSpec, JobState,
    JobStatus, RunStarted, RunTrigger, Schedule, TokioWallClock, TriggerError, TriggerErrorKind,
};
```

The tree does not compile until phase 1 ends; nobody builds before then.

**Phase 1:** five agents write code and tests and **do not build, lint or
run tests**.

| Agent | Writes (disjoint) | May assume |
|---|---|---|
| **A — server and access log** | `crates/sekvent-runtime/src/server.rs`, `src/cors.rs` (new), `crates/sekvent-runtime/tests/server.rs`, `tests/server_essentials.rs` (new); `crates/sekvent-telemetry/**` (manifest, `src/access_log.rs`, `src/request_id.rs`, `src/lib.rs`, tests) | step 0; section 2 exactly |
| **B — jobs** | `crates/sekvent-runtime/src/job/**` (new: `mod.rs`, `schedule.rs`, `driver.rs`, `guard.rs`, `clock.rs`), `src/runtime.rs` (`RuntimeBuilder::job`, validation in `build`), `src/unit.rs` (only if the driver needs a crate-private accessor), `src/reasons.rs` (new), `tests/jobs.rs` (new) | step 0; section 5 exactly |
| **C — database** | `crates/sekvent-db/**` (manifest, `src/error.rs`, `src/lib.rs`, `src/registry.rs`, `src/probe.rs`, `src/lease/**`, `src/reasons.rs`, `tests/permissions.rs`, `tests/probes.rs`, `tests/lease.rs`) | `JobGuard`, `JobPermit`, `DependencyProbe` exactly as in 5.4 and today (B); `RuntimeBuilder::job` and `JobSpec` for the cross-runtime test |
| **D — auth, cache, downloads** | `crates/sekvent-auth/**`; `crates/sekvent-resilience/**` (`src/cache.rs`, `src/lib.rs`); `crates/sekvent-runtime/src/download.rs` (new, unit tests inside) | step 0; sections 4 and 7 exactly |
| **E — facade and docs** | `crates/sekvent/Cargo.toml`, `crates/sekvent/src/lib.rs` (features, docs table, prelude); `README.md`; `AGENTS.md`; `docs/component-model.md`; `skills/**`; `templates/agents/section.md.tmpl` | every name in sections 2–8; the member crates' features exist as in section 8 |

Nobody edits root `Cargo.toml`; an agent that needs a dependency it does
not find there stops and reports it.

**Phase 2 (parent, Mac then rtx):** `cargo metadata --format-version 1 >
/dev/null` to refresh `Cargo.lock`; `cargo fmt --all`; `rrb run gate`;
`rrb run coverage`. **Phase 3:** failures go back to the owner of the file,
in parallel where the write sets allow; anything in
`crates/sekvent-component` or `examples/**` (for example a test that now
sees `x-request-id` or CORS headers) goes to one fix agent.

Every agent reports, besides its summary, **anything in this document that
contradicted the tree or could not be implemented as written**, with file
and reason, and stays inside its write set even when a fix elsewhere looks
obvious. Couplings to watch: C implements B's guard trait token for token;
A's REST route recorder fills telemetry's `RouteTemplate`; E's prelude and
docs name B's and D's types.

## 12. Decisions the user might override, and risks

### Decisions the user might override

1. **No new crate and no reserved keys**: jobs in `sekvent-runtime`, leases
   in `sekvent-db`, CORS read under an app prefix. Alternatives: a
   `sekvent-schedule` crate; `SEKVENT_SERVER_*` / `SEKVENT_JOB_<NAME>_*`
   keys for operators (per-job enable switches would be the first).
2. **Health outside the context and user layers**, access log on by
   default at `info` (`debug` for health).
3. **No server-level gRPC message limits**; per-service tonic settings are
   documented instead.
4. **PERMISSION_DENIED only by errno and SQLSTATE `42501`**, not by
   `42000`, which MySQL also uses for syntax errors; rejected connection
   credentials (1045, `28000`, `28P01`) are `INTERNAL` /
   `DB_CREDENTIALS_REJECTED`.
5. **bcrypt: argon2 hashes rehash to bcrypt** under the bcrypt scheme;
   version tags never trigger a rehash; new bcrypt hashes refuse passwords
   over 72 bytes, while verification reads their first 72 bytes as the
   writers of existing hashes did.
6. **`params()` replaced by `scheme()`** rather than kept as a shim.
7. **Singleton intervals aligned to the Unix epoch** (`interval(15m)` fires
   at :00, :15, :30, :45 UTC), in-process ones anchored at the unit's start.
8. **At-most-once per tick**: a run that died with its process is not
   retried, and timeouts or lost leases drop the run at once.
9. **Per-run leases** (taken at each tick, released after) rather than a
   long-lived leader lease.
10. **Inline downloads downgraded to attachment** for anything outside a
    short safe list.

### Risks

- **croner 4.0** is pinned but unused so far: its `CronDateTime`
  implementation for `chrono::DateTime<Utc>`, `Seconds::Optional` and the
  day-of-month/day-of-week combination are assumed; B's tests pin them.
- **`MatchedPath` under `nest_service`**: whether axum reports the inner
  template with or without the prefix is pinned by A's test, not assumed.
- **tonic trailers**: `grpc_status` relies on tonic putting the status in
  trailers, or in the headers of a trailers-only answer (true in 0.14).
- **MySQL details**: `UTC_TIMESTAMP(6)` versus `NOW()`, changed-rows
  counts, and MariaDB accepting `LOCK IN SHARE MODE`; the Docker suite runs
  MySQL 8 only.
- **Clock assumptions**: lease validity assumes the database's and the
  process's clocks run at nearly the same rate (the tenth of the TTL);
  ticks assume instance wall clocks within the misfire grace of each other.
- **Fenced transactions** take a shared lock on the lease row; a long one
  delays the heartbeat and can cost the lease.
- **Blocking pool**: `*_async` password helpers share tokio's blocking
  threads; a login storm still needs rate limiting.
- **Behaviour changes ripple**: tests elsewhere that compare response
  headers, or that read a `CallContext` on health routes, change (phase 3).
- **Log volume**: one `info` event per request; the `sekvent::access`
  target makes it easy to filter.
- **Build time**: croner pulls `derive_builder` and `strum` macros, only
  with `cron`.
