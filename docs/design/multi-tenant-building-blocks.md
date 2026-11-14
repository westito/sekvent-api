# Building blocks for multi-tenant services

Four additions: a keyed resource cache with a Postgres pool per key
(`sekvent-db`), a request scope resolved before authentication
(`sekvent-context`, `sekvent-component`), CORS origins decided by a function
(`sekvent-runtime`) and EdDSA tokens (`sekvent-auth`). Two smaller ones in
`sekvent-component` ride with the second: a further App on one runtime
(`App::register_as`) and an end-user rejection whose reason the caller
sees (`EndUserRejection`).

> Status: implemented. Builds on
> [`component-end-user.md`](component-end-user.md),
> [`end-user-extensions.md`](end-user-extensions.md),
> [`call-authorizer.md`](call-authorizer.md) and
> [`p8-service-essentials.md`](p8-service-essentials.md). Where this note is
> more specific, it wins. The user guides are
> [modules/db.md](../modules/db.md), [modules/context.md](../modules/context.md),
> [modules/components.md](../modules/components.md),
> [modules/server.md](../modules/server.md#decide-origins-with-a-function)
> and [modules/auth.md](../modules/auth.md). The note was also the work
> order: section 4 first, by the coordinator, then sections 6 to 9 in
> parallel, each inside the files section 5 gives it.

## 1. Problem

A service that serves many tenants from one process, each tenant with its
own database, needs four things the framework does not have:

1. **A pool per tenant.** `PoolRegistry` builds a fixed set of named pools
   at startup. A multi-tenant service learns its databases at run time,
   cannot open all of them at once, and must not open one twice when twenty
   requests for a cold tenant arrive together.
2. **The tenant before the user.** Which tenant a request belongs to is
   decided by its head (the host, a path segment, a selector header), and
   the end-user authenticator already needs it: sessions live in the
   tenant's database. Sign-in is an anonymous method and needs it too.
   Today the only hook that sees the request head is the authenticator,
   and it does not run for anonymous methods.
3. **Origins that are not a list.** Each tenant has its own browser origin.
   The CORS layer takes a fixed list, which cannot follow tenants that
   come and go.
4. **Tokens one side can only verify.** With HS256 every verifier holds the
   signing secret. A service that mints tokens for others to accept wants
   to give them a public key instead.

Each part is useful alone and carries nothing tenant-specific in its API:
the cache is keyed by any `K`, the scope holds any typed value, the CORS
function sees an origin, the tokens are plain JWTs.

A service of this shape also runs into two smaller gaps of
`sekvent-component`, closed with part 2:

5. **One runtime, one App.** `App::register` names its unit `components`,
   so a second App (an operator API on its own listener, with its own
   authenticator) cannot join the same runtime.
6. **Every refused credential looks the same.** The component service
   answers every `UNAUTHENTICATED` of the authenticator with one constant
   error, so a client cannot tell "your session ended, sign in again" from
   "this credential means nothing here".

## 2. Scope

### In

- `Keyed<K, V>`: one lazily created value per key, created once however
  many callers wait, never evicted while in use, evicted when idle or over
  capacity; `KeyedPools<K, V>`, the Postgres case: a pool per key and the
  value built on it, connected with ordered candidates; a runtime unit
  that evicts idle entries and closes the cache on shutdown.
- `RequestScope` on the `CallContext`, a `ScopeResolver` registered once on
  the `AppBuilder` and run by the component gRPC service before the
  end-user authenticator; `App::resolve_scope` and `admit_end_user` for
  REST routes.
- `App::register_as`: the App's runtime unit under a chosen name.
- `EndUserRejection`: an authenticator's way to show the caller a reason.
- `Cors::origin_predicate` and `Cors::or_origin_predicate`.
- `EdDsaSigningKey` and `EdDsaVerificationKeys` in `sekvent_auth::jwt`.

### Out

| Item | Note |
|---|---|
| Configuration keys | no part reads configuration; a service reads its own keys and calls the builders |
| `KeyedPools` for MySQL, sea-orm connections over a keyed pool, readiness probes per keyed pool | `Keyed<K, V>` covers other resources by hand |
| A limit on concurrent creations | each key creates on its own; bound it in the factory if needed |
| An injected `Clock` for the cache's idle time | `sekvent-db` may not depend on `sekvent-context`; 6.3, "Time" |
| A disposal hook for a `KeyedPools` value | the value is dropped, then its pool is closed; a value that owns tasks ends them in `Drop` |
| A request scope for in-process calls and for link callers | decided, 7.4 |
| Reasons on refusals other than the end-user authenticator's own (link tokens, the tenant check of 7.6) | they keep their one constant answer |
| A change to `sekvent-error` | the visible rejection needs none (7.10) |
| Asynchronous CORS decisions | decided, 8.3 |
| `BearerAuth` over EdDSA keys, Ed448, other asymmetric algorithms, JWKS endpoints | `BearerAuth` stays HS256; verify EdDSA tokens with `EdDsaVerificationKeys::verify` |

## 3. How the work is run

1. **Step 0** (section 4): the coordinator applies every change to a file
   that more than one part needs, and every manifest change, then refreshes
   `Cargo.lock`. Nothing compiles differently after it except one new,
   still empty feature.
2. **Parts 1 to 4** (sections 6 to 9) run in parallel. Each writes code,
   tests and its module pages inside its own files (section 5) and does
   not build, lint or test. A part never edits a `Cargo.toml`,
   `Cargo.lock`, `crates/sekvent/**`, `docs/features.md`, `docs/README.md`,
   `README.md`, `AGENTS.md` or `skills/**`: step 0 already did. Each part
   ends its report with what in the tree contradicted this note.
3. **Verification** (section 12): one run of the gate and of coverage over
   the combined tree, then fixes grouped by file.

Rules every part follows: workspace lints are `clippy::pedantic` with
`-D warnings`, `missing_docs` and `unreachable_pub` (every public item is
documented, crate-private items are `pub(crate)`); tests use completion
signals and `tokio::time::pause`, never a sleep; each library crate keeps
95 % line coverage; nothing logs or formats a key, a password, connect
options or a token.

## 4. Step 0

Applied by the coordinator, in this order, before any part starts.

### 4.1 `crates/sekvent-db/Cargo.toml`

In `[features]`, after the `lease = [...]` line:

```toml
# A keyed, lazily created, evicting resource cache (`Keyed`); with
# `sqlx-postgres` also `KeyedPools`, with `runtime` its eviction unit.
keyed = ["dep:tokio"]
```

### 4.2 `crates/sekvent-auth/Cargo.toml`

In `[dependencies]`, after `jsonwebtoken = { workspace = true }`:

```toml
# PEM bodies of Ed25519 keys, and the public key jsonwebtoken derives.
base64 = { workspace = true }
# Wipes private key text and bytes while a key is loaded.
zeroize = { workspace = true }
```

Both are already pinned in the root `[workspace.dependencies]`; the root
manifest does not change.

### 4.3 `crates/sekvent/Cargo.toml`

After `db-lease = ["db", "sekvent-db/lease"]`:

```toml
# sekvent-db: a keyed, lazily created, evicting resource cache (`Keyed`);
# with a Postgres backend `KeyedPools`, with `runtime` its eviction unit.
db-keyed = ["db", "sekvent-db/keyed"]
```

In `full`, after `"db-lease",`:

```toml
    "db-keyed",
```

### 4.4 `Cargo.lock`

On the Mac, after 4.1 to 4.3: `cargo metadata --format-version 1 >/dev/null`.
It adds `base64 0.23.1` and `zeroize` to the dependency list of
`sekvent-auth`; no package is added or moved. The builds run `--locked`.

### 4.5 `crates/sekvent/src/lib.rs` (crate docs only)

| Replace | With |
|---|---|
| `` `CallContext`, `ServiceIdentity`, `Clock`, header codec `` | `` `CallContext`, `ServiceIdentity`, `RequestScope`, `Clock`, header codec `` |
| `argon2id and bcrypt password hashing, JWT with injected time, login helper` | `argon2id and bcrypt password hashing, HS256 and EdDSA JWT with injected time, login helper` |
| `named pools, migrations, distinct-target check, sea-query execution, list filters, readiness probes, leases` | `named pools, a pool per key, migrations, distinct-target check, sea-query execution, list filters, readiness probes, leases` |

After the `db-lease` bullet of "Sub-features":

```rust
//! - `db-keyed`: a keyed resource cache (`db::Keyed`): one value per key,
//!   created once however many callers wait, kept while in use, evicted
//!   when idle or over capacity. On a Postgres backend also
//!   `db::KeyedPools` (a pool per key and the value built on it); with
//!   `runtime` its eviction unit.
```

### 4.6 `docs/features.md`

| Replace | With |
|---|---|
| `` `sekvent::context` (`CallContext`, `Clock`, headers) `` | `` `sekvent::context` (`CallContext`, `RequestScope`, `Clock`, headers) `` |
| `` `sekvent::auth` (passwords, JWT, login) `` | `` `sekvent::auth` (passwords, HS256 and EdDSA JWT, login) `` |
| `` `sekvent::db` (pools, migrations, filters, errors) `` | `` `sekvent::db` (pools, a pool per key, migrations, filters, errors) `` |

"Integration features", after the `db-lease` row:

```markdown
| `db-keyed` | `db` | `Keyed`: one lazily created value per key, shared while in use, evicted when idle or over capacity; with `db-sqlx-postgres` also `KeyedPools`, a Postgres pool per key and the value built on it ([db](modules/db.md#keep-one-pool-per-key-keyedpools)) |
```

"Features that combine", after the `db-lease` row:

```markdown
| `db-keyed` | `db-sqlx-postgres` | `KeyedPools`, `KeyedPoolsBuilder` and `ConnectCandidates` |
| `db-keyed` | `runtime` (default) | `Keyed::into_unit` and `KeyedPools::into_unit`: the unit that evicts idle entries and closes the cache on shutdown |
```

### 4.7 `docs/README.md`

| Replace | With |
|---|---|
| `` `CallContext`, deadlines, idempotency keys, `Clock`, header propagation `` | `` `CallContext`, deadlines, idempotency keys, the request scope, `Clock`, header propagation `` |
| `Named pools, migrations, error classification, list filters, probes, leases` | `Named pools, a pool per key, migrations, error classification, list filters, probes, leases` |
| `argon2id and bcrypt passwords, login helper, JWT with injected time` | `argon2id and bcrypt passwords, login helper, HS256 and EdDSA JWT with injected time` |
| `` Components with proto contracts, the `App` builder, bindings, gRPC serving `` | `` Components with proto contracts, the `App` builder, bindings, gRPC serving, the request scope `` |

"Design notes and examples", after the `end-user-extensions.md` row:

```markdown
| [design/multi-tenant-building-blocks.md](design/multi-tenant-building-blocks.md) | Specification of the keyed resource cache and pools per key, the request scope resolved before authentication, several Apps on one runtime, end-user rejections with a visible reason, CORS origins decided by a function, EdDSA tokens |
```

### 4.8 `README.md`

In the crate table (each text occurs once in the file):

| Replace | With |
|---|---|
| `` `CallContext`, `ServiceIdentity`, `Clock`, header codec `` | `` `CallContext`, `ServiceIdentity`, `RequestScope`, `Clock`, header codec `` |
| `argon2id and bcrypt password schemes, JWT with injected time, login helper, async hashing helpers` | `argon2id and bcrypt password schemes, HS256 and EdDSA JWT with injected time, login helper, async hashing helpers` |
| `` `db-sea-orm-migrate`, `db-lease` `` | `` `db-sea-orm-migrate`, `db-lease`, `db-keyed` `` |
| `Named pools, migrations (directory or embedded),` | `Named pools, a keyed cache with a pool per key, migrations (directory or embedded),` |

### 4.9 `AGENTS.md`

Crate map:

| Replace | With |
|---|---|
| `` `CallContext`, `ServiceIdentity`, `Clock`, header codec (peer vs external propagation) `` | `` `CallContext`, `ServiceIdentity`, `RequestScope`, `Clock`, header codec (peer vs external propagation) `` |
| `argon2id and bcrypt schemes, JWT with the time passed in; runtime-free, wasm-compatible core` | `argon2id and bcrypt schemes, HS256 and EdDSA JWT with the time passed in; runtime-free, wasm-compatible core` |
| `named pools, migrations (directory or embedded), distinct-target check, sea-query execution, sea-orm filters, probes, leases` | `named pools, a keyed resource cache with a pool per key, migrations (directory or embedded), distinct-target check, sea-query execution, sea-orm filters, probes, leases` |

"Components", in the list of specs, after
`` `docs/design/call-authorizer.md` (per-method call authorization), ``:

```markdown
`docs/design/multi-tenant-building-blocks.md` (the request scope, several
Apps on one runtime, visible end-user rejections),
```

"Components", after the bullet about the `CallAuthorizer` (three bullets):

```markdown
- The App's one `ScopeResolver` runs once for every call served over gRPC
  or gRPC-Web that no link token authenticated (end users, anonymous and
  optional-end-user methods, `SERVE_AUTH=none`), on the request head, after
  routing and the address-keyed client limit and before the end-user
  authenticator; its error is the answer, unchanged. Its `RequestScope`
  stays with the serving handler like the end user: never in headers,
  dropped by `child()` and `detached()`, and kept by a served stream until
  it ends. A tenant it sets wins over the end user's, and an end user of
  another tenant is refused.
- An end-user authenticator's `UNAUTHENTICATED` reaches the caller as one
  constant answer. Only an error made from an `EndUserRejection` adds a
  reason and a domain, both `&'static str`; never pass a message, metadata
  or anything read from the request through that path.
- `App::register` adds the unit `components`. A further App on the same
  runtime registers with `App::register_as` under a unit name of its own.
```

### 4.10 `skills/sekvent-api/SKILL.md`

Section 2, after the row starting `| Serve gRPC, gRPC-Web and REST; CORS;`:

```markdown
| Allow CORS origins decided at run time (a function instead of a list) | `docs/modules/server.md` ("Decide origins with a function") |
```

After the row starting `| Queries with sea-query (no ORM),`:

```markdown
| One Postgres pool (or other resource) per tenant or key: lazy, created once, evicted when idle (`KeyedPools`, `Keyed`) | `docs/modules/db.md` ("Keep one pool per key", "Cache any resource by key") |
```

After the row starting `| Passwords (argon2id, bcrypt), login, JWT, roles |`:

```markdown
| Tokens one service signs and others only verify (EdDSA, Ed25519 keys by `kid`) | `docs/modules/auth.md` ("Sign with a private key, verify with public keys") |
```

After the row starting `| Components (trait + proto contract,` (three
rows):

```markdown
| Resolve the tenant (or another per-request value) from the host or path before authentication and read it in handlers | `docs/modules/components.md` ("How to resolve a request scope"), `docs/modules/context.md` ("Request scope") |
| Tell a client why its credential was refused (`EndUserRejection`) | `docs/modules/components.md` ("What a rejected end user sees") |
| Run two Apps on one runtime, each behind its own listener (`App::register_as`) | `docs/modules/components.md` ("Under the runtime") |
```

Section 5, after the row starting `| Singleton jobs, leases with fencing |`:

```markdown
| A pool (or other resource) per key, e.g. per tenant | `db-keyed` (+ `db-sqlx-postgres` for `KeyedPools`) |
```

### 4.11 `skills/sekvent/SKILL.md`

**Crate map.** After the row starting
`| Password hashing (argon2id, bcrypt), JWT, login |`:

```markdown
| Tokens one service signs and others can only verify (Ed25519) | `auth` (`EdDsaSigningKey`, `EdDsaVerificationKeys`) | `auth` |
```

After the row starting `| Leases with fencing tokens, singleton jobs |`:

```markdown
| One value per key, created once, kept while in use, evicted when idle (a Postgres pool per tenant, and what is built on it) | `db` (`Keyed`, `KeyedPools`, `KeyedRef`, `ConnectCandidates`) | `db-keyed`; `db-sqlx-postgres` for `KeyedPools`; `runtime` for `into_unit` |
```

After the row starting `| A feature as a component`:

```markdown
| A tenant (or another per-request value) resolved from the request head before authentication | `component` (`ScopeResolver`, `AppBuilder::scope_resolver`, `App::resolve_scope`, `admit_end_user`), `context::RequestScope`, `cx.scoped::<T>()` | `component`; `component-grpc` to serve |
```

**"Server: CORS, limits, layers, access log".** After the bullet that ends
with `` `Cors::config_keys(prefix)` lists the keys for an env template. ``:

```markdown
- Origins known only at run time (one per tenant, say):
  `Cors::origin_predicate(|origin, head| ..)` decides every request and
  every preflight with a sync function of the `Origin` text and the request
  head; `.or_origin_predicate(..)` adds one to a list, which is checked
  first. Keep it cheap and free of I/O: a function backed by a registry
  reads an in-memory snapshot that a job refreshes. Compare the whole
  origin with known values, never with `ends_with` or `contains`. An
  allowed origin is echoed, never `*`; `null`, an `Origin` that is not text
  and a panicking function count as not allowed. CORS only decides
  response headers: a request without `Origin` is served as usual without
  them, and a request from an origin that is not allowed still reaches its
  handler. A service that ties origins to tenants checks the origin again
  where it resolves the tenant.
```

**"Components".** After the bullet starting
`- Components start in install order and stop in reverse.`:

```markdown
- One runtime can run several Apps (a public one and an operator one, each
  mounted on its own listener). The first registers with
  `app.register(builder)` (unit `components`), every other with
  `admin.register_as("admin-components", builder)`. Each App has its own
  authenticator, authorizer and scope resolver; they start and drain side
  by side, and a unit name used twice fails `RuntimeBuilder::build`.
```

Replace the bullet starting
`` - An authenticator `Err` with `UNAUTHENTICATED` reaches the client as ``
with:

```markdown
- An authenticator `Err` with `UNAUTHENTICATED` reaches the client as
  `UNAUTHENTICATED` / `invalid or expired credentials`, no reason, the same
  for every bad token; any other code passes through unchanged
  (`UNAVAILABLE` for a down session store). To let a client react to one
  case, return `Err(EndUserRejection::new("SESSION_ENDED").into())`: the
  client gets the same code and message plus that reason (and a domain
  with `.with_domain("accounts")`). Both are `&'static str`, nothing else
  of the error is sent, and each reason tells every caller one more thing
  about its credential, so keep them few.
```

**New recipe**, before the `### JWT login` heading:

````markdown
### A pool per key (`KeyedPools`)

Features `db-keyed` and `db-sqlx-postgres` (`db-migrate` for the migration
in `init`).

```rust
use sekvent::db::{ConnectCandidates, KeyedPools, run_embedded_migrations};
use sqlx::postgres::PgConnectOptions;

let pools = KeyedPools::builder(move |tenant: String| {
        let directory = directory.clone();
        async move {
            let db = directory.database_of(&tenant).await?;        // your lookup
            let with = |password: &Secret| PgConnectOptions::new()  // options, never a URL string
                .host(&db.host).port(db.port).database(&db.name)
                .username(&db.user).password(password.expose());
            Ok(match &db.previous_password {                        // tried only if the server rejects the first password
                Some(previous) => ConnectCandidates::new(with(&db.password)).or(with(previous)),
                None => ConnectCandidates::new(with(&db.password)),
            })
        }
    })
    .capacity(200)                                      // pools kept at once (default 64)
    .evict_idle_after(Some(Duration::from_secs(600)))   // unused that long: closed (the default)
    .max_connections(5)                                 // per pool (default 5); min is always 0
    .init(|_tenant, pool| async move {                  // before anything else sees the pool
        run_embedded_migrations(&pool, &MIGRATOR).await.map_err(AppError::from)
    })
    .build_value(|tenant, pool| async move {            // optional: cache what you build on the pool
        Ok(TenantState::new(tenant, pool))              // without it the value is the PgPool
    })
    .build()?;

let runtime = Runtime::builder()                        // evicts idle pools, closes all on shutdown
    .unit("tenant-pools", Stage::Infrastructure, UnitPolicy::Critical, pools.clone().into_unit());

let state = pools.get(&tenant).await?;                  // KeyedRef<TenantState>: never evicted while held
let orders = state.orders().count().await?;
```

- Twenty `get`s for a cold key connect and run `init` and `build_value`
  once; all of them get the same value or the same error. An error is
  never cached: the next `get` tries again. Creation has one deadline
  (`create_timeout`, 30 s) that covers the connector, every connect
  attempt and both hooks; a pool that connected and was not published is
  closed.
- A connector may return plain `PgConnectOptions`. With
  `ConnectCandidates` the next one is tried only after SQLSTATE `28P01`
  (the server rejected the password); any other failure ends the creation.
  All rejected: `INTERNAL` / `DB_CREDENTIALS_REJECTED`.
- At capacity a new key evicts the least recently used value nobody holds;
  when every value is held the `get` fails with `RESOURCE_EXHAUSTED` /
  `KEYED_CAPACITY`. `pools.invalidate(&key)` drops the cached value (after
  a credential change, say); holders keep theirs and its pool closes when
  the last `KeyedRef` drops. Such values are outside the capacity
  (`pools.stats().retiring`): leave headroom in the server's connection
  limit.
- `pools.peek(&key)` returns a value only if it is warm: it never connects
  and does not count as a use. `evict_idle_older_than(Duration::ZERO)`
  closes everything not in use right now.
- Hold the `KeyedRef` for as long as you use the value (a stream holds it
  for its life); do not clone the `PgPool` out of it, and give work that
  outlives the call its own `pools.get(..)`.
- Set host, port, user, password and database explicitly:
  `PgConnectOptions::new()` fills what you leave out from `PG*`
  environment variables. Never `{:?}` the options: their `Debug` shows the
  password.
- Anything else per key (a client, a parsed config): `Keyed::builder(|key|
  async move { .. }).dispose(|key, value| async move { .. }).build()?`.
````

**New recipe**, before the `### Browser single sign-on` heading:

````markdown
### Tokens between services (EdDSA)

```rust
use sekvent::auth::{Claims, EdDsaSigningKey, EdDsaVerificationKeys, NoClaims, Validation};

// The issuer holds the private key: a Secret with the PKCS#8 PEM that
// `openssl genpkey -algorithm ed25519` writes.
let signing = EdDsaSigningKey::from_pkcs8_pem("2026-11", &config.signing_key)?;
let now = clock.now_unix_millis() / 1000;
let token = signing.issue(
    &Claims::new("billing", now, 60, NoClaims {}).with_issuer("billing").with_audience("orders"),
)?;

// A verifier holds public keys by key id (two during a rotation) and can never mint.
let keys = EdDsaVerificationKeys::from_public_key_pem("2026-11", &config.billing_key)?
    .with_public_key_pem("2026-05", &config.billing_previous_key)?;
let rules = Validation::new().with_issuer("billing").with_audience("orders").with_leeway(0);
let claims: Claims<NoClaims> = keys.verify(&token, now, &rules)?;
```

- The public key is not a secret: `signing.public_key_pem()` prints the
  PEM to hand to verifiers (`openssl pkey -pubout` gives the same text).
- Same checks as `JwtKeys`: `kid` required and known, `exp`, `nbf`, `iat`,
  issuer and audience when asked. The algorithm is pinned: an HS256 token
  never passes an EdDSA verifier, nor the other way round.
- A public key of small order or in a non-canonical encoding is refused
  when it is loaded: a verifier holding one would accept forged tokens.
- Tests: `EdDsaSigningKey::from_seed("k1", &[7; 32])?` and
  `.verification_keys()` give a matching pair without key files.
````

**New recipe**, before the `## Pitfalls` heading:

````markdown
### Request scope (a tenant from the request head)

```rust
use sekvent::component::{RequestScope, ScopeResolver};

#[derive(Clone)]
struct TenantDb(sekvent::db::KeyedRef<sqlx::PgPool>);    // a type of your own per value

struct Tenants { hosts: Arc<HostTable>, pools: KeyedPools<String> }

impl ScopeResolver for Tenants {
    fn resolve<'a>(&'a self, request: &'a http::request::Parts, _cx: &'a CallContext)
        -> Pin<Box<dyn Future<Output = Result<RequestScope, AppError>> + Send + 'a>> {
        Box::pin(async move {
            let host = request.uri.host()                                           // HTTP/2
                .or_else(|| request.headers.get(http::header::HOST)?.to_str().ok()) // HTTP/1.1, may carry a port
                .ok_or_else(|| AppError::not_found("unknown site"))?;
            let tenant = self.hosts.tenant_of(host).ok_or_else(|| AppError::not_found("unknown site"))?;
            let pool = self.pools.get(&tenant).await?;
            Ok(RequestScope::new().with_tenant(tenant).with_extension(TenantDb(pool)))
        })
    }
}

builder.scope_resolver(Tenants { hosts, pools })?;       // one per App; a second is a build error

// in any served method, anonymous ones included
let TenantDb(pool) = cx.scoped::<TenantDb>().ok_or_else(|| AppError::internal("no request scope"))?;
let tenant = cx.tenant();                                // what the scope set
```

- It runs once per call served over gRPC or gRPC-Web that no link token
  authenticated, after routing and before the end-user authenticator, on
  the request head only. The authenticator reads the same scope with
  `request.extensions.get::<RequestScope>()`.
- A sync closure `Fn(&Parts, &CallContext) -> Result<RequestScope, AppError>`
  is a resolver too. Its error is the caller's answer, unchanged.
- The scope's tenant becomes `cx.tenant()` and no header can replace it;
  an end user whose own tenant differs is refused like a bad token.
- The scope lives as long as the call: until a unary handler returned,
  and until a served stream ended. `cx.child()` and `cx.detached()` keep
  the tenant and drop the scope, and nothing of it is sent to other
  services: a component called by another component works from
  `cx.tenant()`, and work that outlives the call takes its own
  `pools.get(..)` instead of a value cloned out of the scope.
- The resolver sees the path (`request.uri.path()`,
  `/<package>.<Service>/<Rpc>`): return `RequestScope::new()` for methods
  that must not open a tenant's resources before the caller is authorized
  (an operator API on the same listener).
- REST routes hold a clone of the `App`:
  `let cx = app.resolve_scope(&parts, cx).await?;` runs the same resolver.
  It does not authenticate and does not touch the request's extensions;
  after your own authentication `let cx = admit_end_user(cx, user)?;`
  applies the tenant rule.
````

**Pitfalls.** Before the `- **Tests.**` bullet:

```markdown
- **Request scope.** Resolve the tenant from the request head in the App's
  `ScopeResolver`, never from a header inside a handler and never from a
  task-local. Treat `cx.scoped::<T>()` being `None` as an error in methods
  that need it, and remember that calls from other components and
  contexts made with `child()` or `detached()` carry no scope, only
  `cx.tenant()`; work that outlives the call takes its own reference.
- **Pools per key.** Go through `KeyedPools::get` on every call and hold
  the `KeyedRef` while working; a `PgPool` cloned out of it may be closed
  under you. Build `PgConnectOptions` field by field and never print them.
  `invalidate` is for an operator's action, not for every request.
```

## 5. File ownership

A part creates or edits only the files in its row.

| Part | Creates | Edits |
|---|---|---|
| 1 keyed cache | `crates/sekvent-db/src/keyed/mod.rs`, `crates/sekvent-db/src/keyed/pools.rs`, `crates/sekvent-db/src/keyed/unit.rs`, further private modules under `crates/sekvent-db/src/keyed/` as it sees fit, `crates/sekvent-db/tests/keyed.rs`, `crates/sekvent-db/tests/keyed_pools.rs` | `crates/sekvent-db/src/lib.rs`, `crates/sekvent-db/src/reasons.rs`, `docs/modules/db.md` |
| 2 request scope, `register_as`, `EndUserRejection` | `crates/sekvent-context/src/scope.rs`, `crates/sekvent-component/src/scope.rs`, `crates/sekvent-component/tests/grpc_scope.rs` | `crates/sekvent-context/src/lib.rs`; in `crates/sekvent-component`: `src/lib.rs`, `src/app.rs`, `src/end_user.rs`, `src/error.rs`, `src/runtime.rs`, `src/server.rs`, `src/grpc/service.rs`, `tests/app.rs`, `tests/grpc_end_user.rs`, `tests/runtime.rs`; `crates/sekvent-facade-check/src/component.rs`; `docs/modules/components.md`, `docs/modules/context.md`, `docs/modules/runtime.md` |
| 3 CORS | `crates/sekvent-runtime/tests/cors_predicate.rs` | `crates/sekvent-runtime/src/cors.rs`, `docs/modules/server.md` |
| 4 EdDSA | `crates/sekvent-auth/src/jwt/eddsa.rs` | `crates/sekvent-auth/src/jwt.rs`, `crates/sekvent-auth/src/lib.rs`, `docs/modules/auth.md` |

Step 0 owns `crates/sekvent-db/Cargo.toml`, `crates/sekvent-auth/Cargo.toml`,
`crates/sekvent/Cargo.toml`, `crates/sekvent/src/lib.rs`, `Cargo.lock`,
`docs/features.md`, `docs/README.md`, `README.md`, `AGENTS.md`,
`skills/sekvent/SKILL.md` and `skills/sekvent-api/SKILL.md`.

No file is shared, and nothing outside these rows changes:

- Every crate's `lib.rs` and `reasons.rs` belong to one part.
- Part 2 owns `server.rs`, `runtime.rs` and `end_user.rs` of
  `sekvent-component`. `sekvent-runtime`'s `server.rs` and `runtime.rs` are
  different files that no part touches: the CORS surface stays on `Cors`,
  the request scope needs nothing from the runtime server (section 10,
  point 3), and `register_as` uses `RuntimeBuilder::unit` as it is.
- `sekvent-error` is not touched by any part or step: the visible
  rejection travels in the error's existing source slot (7.10).
- `crates/sekvent-component/tests/support/**` and the fixtures of the
  other crates are read by tests and edited by nobody; a test that needs a
  new helper keeps it in its own file.
- `docs/modules/runtime.md` is part 2's alone (one sentence, 7.9);
  `docs/modules/server.md` is part 3's alone.

The module pages must use the headings named in each part, because step 0
links to them.

## 6. Part 1: keyed resource cache (`sekvent-db`)

### 6.1 Placement

`Keyed<K, V>` has no database in it, yet it stays in `sekvent-db`: the
crate map allows everything it needs there (`sekvent-error`, an optional
`tokio`, and `sekvent-runtime` behind the `runtime` feature for the unit),
its first user is `KeyedPools` in the same crate, and `sekvent-resilience`,
the other candidate, may not depend on `sekvent-runtime`. It is public, so
a service can cache any resource by key.

Features: `Keyed`, `KeyedBuilder`, `KeyedRef`, `KeyedStats` need `keyed`;
`KeyedPools`, `KeyedPoolsBuilder`, `ConnectCandidates` need `keyed` and
`sqlx-postgres`; `into_unit` needs `keyed` and `runtime`. The facade feature
is `db-keyed`. The `keyed` feature brings `tokio` and nothing else:
`futures` and `tokio-util` are not available to it (`futures` only under
`runtime`).

### 6.2 Public API

```rust
// crates/sekvent-db/src/keyed/mod.rs  (feature `keyed`)

/// One lazily created value per key.
pub struct Keyed<K, V> { /* an Arc; private */ }

impl<K, V> Clone for Keyed<K, V> { /* shares the cache */ }
impl<K, V> fmt::Debug for Keyed<K, V> { /* the stats, never keys or values */ }

impl<K, V> Keyed<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// Start a cache whose values `factory` creates.
    pub fn builder<F, Fut>(factory: F) -> KeyedBuilder<K, V>
    where
        F: Fn(K) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<V, AppError>> + Send + 'static;

    /// The value of `key`, created if the cache does not hold it.
    pub async fn get(&self, key: &K) -> Result<KeyedRef<V>, AppError>;
    /// The value of `key` if the cache holds one. Never creates, never
    /// waits, and does not count as a use.
    pub fn peek(&self, key: &K) -> Option<KeyedRef<V>>;
    /// Drop the cache's value of `key`; whether there was one.
    pub async fn invalidate(&self, key: &K) -> bool;
    /// Evict every entry that was idle for `evict_idle_after`; how many.
    pub async fn evict_idle(&self) -> usize;
    /// Evict every entry that nothing refers to and that was last used at
    /// least `idle` ago; how many.
    pub async fn evict_idle_older_than(&self, idle: Duration) -> usize;
    /// Drop every value, cancel every creation and refuse further `get`s.
    pub async fn close(&self);
    pub fn is_closed(&self) -> bool;
    pub fn stats(&self) -> KeyedStats;
}

pub struct KeyedBuilder<K, V> { /* private */ }
impl<K, V> fmt::Debug for KeyedBuilder<K, V> { /* the settings, never the factory or the hook */ }

impl<K, V> KeyedBuilder<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    #[must_use] pub fn capacity(self, entries: usize) -> Self;
    #[must_use] pub fn evict_idle_after(self, idle: Option<Duration>) -> Self;
    #[must_use] pub fn create_timeout(self, timeout: Duration) -> Self;
    #[must_use] pub fn reap_interval(self, interval: Duration) -> Self;
    /// How a value is released once the cache dropped it and nothing
    /// refers to it any more.
    #[must_use] pub fn dispose<F, Fut>(self, dispose: F) -> Self
    where
        F: Fn(K, V) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static;
    pub fn build(self) -> Result<Keyed<K, V>, AppError>;
}

/// A counted reference to a cached value. While one exists the value is
/// in use: the cache never evicts it.
pub struct KeyedRef<V> { /* private */ }

impl<V> Clone for KeyedRef<V> { /* one more reference to the same value */ }
impl<V> std::ops::Deref for KeyedRef<V> { type Target = V; }
impl<V> fmt::Debug for KeyedRef<V> { /* `KeyedRef { .. }`, never the value */ }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyedStats {
    /// The most entries (warm and being created) the cache holds.
    pub capacity: usize,
    /// Entries holding a value.
    pub warm: usize,
    /// Warm entries with at least one `KeyedRef`, or a waiter about to
    /// receive one.
    pub in_use: usize,
    /// Entries whose value is being created.
    pub creating: usize,
    /// Values out of the cache that are still referenced or being
    /// disposed. Not bounded by `capacity`.
    pub retiring: usize,
    /// Creations started.
    pub creations: u64,
    /// Creations that ended without a value: an error, the deadline, a
    /// panic, or the cache closing under them.
    pub create_failures: u64,
    /// Entries the cache removed on its own, for capacity or idleness.
    pub evictions: u64,
    /// `get`s refused because the cache was full.
    pub capacity_refusals: u64,
}
```

`creations`, `create_failures`, `evictions` and `capacity_refusals` only
grow; the other fields are gauges.

```rust
// crates/sekvent-db/src/keyed/unit.rs  (features `keyed` + `runtime`)

impl<K, V> Keyed<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// The runtime unit of this cache.
    pub fn into_unit(
        self,
    ) -> impl FnMut(sekvent_runtime::UnitContext) -> futures::future::BoxFuture<'static, Result<(), AppError>>
           + Send
           + 'static;
}
```

```rust
// crates/sekvent-db/src/keyed/pools.rs  (features `keyed` + `sqlx-postgres`)

/// The connect options to try for one key, in order. Never empty.
pub struct ConnectCandidates { /* private */ }

impl ConnectCandidates {
    /// One candidate.
    pub fn new(first: sqlx::postgres::PgConnectOptions) -> Self;
    /// Also try `next`, once the server rejected the password of every
    /// candidate before it.
    #[must_use]
    pub fn or(self, next: sqlx::postgres::PgConnectOptions) -> Self;
}

impl From<sqlx::postgres::PgConnectOptions> for ConnectCandidates { /* one candidate */ }
impl fmt::Debug for ConnectCandidates { /* how many candidates, never one of them */ }

/// A Postgres pool per key, and the value built on it.
pub struct KeyedPools<K, V = sqlx::PgPool> { /* private */ }

impl<K, V> Clone for KeyedPools<K, V> { /* shares the cache */ }
impl<K, V> fmt::Debug for KeyedPools<K, V> { /* the stats only */ }

impl<K> KeyedPools<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    /// Start a cache whose pools connect with what `connector` returns for
    /// a key: `PgConnectOptions`, or `ConnectCandidates` to try in order.
    pub fn builder<C, Fut, O>(connector: C) -> KeyedPoolsBuilder<K>
    where
        C: Fn(K) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, AppError>> + Send + 'static,
        O: Into<ConnectCandidates>;
}

impl<K, V> KeyedPools<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub async fn get(&self, key: &K) -> Result<KeyedRef<V>, AppError>;
    pub fn peek(&self, key: &K) -> Option<KeyedRef<V>>;
    pub async fn invalidate(&self, key: &K) -> bool;
    pub async fn evict_idle(&self) -> usize;
    pub async fn evict_idle_older_than(&self, idle: Duration) -> usize;
    pub async fn close(&self);
    pub fn is_closed(&self) -> bool;
    pub fn stats(&self) -> KeyedStats;
    #[cfg(feature = "runtime")]
    pub fn into_unit(
        self,
    ) -> impl FnMut(sekvent_runtime::UnitContext) -> futures::future::BoxFuture<'static, Result<(), AppError>>
           + Send
           + 'static;
}

pub struct KeyedPoolsBuilder<K, V = sqlx::PgPool> { /* private */ }
impl<K, V> fmt::Debug for KeyedPoolsBuilder<K, V> { /* the settings, never the connector or a hook */ }

impl<K, V> KeyedPoolsBuilder<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    #[must_use] pub fn capacity(self, pools: usize) -> Self;
    #[must_use] pub fn evict_idle_after(self, idle: Option<Duration>) -> Self;
    #[must_use] pub fn create_timeout(self, timeout: Duration) -> Self;
    #[must_use] pub fn reap_interval(self, interval: Duration) -> Self;
    #[must_use] pub fn max_connections(self, max: u32) -> Self;
    #[must_use] pub fn acquire_timeout(self, timeout: Duration) -> Self;
    #[must_use] pub fn connection_idle_timeout(self, idle: Option<Duration>) -> Self;
    #[must_use] pub fn connection_max_lifetime(self, lifetime: Option<Duration>) -> Self;
    /// Run `init` on a new pool before anything else sees it.
    #[must_use] pub fn init<F, Fut>(self, init: F) -> Self
    where
        F: Fn(K, sqlx::PgPool) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), AppError>> + Send + 'static;
    pub fn build(self) -> Result<KeyedPools<K, V>, AppError>;
}

impl<K> KeyedPoolsBuilder<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    /// Cache what `build` makes of a key's pool instead of the pool. It
    /// runs once per creation, after `init`.
    #[must_use]
    pub fn build_value<V, F, Fut>(self, build: F) -> KeyedPoolsBuilder<K, V>
    where
        V: Send + Sync + 'static,
        F: Fn(K, sqlx::PgPool) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<V, AppError>> + Send + 'static;
}
```

`build_value` exists only while the value is still the pool, so it is
called at most once; the other setters may come before or after it. A
connector that returns plain `PgConnectOptions` compiles unchanged
(`From<PgConnectOptions> for ConnectCandidates`).

`crates/sekvent-db/src/lib.rs` gains:

```rust
#[cfg(feature = "keyed")]
mod keyed;

#[cfg(feature = "keyed")]
pub use keyed::{Keyed, KeyedBuilder, KeyedRef, KeyedStats};
#[cfg(all(feature = "keyed", feature = "sqlx-postgres"))]
pub use keyed::{ConnectCandidates, KeyedPools, KeyedPoolsBuilder};
```

and `feature = "keyed"` in the `cfg(any(..))` of `pub mod reasons`, plus
two rows in the feature table of the crate docs.

Defaults and what `build` refuses (`INVALID_ARGUMENT`, the message names
the setting):

| Setting | `Keyed` | `KeyedPools` | Refused |
|---|---|---|---|
| `capacity` | 64 | 64 | 0 |
| `evict_idle_after` | `None` (never) | `Some(10 min)` | `Some(0)` |
| `create_timeout` | 30 s | 30 s | 0 |
| `reap_interval` | 30 s | 30 s | 0 |
| `max_connections` | | 5 | 0 |
| minimum connections | | 0, fixed | |
| `acquire_timeout` | | 5 s | 0 |
| `connection_idle_timeout` | | `Some(10 min)` | `Some(0)` |
| `connection_max_lifetime` | | `Some(30 min)` | `Some(0)` |

### 6.3 Behaviour of `Keyed`

An entry of the cache is absent, being created, or warm. A value is warm
(the cache holds it) or retiring: out of the cache, and either still
referenced or being disposed.

**Time.** Idle time is measured on tokio's clock (`tokio::time::Instant`),
which the creation deadline and the unit's interval use anyway. An injected
`Clock` is not an option: it lives in `sekvent-context`, which the crate
map does not let `sekvent-db` depend on. Tests on a paused clock move it
with `tokio::time::advance`. Tests that must run on the real clock
(anything with a socket) never wait for idleness: they call
`evict_idle_older_than(Duration::ZERO)`, which evicts every entry not in
use at that instant whatever the clock says. That method is also what a
service calls to shed everything it is not using.

**`get(&key)`**

1. Closed cache: `UNAVAILABLE` / `KEYED_CLOSED`.
2. Warm: the entry is marked used now and a `KeyedRef` is returned.
3. Being created: the caller joins that creation (step 5).
4. Absent. When warm plus creating entries equal the capacity, the warm
   entry nothing refers to with the oldest last use is evicted
   (`evictions += 1`; its disposal runs in the background, the caller does
   not wait for it); when there is none, `capacity_refusals += 1` and the
   call fails with `RESOURCE_EXHAUSTED` / `KEYED_CAPACITY`. Then the entry
   becomes "being created" under a new generation, `creations += 1`, and
   the creation starts on a task of its own, which calls
   `factory(key.clone())` under `create_timeout`.
5. Every caller waiting for one creation gets its one outcome:

   | The creation ends with | Its entry | Each waiter gets |
   |---|---|---|
   | `Ok(value)` in time | becomes warm | a `KeyedRef` to the same value |
   | `Err(error)` | is removed | the caller-visible part of `error` (`AppError::from_wire(error.to_wire())`: code, message, reason, domain, metadata, retry hint, field violations; no source) |
   | the deadline | is removed, the factory's future dropped | `UNAVAILABLE` / `KEYED_CREATE_TIMEOUT` |
   | a panic of the factory | is removed | `INTERNAL` / `KEYED_FACTORY_PANICKED`; the payload is never shown |

   The last three add one to `create_failures`. **A failure is never
   cached**: after any of them the next `get` starts a fresh creation.
6. Dropping a `get` future never cancels the creation it waits for: the
   others still get its outcome, and a value nobody waited for stays warm.

**Generations.** Every creation carries a generation number, unique in the
cache, and its entry records it. When the creation ends it looks at the
map under the lock and acts on it only if the entry of its key is still
"being created" with that number and the cache is open: then a value makes
the entry warm and a failure removes it, as the table says. Otherwise the
creation is *stale*: `invalidate` or `close` removed its entry meanwhile,
and a newer creation or a newer warm value may sit under the key. A stale
creation leaves the map alone. It never replaces and never removes what is
there, and what it produced reaches nobody: a value is disposed at once
(it was never handed out), an error is logged and dropped. Its waiters go
back to step 1, so they get the key's current value, join its current
creation, start one, or fail with `KEYED_CLOSED`.

**Failure log.** A failed creation is logged once, by its own task, at
`warn`: the `code`, the `reason`, and `source`, the error's source chain
joined with `": "`. Waiters get copies without the source, so this event
is the only place the cause is kept. The chain is cut at 2 KiB on a
character boundary, the cap the serving boundary's log applies;
`truncate_for_log` itself lives in `sekvent-telemetry`, which this crate
may not depend on, so the module carries the few lines that do it and
follows the same rule: cutting shortens a text, it does not make a secret
safe, so a factory never puts one into its error's source. The message and
the metadata are left out, as in the boundary's log, and the key is never
logged (`K` has no `Debug` bound). A panic is logged with its code and
reason and without its payload.

**In use.** A value is in use while at least one `KeyedRef` to it exists; a
waiter that has not woken yet counts as one. Taking a reference with `get`
and dropping it both mark the entry used. An entry in use is never
evicted, for capacity or for idleness, whatever its age.

**`peek(&key)`** returns a reference to the warm value of `key`, and
`None` when the key is absent or being created or the cache is closed. It
takes only the map's lock: it never creates, never waits and never fails.
It does not count as a use: neither taking nor dropping a peeked reference
(or a clone of one) marks the entry used, so looking at a key on a
schedule does not keep it warm. While the reference exists the value is in
use like under any other.

**`evict_idle_older_than(idle)`** removes every warm entry that nothing
refers to and that was last used at least `idle` ago, adds them to
`evictions`, and returns their number after their disposal finished.
`Duration::ZERO` takes every entry that is not in use. **`evict_idle()`**
is `evict_idle_older_than(evict_idle_after)` and evicts nothing when that
setting is `None`.

**`invalidate(&key)`** makes the next `get` create a fresh value.

- Warm: the entry is removed. With no reference the value is disposed
  before `invalidate` returns; otherwise the holders keep it, it retires,
  and it is disposed when the last `KeyedRef` drops. `true`.
- Being created: the entry is removed and its creation is stale (see
  "Generations"): it runs to its end, at most `create_timeout`, and what it
  produces is discarded. `true`.
- Absent: `false`.

Invalidations do not count as evictions. `invalidate` is an operator's tool
(after a credential change or a restore), not a per-request one: each call
during a creation leaves one more stale creation running until it ends,
next to the fresh one of the same key.

**Capacity and what is outside it.** The capacity bounds what the cache
*holds*: warm entries plus entries being created. It does not bound what
*exists*. Outside it are

- a value that left the cache while referenced (`invalidate`, `close`),
  until its last reference drops and its disposal ends;
- a value being disposed after an eviction;
- a stale creation until it ends, and its value until it is disposed.

`stats().retiring` counts the values among these. Counting them against
the capacity instead would let one long-held reference to an invalidated
value block its key's fresh value, and this design prefers that
`invalidate` followed by `get` always works. The price: the capacity is
not a hard bound on resources. For a moment, or for as long as a holder
keeps its reference, two values of one key are alive. A consumer that
sizes something scarce from the capacity (connections against a server's
limit) adds headroom for retiring values and watches `retiring`.

**Disposal.** `dispose` runs exactly once for every value a factory
returned, with the key and the value itself, once the cache dropped it and
no reference remains. Without a hook the value is dropped. Who disposes is
decided like this:

- The cache removes entries from its map under the map's lock. An eviction
  (for capacity, `evict_idle`, `evict_idle_older_than`) only takes entries
  whose slot has a strong count of one under that lock. That test is
  exact, because a reference is only ever made under the lock or by
  cloning an existing one. `invalidate` and `close` take the entry
  whatever its count. Under the same lock each removed value is counted
  as retiring and as a running task, so a `close` that finds the map
  empty right after still waits for that disposal.
- The cache then gives up its own handle with `Arc::into_inner`. `Some`:
  it held the last one, takes the value out and disposes it. `None`:
  someone still holds the slot (after `invalidate` and `close` that is
  normal), the value retires, and the slot's `Drop`, which runs when the
  last holder lets go, disposes it. Nothing unwraps that `Option`.
- `KeyedRef::drop` marks the entry used (unless peeked) and drops its
  handle. It never reads a count and never takes the lock.

Every disposal runs on a task of its own, so dropping the future of
`evict_idle`, `evict_idle_older_than`, `invalidate` or `close` never stops
one half-way. Those four wait for the disposals they caused; a capacity
eviction, the unit's sweep and a dropped last reference do not. The cache
tracks every such task until it ends, for `close`. A last reference
dropped on a thread outside any runtime disposes on the runtime the value
was created on (its handle is kept in the slot); only once that runtime
shut down is the value just dropped. A panic in the hook is logged at
`error` and otherwise ignored; a panic while dropping the hook's future is
contained too, and the finisher still runs.

**A creation that does not publish.** At the deadline the factory's future
is dropped; on a panic it unwinds; when `close` cancels it, it is dropped.
The drop runs under the same panic guard as the polls, so a future that
panics when dropped still ends as `KEYED_CREATE_TIMEOUT` or `KEYED_CLOSED`,
never as a retried creation.
In all three whatever the factory had built so far is released by its own
`Drop`: a factory that builds in steps keeps the intermediate resource in
a guard that releases it. A value the factory did return but that cannot
be published (a stale creation) goes through `dispose`.

**`close()`**

1. marks the cache closed: every later `get` fails with `KEYED_CLOSED`,
   `peek` returns `None`;
2. removes every entry: unreferenced values go to disposal, referenced
   ones retire; every creation in flight, current or stale, is cancelled:
   the factory's future is dropped at its next await point, the creation
   counts in `create_failures`, and its waiters get `KEYED_CLOSED`;
3. waits until no creation task is running and no disposal is in progress,
   the ones it caused and the ones already running in the background, and
   returns.

It does not wait for values that are still referenced: their holders
decide, and their disposal starts when the last reference drops, on a task
nobody waits for. `stats().retiring` shows them.

`close` has no bound of its own. A cancelled creation ends at its next
await point; a disposal is user code and takes as long as it takes (a pool
waits for its connections to come back). A caller that needs a bound wraps
`close` in `tokio::time::timeout`. Giving up, or dropping the future,
abandons the wait and nothing else: the cache stays closed and the tasks
finish on their own. `close` is idempotent, and a second call waits the
same way.

**`into_unit()`** reports ready at once and every `reap_interval` evicts
like `evict_idle()`, without awaiting the disposals it starts: they are
tracked tasks of their own, so one slow disposal (a pool waiting for its
connections) never stalls the next sweep. When its stage drains it calls
`close()` under a bound: nine tenths of the time left until the unit's stop deadline
(`UnitContext::stop_deadline`, or `stage_grace` from now when the runtime
set none), so it returns before the runtime would abort it.

| `close()` | The unit |
|---|---|
| returns in time, nothing retiring | returns `Ok(())` |
| returns in time, values still referenced | logs one `warn` with their number, returns `Ok(())` |
| does not return in time | logs one `warn` with the creation tasks still running and the values still retiring, abandons the wait, returns `Ok(())` |

It never fails on its own. Register it in `Stage::Infrastructure` with
`UnitPolicy::Critical`: infrastructure drains after ingress and
components, so the references held by calls and streams are gone by then
and the values are released after the last call. A closed cache stays
closed, so the unit is not meant to restart.

```rust
let clients = Keyed::builder(|region: String| async move { connect(&region).await })
    .capacity(16)
    .evict_idle_after(Some(Duration::from_mins(15)))
    .dispose(|_region, client| async move { client.shutdown().await })
    .build()?;
let client = clients.get(&region).await?;      // KeyedRef<Client>
```

Notes for the implementation, none of them contract:

- One `std::sync::Mutex` over the map, the closed flag and the generation
  counter, never held across an await (poisoning is ignored with
  `PoisonError::into_inner`, as elsewhere in the crate).
- The creation runs on a spawned task. Its outcome (the published slot, a
  failure as a `WireError`, or "stale") goes to the waiters through a
  `tokio::sync::watch` channel. The task alone owns the sender and drops
  it right after its one send. The "being created" entry holds a receiver
  that later callers clone, and it is replaced by the warm entry, which
  holds the slot and nothing else, before the send. So only the map, live
  references and waiters that have not woken yet ever hold a slot, and its
  count is back at one when the last of them is gone: no channel outlives
  its creation.
- A guard in the creation task covers the unwind and the cancellation: it
  settles the entry by the generation rule and sends the outcome
  (`KEYED_FACTORY_PANICKED` when `std::thread::panicking()`, "stale"
  otherwise), so no waiter hangs. The key's own code (`Clone`, `Hash`,
  `Eq`) may panic too: `get` runs it before inserting the "being created"
  entry, and when it panics while the creation settles, the creation
  removes its entry by generation alone, releases what it made (value
  dropped, finisher run; the hook needs the key) and fails its waiters
  with `KEYED_FACTORY_PANICKED`. A channel closed without an outcome is
  that same terminal failure for a waiter, never a retry, and `get`
  replaces an entry whose creation vanished, so no key stays wedged and
  no waiter spins. `close` cancels through a signal the
  task selects on (`tokio::select!` with a `watch` or a `Notify`).
- The slot holds the value in an `Option` (the cache takes it out, and
  `Drop` then finds `None`), the last use as an atomic offset from the
  cache's first instant, so dropping a `KeyedRef` takes no lock, and a
  boxed closure that captured the key and the shared disposer, so
  `KeyedRef<V>` needs no `K`.
- Creation and disposal tasks are counted in the shared state with a
  `tokio::sync::Notify` for "none left"; each task holds a guard that
  counts it out on every exit, a panic included. `retiring` is a gauge
  raised when a value leaves the map or is born stale and lowered when its
  disposal ends.
- `KeyedPools` reuses all of this through a crate-private constructor
  whose factory also returns a per-value finisher that runs after the
  value was dropped at disposal (6.4).

### 6.4 Behaviour of `KeyedPools`

One cache holds, per key, the value built on the key's pool, and keeps the
pool beside it. Its factory, all of it under the one `create_timeout`:

1. `connector(key)` returns the candidates; its error is the creation's
   error.
2. For each candidate in order,
   `sqlx::pool::PoolOptions::<sqlx::Postgres>::new()` with
   `max_connections`, `min_connections(0)`, `acquire_timeout`,
   `idle_timeout`, `max_lifetime`, then `connect_with(candidate)`: one
   connection is opened, so wrong credentials fail the creation instead of
   the first query.

   | The attempt | Next |
   |---|---|
   | connects | step 3 |
   | the server rejects the password (SQLSTATE `28P01`) and a candidate is left | that candidate |
   | fails in any other way, or with `28P01` on the last candidate | the creation fails, as the table below says |

3. `init(key, pool.clone())`, when set. Its error is the creation's error.
4. `build_value(key, pool.clone())`, when set, returns the value; without
   it the value is the pool. Its error is the creation's error.
5. The value is published.

**Only a rejected password moves on.** `28P01` (`invalid_password`) is what
Postgres answers when a password method refused this user and password:
another credential may be accepted, which is the case the list exists for
(a password in rotation). `28000` (`invalid_authorization_specification`)
gets no fallback: the server refused the login by rule (no matching
host-based rule, a role that may not log in, a method that is not a
password), another password does not change that, and working through the
list would hide the misconfiguration behind a later candidate and
multiply failed logins. An unreachable server, a timeout, a missing
database or anything else is not about the credential and is never
retried with another one. The decision reads the server's SQLSTATE
(`sekvent_db::sqlstate`); a proxy that reports a rejected password under
another code gets no fallback. A creation that connected with a candidate
other than the first logs one `info` event with the candidate's position
and the number of candidates, never an option.

Each attempt is bounded by `acquire_timeout`, which `connect_with` applies
itself (it retries a refused connection until then), and the attempts,
`init` and `build_value` together by `create_timeout`. Keep the first
below the second, or an unreachable server is reported as
`KEYED_CREATE_TIMEOUT` instead of a connect timeout; both are
`UNAVAILABLE`.

What a failed creation answers, with `public_message(code)` as its message:

| What failed | Code | Reason |
|---|---|---|
| the connector, `init`, `build_value` | its own | its own |
| connect, `ConnectFailure::Unreachable` or `Timeout` | `UNAVAILABLE` | none |
| connect, `ConnectFailure::Auth`: `28P01` on the last candidate, `28000` on any | `INTERNAL` | `DB_CREDENTIALS_REJECTED` |
| connect, `ConnectFailure::BadUrl` or `Other` | `INTERNAL` | none |

**What is logged.** One `warn` per failed creation, from `Keyed` (6.3). An
error of the connector or of a hook is logged with its own source chain.
A connect failure is classified with `classify_connect`, and the driver's
error is **neither kept as the source nor logged**: that is the crate's
standing rule for connect errors, whose text names users, hosts and
databases and can quote what it was given. In its place the error carries
a small source of this module that renders only the class
(`ConnectFailure::as_str`), the server's SQLSTATE when there is one and it
is five ASCII letters or digits, and how many candidates were tried, for
example `connect failed: auth (sqlstate 28P01), 2 of 2 candidates tried`.
That tells a missing database (`3D000`) from a rejected password without
printing anything of the driver's.

**A pool that connected is always closed.** From step 2 on the pool
belongs to a guard owned by the creation. Publishing disarms it. If the
creation ends any other way (an error of `init` or `build_value`, their
panic, the deadline, `close` cancelling it) the guard closes the pool with
`pool.close()` on a tracked task, counted in `retiring` until it ended, so
`close()` and the unit wait for it. Closing, not dropping, is the point: a
hook may have kept its clone, and a pool that is merely dropped stays open
for as long as a clone lives. After the close such a clone is a closed
pool: its queries fail, and a connection it still holds delays the close
until it is returned. What a hook does with its clone beyond its own
future is the hook's responsibility: it ends that work itself.

**Disposal.** The value is dropped, then `pool.close().await` runs: a pool
is closed on eviction, invalidation and `close()`, once no `KeyedRef`
remains. A value built by `build_value` normally holds the clone it was
given; it is gone before the close starts. A value that owns tasks ends
them in its `Drop`. There is no `dispose` hook here.

Secrets: `PgConnectOptions` derives `Debug` and shows the password, so
this module never formats, logs or stores it beyond the connect call, and
no `Debug` here prints the connector, a hook or a candidate
(`ConnectCandidates` prints how many it holds). Keys are not logged either
(`K` has no `Debug` bound). The connector returns options, never a URL, so
there is no string with a password in it to leak.

### 6.5 Errors

`crates/sekvent-db/src/reasons.rs` gains:

| Constant | Value | Code | Message | When |
|---|---|---|---|---|
| `KEYED_CAPACITY` | `KEYED_CAPACITY` | `RESOURCE_EXHAUSTED` | `the resource cache is full` | a new key while every entry is in use or being created |
| `KEYED_CREATE_TIMEOUT` | `KEYED_CREATE_TIMEOUT` | `UNAVAILABLE` | `creating the resource took too long` | the factory outlived `create_timeout` |
| `KEYED_FACTORY_PANICKED` | `KEYED_FACTORY_PANICKED` | `INTERNAL` | `internal error` | the factory panicked |
| `KEYED_CLOSED` | `KEYED_CLOSED` | `UNAVAILABLE` | `the resource cache is closed` | `get` after `close`, and the waiters of a creation that `close` cancelled |

The timeout is `UNAVAILABLE`, like a pool that cannot connect in time: to
the caller of a method it is a retryable outage, not that call's deadline.

### 6.6 Tests

Unit tests in `keyed/mod.rs`, `#[tokio::test(start_paused = true)]`, with
factories gated on a `tokio::sync::Notify` or a oneshot and a `dispose`
hook that reports through a channel:

- Single flight: many concurrent `get`s of one key call the factory once
  and get the same value; two keys create side by side.
- A factory error reaches every waiter with its code, message, reason and
  metadata and without a source; nothing is warm afterwards; the next
  `get` calls the factory again and succeeds.
- The deadline: a factory that never finishes fails every waiter with
  `KEYED_CREATE_TIMEOUT` once the clock passes it, and is retried. The
  factory holds a guard that reports its drop: the future was dropped, not
  left running.
- A panicking factory (in its body and before its future exists):
  `KEYED_FACTORY_PANICKED`, retried, no payload in the error; a guard the
  factory built before it panicked was dropped.
- A dropped `get` does not cancel the creation.
- Capacity: the least recently used unreferenced entry is evicted and
  disposed; with every entry in use, or being created, the `get` fails
  with `KEYED_CAPACITY` and succeeds once a reference drops; a `get` of a
  key the cache already holds never counts against the capacity.
- Idleness: an entry is evicted only after `evict_idle_after`; never while
  a `KeyedRef` exists however old it is; dropping the reference restarts
  the idle time; a clone keeps the value in use; `None` evicts nothing;
  `evict_idle_older_than(Duration::ZERO)` evicts every entry not in use
  without the clock moving, and leaves the ones in use.
- `peek`: `None` for an absent key, for a key being created (and the
  factory is not called a second time) and on a closed cache; the same
  value as `get` for a warm key; it does not postpone eviction (a peek
  taken and dropped just before `evict_idle_after` has passed since the
  last `get` reference dropped changes nothing); a held peek reference
  keeps the entry from being evicted.
- `invalidate` of a warm entry with a reference in flight: the reference
  still works, the next `get` creates a new value, the old one is disposed
  exactly once when the reference drops and counts in `retiring` until
  then; without a reference it is disposed before `invalidate` returns; an
  absent key is `false`.
- Generations. `invalidate` during a creation, then a second `get`: the
  factory runs a second time; when the first creation's gate opens, its
  value is disposed and nobody ever received it; when the second's opens,
  the first waiter and the second both hold the second value, and the
  factory was called exactly twice. The same with the first creation
  failing: its error reaches nobody and the first waiter gets the second
  value. A stale creation that ends after the second value is warm
  neither replaces nor removes it (the warm value is the second one
  before and after, and is not disposed).
- `close`: `get` fails with `KEYED_CLOSED`, unreferenced values are
  disposed before it returns, referenced ones when dropped; a creation in
  flight is cancelled (its factory's guard reports the drop), its waiters
  get `KEYED_CLOSED`, and `close` returns only after that; `close` waits
  for a background disposal that was already running (a capacity
  eviction whose hook is gated); a `close` under `tokio::time::timeout`
  that gives up leaves the cache closed and the disposal still finishes;
  twice is harmless.
- `dispose` panics are swallowed; a reference dropped outside a runtime
  (a plain `#[test]` with its own runtime) neither panics nor disposes;
  dropping the future of `invalidate` or `evict_idle` while its disposal
  is gated does not stop the disposal.
- `stats` after each of the above, `creations`, `create_failures` and
  `retiring` included; `build` refusals; `Debug` of `Keyed`, `KeyedRef`
  and the builders shows no key and no value.

`tests/keyed.rs` (features `keyed` + `runtime`, paused clock, a runtime
built with `without_signals()`):

- The unit evicts an idle entry after a reap interval and leaves one in
  use alone; on shutdown the cache is closed and its values disposed
  before `RuntimeHandle::wait` returns.
- A disposal that outlasts the unit's bound: the unit returns `Ok(())`
  once the bound passed (not at the stage grace), the run still completes,
  and the cache is closed.
- A value still referenced at shutdown: the unit returns without waiting
  for it, and the value is disposed when the reference drops.
- The failure log. This file is its own test binary, so it may install a
  recording `tracing::Subscriber` (written in the file with `tracing`
  alone, set with `tracing::subscriber::set_default` on the current-thread
  test runtime, over a global default that answers
  `Interest::sometimes()`, the pattern of `sekvent-error`'s test support):
  a factory error with a two-level source logs one `warn` event with the
  code, the reason and the chain joined with `": "`; a 10 KiB source is
  cut at 2 KiB on a character boundary; the message, the metadata and the
  panic payload never appear; twenty waiters still mean one event.

`tests/keyed_pools.rs` (features `keyed` + `sqlx-postgres`; Docker,
`#[ignore]`, the `docker_tests_enabled()` skip the other files use, real
clock, a 30 s `tokio::time::timeout` around each scenario; the connector
parses the harness's URL into `PgConnectOptions`; roles with a known
password are created per test and named after the test's own database, as
`tests/permissions.rs` does, because roles are server-wide; log events are
read with the same kind of recording subscriber as in `tests/keyed.rs`,
each file carrying its own):

- A pool per database answers `select current_database()`; concurrent
  `get`s run `init` and `build_value` once; what `init` created is there
  for the first caller; `build_value` receives the pool after `init` ran
  and the cache hands out its value.
- A failing `init` and a failing `build_value` publish nothing, close the
  pool and are retried.
- Candidates, told apart by `select current_user`. The first connects: its
  role is the session's. The first has a wrong password and the second is
  right: the second's role, and one `info` event names position 2 of 2.
  The first names a database that does not exist (`3D000`) and the second
  is right: the creation fails with `INTERNAL` and no reason, so nothing
  fell back. Every candidate has a wrong password: `INTERNAL` /
  `DB_CREDENTIALS_REJECTED`, not cached, and neither the error, the log
  event nor any `Debug` output contains a password; the event's `source`
  names `auth` and `28P01`. The first is a role created `NOLOGIN` with its
  right password (`28000`) and the second is right:
  `DB_CREDENTIALS_REJECTED`, so `28000` did not fall back.
- A pool that connected is closed when the creation does not publish: an
  `init` that hands its clone to the test through a channel and never
  returns, under a short `create_timeout`, fails the `get` with
  `KEYED_CREATE_TIMEOUT`, and the `close_event()` of the clone the test
  holds resolves (a pool that was only dropped would stay open under that
  clone); the same for a `build_value` that panics after handing over its
  clone, with `KEYED_FACTORY_PANICKED`.
- A capacity eviction, `evict_idle_older_than(Duration::ZERO)` and an
  `invalidate` close the pool (observed through `PgPool::close_event` on a
  clone taken for the test, never by polling); a held `KeyedRef` keeps
  querying after `invalidate`; with `build_value`, the value is dropped
  before the pool closes.

Tests in the same file that need no server are not `#[ignore]`d: the
builder's refusals; a connector error passed through; `127.0.0.1:1` with a
short `acquire_timeout` answered `UNAVAILABLE` and tried again by the next
`get`; `ConnectCandidates` (`new`, `or`, `From`, a `Debug` without
options). The fallback decision is a private function of one
`sqlx::Error`, unit-tested in `keyed/pools.rs` with a fake
`DatabaseError`, as `error.rs`'s tests build one: `28P01` moves on;
`28000`, `3D000`, an I/O error, `PoolTimedOut` and an error without a
SQLSTATE do not.

### 6.7 Docs

`docs/modules/db.md`: a row in "Enable it"; under "How to …" the sections
`` ### Keep one pool per key (`KeyedPools`) `` and
`` ### Cache any resource by key (`Keyed`) `` (those exact headings). The
first covers candidates and the one code that falls back, `init` and
`build_value`, `peek`, `invalidate`, `evict_idle_older_than`, the unit and
what it does at shutdown, and capacity against a server's connection
limit (`capacity × max_connections` plus headroom for `retiring`); the
second the general cache, generations and the disposal rule. Then: the
settings table of 6.2 under "Configuration keys" with the sentence that
there are no keys; the four reasons in "Errors and reasons"; testing tips
(paused clock, `evict_idle_older_than(Duration::ZERO)` on the real clock,
`close_event`); pitfalls: hold the `KeyedRef`, do not clone the pool out
of it, a hook that keeps its pool clone ends that work itself, work that
outlives a call takes its own reference, set every connect option
explicitly because `PgConnectOptions::new()` reads `PG*` variables, never
print the options, `invalidate` is not for every request, one cache per
process per role.

## 7. Part 2: request scope (`sekvent-context`, `sekvent-component`)

Sections 7.1 to 7.8 are the request scope. Two smaller additions to
`sekvent-component` ride with it because they touch the same files: a
second App on one runtime (7.9) and a rejection whose reason the caller
sees (7.10).

### 7.1 Public API

```rust
// crates/sekvent-context/src/scope.rs

/// What a serving boundary resolved for one request before it
/// authenticated the caller: the tenant the request belongs to, and typed
/// values its handlers need.
#[derive(Clone, Default)]
pub struct RequestScope { /* private; cloning shares */ }

impl RequestScope {
    /// A scope without a tenant or extensions.
    pub fn new() -> Self;
    /// Name the tenant the request belongs to; it must pass
    /// `headers::is_valid_tenant` (7.6).
    #[must_use] pub fn with_tenant(self, tenant: impl Into<String>) -> Self;
    /// Attach `value` as the extension of type `T`, replacing an earlier one.
    #[must_use] pub fn with_extension<T: Clone + Send + Sync + 'static>(self, value: T) -> Self;
    pub fn tenant(&self) -> Option<&str>;
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T>;
}

impl fmt::Debug for RequestScope { /* tenant and the number of extensions */ }

// crates/sekvent-context/src/lib.rs
mod scope;
pub use scope::RequestScope;

impl CallContext {
    /// Attach the scope a serving boundary resolved. A tenant the scope
    /// names becomes the context's tenant. A plain setter: it checks
    /// neither the tenant nor an end user already set.
    #[must_use] pub fn with_scope(self, scope: RequestScope) -> Self;
    /// The request scope, when this call was served with one.
    pub fn scope(&self) -> Option<&RequestScope>;
    /// The scope's extension of type `T`.
    pub fn scoped<T: Send + Sync + 'static>(&self) -> Option<&T>;
}

// crates/sekvent-context/src/headers.rs
/// The inbound `x-sekvent-tenant` rule: 1–255 visible ASCII, no spaces.
pub fn is_valid_tenant(tenant: &str) -> bool;
```

```rust
// crates/sekvent-component/src/scope.rs  (no feature needed)

/// Resolves the scope of a request before its caller is authenticated.
pub trait ScopeResolver: Send + Sync + 'static {
    /// The scope of `request`. `cx` is the context decoded from its head.
    fn resolve<'a>(
        &'a self,
        request: &'a http::request::Parts,
        cx: &'a CallContext,
    ) -> Pin<Box<dyn Future<Output = Result<RequestScope, AppError>> + Send + 'a>>;
}

/// Any sync closure is a resolver.
impl<F> ScopeResolver for F
where
    F: Fn(&http::request::Parts, &CallContext) -> Result<RequestScope, AppError>
        + Send
        + Sync
        + 'static;

/// Make `user` the end user of `cx`, as the component service does once
/// its authenticator accepted `user`: when `cx`'s scope names a tenant, a
/// user who does not name the same one (another, or none) is refused (7.6).
pub fn admit_end_user(cx: CallContext, user: EndUser) -> Result<CallContext, AppError>;

// crates/sekvent-component/src/lib.rs
mod scope;
pub use scope::{ScopeResolver, admit_end_user};
pub use sekvent_context::{CallContext, EndUser, RequestScope};

// crates/sekvent-component/src/app.rs
impl AppBuilder<'_> {
    /// Register the App's scope resolver. One per App; a second is
    /// `BuildError::DuplicateScopeResolver`.
    pub fn scope_resolver(&mut self, resolver: impl ScopeResolver) -> Result<(), BuildError>;
}

impl App {
    /// Run the App's scope resolver for a request served outside the
    /// component service (a REST route) and attach its scope to `cx`.
    pub async fn resolve_scope(
        &self,
        request: &http::request::Parts,
        cx: CallContext,
    ) -> Result<CallContext, AppError>;
}

// crates/sekvent-component/src/reasons.rs
/// The resolver returned a tenant that fails `is_valid_tenant` (`INTERNAL`).
pub const SCOPE_TENANT_INVALID: &str = "SCOPE_TENANT_INVALID";

// crates/sekvent-component/src/error.rs, in BuildError (non-exhaustive)
/// A second scope resolver was registered.
#[error("a scope resolver is registered twice")]
DuplicateScopeResolver,
```

`AppBuilder`'s `Debug` gains `scope_resolver: bool`. No configuration key,
no feature: the trait, the builder method, `resolve_scope` and
`admit_end_user` compile without `grpc`, like the authenticator's.

### 7.2 Storage

`RequestScope` mirrors `EndUser`'s extensions: an optional tenant and an
`http::Extensions` map, one value per type, the last insert of a type
wins, inserts need `Clone + Send + Sync + 'static`, reads
`Send + Sync + 'static`. It differs in one way: the two fields sit behind
a private `Arc` (builders use `Arc::make_mut`), so a clone shares them.
The context is cloned on every call and the same scope is also put into
the request head for the authenticator; neither clones a value.

The context holds `scope: Option<RequestScope>`. `Debug` (reached through
the context's derived one) prints the tenant and `extensions: <n>`, never
a value or a type name, as `EndUser`'s does. A newtype per meaning keeps
two sources from colliding on `String`.

A value in the scope lives as long as some clone of the scope does, so how
long a `KeyedRef` placed there keeps its entry in use depends on who holds
a clone. The serving boundary guarantees this much (7.5):

| Call | The scope is released |
|---|---|
| unary | after the handler returned, not before |
| server-streaming | after the stream ended (it completed, failed, expired, was closed or was dropped by the consumer), not when the open returned |
| REST route through `App::resolve_scope` | when the route drops the context it got back and every clone of it |

Code that keeps a clone of the context keeps the scope for that long too.

### 7.3 Changes to `CallContext`

| Method | Change |
|---|---|
| `new` | no scope |
| `with_scope` | stores the scope; `tenant = scope.tenant()` when the scope names one, otherwise the tenant is left as it is |
| `with_end_user` | subject and caller as today. Tenant: the scope's when the context has a scope that names one, otherwise the user's as today (cleared when the user has none) |
| `sanitize_for_caller` | where it drops the tenant today, the tenant becomes the scope's (or none); subject and client address are dropped as today |
| `child`, `detached` | no scope; the tenant stays, like subject |
| `with_caller`, `with_tenant` | unchanged |

`headers.rs` gains only `is_valid_tenant` (the existing token rule for
`x-sekvent-tenant`, made public). `inject` and `propagate` write the
context's tenant as they do today, so a tenant the scope set travels to
peers exactly like an end user's; `from_headers` never produces a scope;
nothing ever writes an extension.

`child()` and `detached()` drop the scope on purpose, and their
documentation says what follows from it: a callee and a queued job get the
tenant and nothing else of the scope. Work that outlives the call (a
spawned task, a job queued with `detached()`) must not carry a value it
cloned out of the scope, such as a pool taken from a `KeyedRef`: the
reference that kept the value alive is released with the call, and the
value may be closed under the work. Such work takes its own reference,
`pools.get(tenant)`, and holds it for as long as it runs.

### 7.4 Where it runs

| Call | Resolver | `cx.scope()` in the handler |
|---|---|---|
| served over gRPC or gRPC-Web, no link token matched: an end user, an anonymous method, an `optional_end_user` method with or without a header, any caller under `SERVE_AUTH=none` | runs once | `Some` |
| served over gRPC to a link caller (`link`, or a matching token under `link,bearer`) | never | `None` |
| `local`, `local-serialized` | never | `None` |
| caller side of `grpc` | never | |

Decided: link callers and in-process calls get no scope. The scope belongs
to the request that entered the system and reaches the handler that served
it, nothing below it, like the end user and its extensions. A component
called by another component then sees the same context whether the call
stayed in the process or crossed a link, and the tenant has one source per
call: the trusted link on a link call, the resolver on every other. A
handler that other components may call works from `cx.tenant()`, which
does travel, and treats the scope as a shortcut:
`cx.scoped::<TenantDb>()`, else `pools.get(tenant)`.

### 7.5 Serving order

In `ComponentService::call`, between the address-keyed client limit and
the end-user authenticator. The full order becomes:

1. Link.
2. Route.
3. Context.
4. Client limit by address.
5. **Request scope**, when the App has a resolver and no link matched
   (`cx.caller()` is `None`).
6. End user.
7. Client limit by end user.
8. Hop limit.
9. Call authorizer.
10. Gate, bulkhead, timeout, handler.

So an unknown path and a flood from one address never cost a resolution,
the body is not read yet, and the authenticator, both later limits and the
call authorizer see the scope.

The call's cancellation guard (`cx.cancel_token().clone().drop_guard()`)
is taken before step 5, not after step 9: a resolver that hands
`cx.cancel_token()` to spawned work sees it cancelled when the client goes
away or the deadline passes during resolution. A unary call disarms it
once the handler answered; a stream hands it to `serve_stream`, which
disarms it after the open, so the stream's own guard takes over as before.

```rust
let guard = cx.cancel_token().clone().drop_guard();
let (mut head, body) = request.into_parts();
if cx.caller().is_none() {
    cx = match served.resolve_scope(method, cx, &mut head, open_deadline).await {
        Ok(cx) => cx,
        Err(error) => return Ok(error_response(error)),
    };
}
if end_user { /* authenticate, as today */ }
```

`Served::resolve_scope` (new, `Served` gains
`scope: Option<Arc<dyn ScopeResolver>>` set by `with_scope_resolver`, and
`expose` in `app.rs` passes the App's resolver to every exposed component,
whatever its `SERVE_AUTH`):

| The resolver | The caller gets |
|---|---|
| none registered | the call continues without a scope |
| `Ok(scope)` | the scope is inserted into `head.extensions` (as `RequestScope`) and the context becomes `cx.with_scope(scope)` |
| `Ok(scope)` with a tenant that fails `is_valid_tenant` | `INTERNAL` / `SCOPE_TENANT_INVALID`, tagged, "the scope resolver returned an invalid tenant"; only the tenant's length is logged. Otherwise the header codec would drop it and a downstream call would run without a tenant |
| `Err(e)` | `e` unchanged: no tag, no metadata, no normalisation, also for `UNAUTHENTICATED` |
| outlives the call's deadline (for a stream, the open bound) | `DEADLINE_EXCEEDED`, "the call deadline was exceeded", as for the authenticator |
| the bound has already passed when step 5 is reached (`grpc-timeout: 0n`) | the same `DEADLINE_EXCEEDED`, without running the resolver (`timeout_at` would poll it once first) |
| panics | `INTERNAL` / `HANDLER_PANICKED`, tagged, message "the scope resolver of component `<name>` panicked"; the payload is never shown |

**The authenticator** keeps its signature. It reads the scope from the
head it is given: `request.extensions.get::<RequestScope>()`.

**A resolver may look at the route.** It gets the request head, so
`request.uri.path()` is the path the component service routed on,
`/<package>.<Service>/<Rpc>`, without the server's prefix. One listener
often serves methods that belong to a tenant next to methods that do not
(an operator API, a sign-up call): the resolver returns
`RequestScope::new()` for those, so it never opens a tenant's resources
for a caller nobody has authorized yet. The call authorizer then decides
on the context alone.

**Unary calls keep the scope for the handler.** `Server::run` holds the
context until the handler returned, as today; nothing changes.

**Streams keep the scope until they end.** Today `Server::open_stream`
(`crates/sekvent-component/src/server.rs`) builds the stream's context,
hands the handler's open a clone, and drops its own when it returns; the
copy in the request's extensions is gone by then as well, because tonic's
request is consumed when the open is called. A producer that does not
capture the context (it took what it needed during the open) would
therefore run with the scope already released, and a `KeyedRef` in it
would let its pool be evicted under a live stream. The fix is in the
serving wrapper:

- `Live` gains a field `_scope: Option<RequestScope>`, declared right
  after `producer`, so the producer is dropped first and the scope right
  after it, before the bulkhead permit and the admission.
- `open_stream` fills it from the context it was given
  (`cx.scope().cloned()`), before the open runs.

`Live` is dropped as a whole when the stream ends for any reason
(`ServingStream::end`, or its `Drop` when the consumer goes away), so the
scope is released exactly then. `open_stream` is shared by every binding;
under the in-process bindings the context has no scope and the field is
`None`. When the open fails nothing is kept.

**`App::resolve_scope`** applies the same rule outside the component
service. The App keeps its resolver for it (`Inner` gains
`scope: Option<Arc<dyn ScopeResolver>>`, whether or not a component is
exposed). `App` is `Clone` and a clone shares everything (`App::register`
already clones it into its unit), so a REST route holds a clone, usually
as router state:

```rust
async fn list_orders(
    State(app): State<App>,                 // a clone of the App
    Ctx(cx): Ctx,                           // the context the runtime server attached
    request: axum::extract::Request,
) -> Result<Json<Orders>, AppError> {
    let (parts, _body) = request.into_parts();
    let cx = app.resolve_scope(&parts, cx).await?;         // tenant and extensions
    let user = auth.end_user::<Profile>(&parts.headers)?;  // the route's own authentication
    let cx = admit_end_user(cx, user)?;                    // the tenant rule of 7.6
    // …
}
```

- A context with a service caller, or an App without a resolver: `cx` is
  returned unchanged.
- Otherwise the resolver runs within `cx`'s deadline (past it:
  `DEADLINE_EXCEEDED`, "the call deadline was exceeded"; not run when it
  had already passed) and `Ok(cx.with_scope(scope))` is returned. The
  error is the resolver's, unchanged; a panic propagates like any code of
  the route. An invalid tenant is `INTERNAL` / `SCOPE_TENANT_INVALID`.
- When `cx` already has an end user (the route authenticated first), the
  rule of 7.6 applies to that user: a mismatch is the constant
  `UNAUTHENTICATED`.
- It takes the head by shared reference and changes nothing in it: the
  scope is **not** put into the request's extensions. A route that wants
  it there (for an extractor further in) inserts `cx.scope().cloned()`
  itself.
- It does **not** run the end-user authenticator: a REST route
  authenticates by its own means. A route that authenticates after
  resolving calls `admit_end_user`, the function the component service
  itself calls, so both surfaces apply one rule. Calling
  `cx.with_end_user(user)` or `cx.with_scope(scope)` directly skips it.
- The scope lives as long as the returned context and its clones; a route
  that streams its response keeps a clone in the body.

### 7.6 The tenant

`CallContext::tenant` is an `Option<String>` that `from_headers` reads
from `x-sekvent-tenant` and `sanitize_for_caller` drops unless the caller
is a trusted link. With the scope its sources are, per call:

| Caller | `cx.tenant()` |
|---|---|
| trusted link | the link's `x-sekvent-tenant`, as today; no resolver |
| untrusted link | none, as today; no resolver |
| anyone else | a header the client sent is dropped by `from_headers` in step 3, before the resolver runs; then the scope's tenant when it names one; then the end-user rule below |

Nothing reads a header into the context after step 3, so **a tenant the
resolver set cannot be replaced by a header**, on an authenticated call or
an anonymous one.

End-user rule, after the authenticator accepted a user. Decided: the
framework applies it, not each authenticator.

| Scope's tenant | User's tenant | Outcome |
|---|---|---|
| none | any | as today: the user's tenant, or none |
| `S` | none | refused like the last row (owner decision: fail closed; a tenant-less user is not admitted into whatever tenant the host names) |
| `S` | `S` | the call runs with tenant `S` |
| `S` | `U` ≠ `S` | refused: `UNAUTHENTICATED`, `END_USER_REJECTED_MESSAGE`, no reason, the same answer as a bad token; logged at `debug` without either value |

The rule is `admit_end_user(cx, user)`: the refusals of rows two and
four, and otherwise `Ok(cx.with_end_user(user))`. `Served::authenticate`
calls it where it calls `cx.with_end_user(user)` today, and
`App::resolve_scope` applies it to an end user already in the context.
On those paths, when the scope names `S`, a call with an end user runs
only for a user of `S` (`cx.tenant()` and `cx.end_user().tenant()` are
both `S`); a context assembled by hand with `with_scope` and
`with_end_user` is not checked. A credential that is valid for another
tenant tells its holder nothing: the refusal never carries a reason, not
even when the authenticator uses 7.10. An authenticator that lets a user
act across tenants (an operator) returns that user with the scope's
tenant, which it reads from the head's extensions.

A resolved tenant must pass `headers::is_valid_tenant`, the rule
`from_headers` applies to `x-sekvent-tenant`; anything else fails the
call (7.5), since the codec would lose it on the next hop.

The typed extensions never leave the process. The tenant does, as it
always did: it is a field of the context, `child()` keeps it and the
header codec forwards it to peers that trust this service's link.

### 7.7 Tests

`sekvent-context`, unit tests in `scope.rs`: builders and getters; an
absent type is `None`; the last insert of a type wins; clones share;
`Debug` of the scope and of a context holding one shows the tenant and the
count, no value and no type name; `with_scope` sets a named tenant and
leaves the tenant alone otherwise; `scope()` and `scoped::<T>()`; `child`
and `detached` drop the scope and keep its tenant; `with_end_user` after
`with_scope` for the four rows of 7.6 (the context keeps `S` in the last
one: the refusal is the serving boundary's); `sanitize_for_caller` keeps a
scope's tenant, drops a header's, and still drops subject and client
address; `from_headers` with a client `x-sekvent-tenant` and no caller,
then `with_scope`, yields the scope's tenant; `inject`, `propagate` and
`propagate_external` write no extension, and only the first two write the
tenant.

`sekvent-component`:

- `scope.rs`: a closure is a resolver; its error passes; `admit_end_user`
  for the four rows of 7.6, without a scope, and that its refusal has the
  constant message and no reason; an invalid tenant is
  `SCOPE_TENANT_INVALID`.
- `server.rs` tests, with an extension whose drop is observable (a `Clone`
  wrapper around an `Arc` of a type that signals from `Drop`):
  - `run`: while the handler is parked the signal has not fired; it fires
    once the handler returned.
  - `open_stream` with a producer that does **not** capture the context
    (the existing `open` helper builds exactly that): the signal has not
    fired after the open returned, nor after items were read; it fires
    when the stream completes, when its consumer drops it, when the
    lifetime expires, when the caller's deadline passes and when the
    streams are closed. One case each.
  - a failed open releases the scope at once.
- `grpc/service.rs` tests: the resolver runs for an authenticated, an
  anonymous and an optional-end-user method (with and without a header)
  and under `none`; never for a link caller under `link` and `link,bearer`;
  never for an unknown path; never after the address-keyed limit refused
  the call; before the authenticator, which finds the scope in the head's
  extensions; the handler and the call authorizer see `cx.scope()`,
  `cx.scoped::<T>()` and the tenant; a client `x-sekvent-tenant` loses to
  the scope and is dropped without one; a resolver error arrives unchanged
  (code, message, reason, metadata), the handler does not run and a body
  that never arrives is still refused; the deadline; a panic; the four
  rows of 7.6; an expired call (`0n`) never reaches the resolver; an
  invalid tenant is `INTERNAL`; a call dropped during resolution, or whose
  deadline passes there, cancels the token the resolver saw, and a served
  unary call or an open stream does not; a route-aware resolver that returns an empty scope for one
  method and a tenant for another; a unary call releases the scope's value
  only after the handler returned; a stream open (the scope reaches the
  stream's handler, the open bound limits the resolver) whose producer
  does not capture the context keeps the scope's value until the response
  stream ended.
- `tests/app.rs`: a second registration is `DuplicateScopeResolver`;
  `Debug`; `App::resolve_scope` without a resolver, with one, for a
  context with a service caller, past the deadline, with a resolver error,
  through a clone of the App; it leaves the request's extensions alone; a
  user of another tenant passes `cx.with_end_user` and is refused by
  `admit_end_user`; an end user already in the context of another tenant,
  or of none, is refused; an invalid tenant is `SCOPE_TENANT_INVALID`.
- `tests/grpc_scope.rs` (loopback `127.0.0.1:0`, real clock, 30 s guard,
  the runtime server with `.prefix("/api")` and gRPC-Web over HTTP/1.1, as
  `tests/grpc_end_user.rs` does): a resolver that maps the `host` header to
  a tenant; an anonymous method and an authenticated one see it; an
  unknown host is answered with the resolver's `NOT_FOUND` as
  `grpc-status` 5 and its message; a CORS preflight never reaches the
  resolver; a handler that calls a second component under `local` and
  `local-serialized`: the callee has the tenant and no scope; a served
  stream whose producer does not capture the context holds the scope's
  value until the client has read the last item, and releases it when the
  client goes away mid-stream.
- `sekvent-facade-check`: a closure registered through
  `sekvent::component::{ScopeResolver, RequestScope}`, and
  `sekvent::component::admit_end_user`.

### 7.8 Docs

`docs/modules/components.md`: a section
`## How to resolve a request scope` (that exact heading) between "How to
serve components to end users (browsers, apps)" and "How to authorize
calls per method": registering, the trait and the closure, reading it in a
handler and in the authenticator, the table of 7.4, the tenant rules of
7.6, how long the scope lives (the table of 7.2, streams included), the
route-aware resolver, the REST recipe with `App::resolve_scope` and
`admit_end_user` and the three things `resolve_scope` does not do, and the
host pitfall (HTTP/2 carries it in the URI, HTTP/1.1 in `host`, which may
include a port; behind a proxy the app decides which forwarded header it
trusts), the pitfalls of a scope held while the body is read (no body
timeout; anonymous calls can hold or churn pinned per-tenant resources:
return an empty scope for them, rate-limit by address, size the cache) and
of the per-end-user client limit keyed on the subject alone (equal
subjects in different tenants share a budget). The list under "Serving
order for end users" gets step 5.
"Pitfalls and security" gets the nested-call rule, "never a task-local",
and "work that outlives the call takes its own reference".

`docs/modules/context.md`: a row `request scope` in the "Concepts" table
and a subsection `### Request scope` (that exact heading) at the end of
"Concepts": the API, storage, `Debug`, what `child` and `detached` keep
and what that means for work that outlives the call, and that the tenant
travels while the extensions do not. "How the context travels as headers"
says the scope is never encoded.

### 7.9 A second App on one runtime (`App::register_as`)

`App::register` names its unit `components`, and the runtime refuses two
units of one name, so a runtime runs one App today. A service with two
audiences wants two: a public App and an operator App, each with its own
authenticator, authorizer and scope resolver, each mounted on its own
listener.

```rust
// crates/sekvent-component/src/runtime.rs  (feature `runtime`)

impl App {
    /// As today: the unit `components`.
    #[must_use]
    pub fn register(&self, runtime: RuntimeBuilder) -> RuntimeBuilder;

    /// The same unit under `name`, for a runtime that runs more than one
    /// App.
    #[must_use]
    pub fn register_as(&self, name: impl Into<String>, runtime: RuntimeBuilder) -> RuntimeBuilder;
}
```

`register(runtime)` becomes `self.register_as(UNIT, runtime)` with the
existing constant `UNIT = "components"`; its body moves into `register_as`
unchanged. The unit is the same in every respect but its name: critical,
in `Stage::Components`, it starts the App, reports ready, keeps the health
of the App's gRPC services, closes the streams when shutdown begins and
drains within half of its stop window.

- **Names** are the runtime's business, as for `RuntimeBuilder::unit`:
  `register_as` checks nothing, and `RuntimeBuilder::build` fails with
  `INVALID_ARGUMENT` for a name used twice (`unit <name> is registered
  twice`) or an empty one (`a unit has an empty name`).
- **Two Apps share nothing but the runtime.** Each is built by its own
  `AppBuilder` and serves through its own `grpc_routes()`. Their units are
  in the same stage: they start side by side and drain side by side, in no
  order between them, each with its own stop window. A component of one
  App reaches a component of the other over a `grpc` binding, like
  another process.
- **Health entries are per service name**, and the check for two
  components serving one gRPC service looks at one App. A service exposed
  by two Apps is one health entry with two writers: expose a service from
  one App.
- **Configuration** is read from the `ConfigSource` each builder was
  given. A component installed in both Apps reads the same
  `SEKVENT_COMPONENT_<C>_*` keys unless the two builders get different
  sources.

```rust
let public = public_builder.build()?;                   // end users
let admin = admin_builder.build()?;                     // operators: its own authenticator
let api = Server::builder().grpc_routes(public.grpc_routes()).bind(public_addr).await?;
let ops = Server::builder().grpc_routes(admin.grpc_routes()).bind(admin_addr).await?;
let builder = public.register(Runtime::builder());      // unit `components`
admin
    .register_as("admin-components", builder)
    .unit("api", Stage::Ingress, UnitPolicy::default(), api.into_unit())
    .unit("ops", Stage::Ingress, UnitPolicy::default(), ops.into_unit())
    .build()?
    .run()
    .await?;
```

Tests, in `crates/sekvent-component/tests/runtime.rs`:

- Two Apps on one runtime (`first.register(second.register_as(
  "admin-components", Runtime::builder().without_signals()))`): both
  start, both components serve a call, and after `shutdown` the report
  has the units `components` and `admin-components`, both in
  `Stage::Components` and `UnitExit::Completed`; both Apps are stopped and
  each lifecycle hook ran once.
- A name used twice is the runtime's build error: two `register` calls,
  and `register_as("components", ..)` next to `register`, fail
  `RuntimeBuilder::build` with `INVALID_ARGUMENT` and a message that names
  the unit.
- `register` still names its unit `components` (the existing tests assert
  it and stay as they are).

Docs: `docs/modules/components.md`, "Under the runtime": the method, the
four points above and the example, and a row in the API table next to
`register`. `docs/modules/runtime.md`: the bullet about
`app.register(builder)` also names
`app.register_as(name, builder)` for a further App.

### 7.10 A rejection the caller can tell apart (`EndUserRejection`)

`end_user::rejection()` replaces every `UNAUTHENTICATED` of the
authenticator with one constant answer, so a client cannot tell "your
session ended, sign in again" from "this credential means nothing here"
and cannot react differently. An authenticator can now opt in, one error
at a time, to showing a reason. The constant answer stays the default.

`AppError` has no notion of a field that may or may not be shown: all of
it but the source is caller-visible by definition, and this boundary is
the one place that strips it. No such marker is added to `sekvent-error`.
The opt-in is a typed value that travels in the error's existing source
slot, which is never sent anywhere.

```rust
// crates/sekvent-component/src/end_user.rs

/// A refusal of an end user whose reason the caller is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndUserRejection { /* a reason and an optional domain; private */ }

impl EndUserRejection {
    /// A rejection the caller sees as `UNAUTHENTICATED` with `reason`.
    pub const fn new(reason: &'static str) -> Self;
    /// Name the domain the reason belongs to.
    #[must_use]
    pub const fn with_domain(self, domain: &'static str) -> Self;
    pub const fn reason(&self) -> &'static str;
    pub const fn domain(&self) -> Option<&'static str>;
}

impl fmt::Display for EndUserRejection { /* `end user rejected: <reason>` */ }
impl std::error::Error for EndUserRejection {}

/// `UNAUTHENTICATED` with `END_USER_REJECTED_MESSAGE`, the reason, the
/// domain when there is one, and the rejection itself as the source.
impl From<EndUserRejection> for AppError { /* … */ }

// crates/sekvent-component/src/lib.rs
pub use end_user::{END_USER_REJECTED_MESSAGE, EndUserAuthenticator, EndUserRejection};
```

```rust
const SESSION_ENDED: EndUserRejection =
    EndUserRejection::new("SESSION_ENDED").with_domain("accounts");

// in an authenticator
if session.ended() {
    return Err(SESSION_ENDED.into());
}
```

`rejection()` becomes:

| The authenticator returns | The caller gets |
|---|---|
| `Err(e)` with code `UNAUTHENTICATED` whose source is an `EndUserRejection` | `UNAUTHENTICATED`, `END_USER_REJECTED_MESSAGE`, the rejection's reason, its domain when it has one, and nothing else |
| `Err(e)` with code `UNAUTHENTICATED` otherwise | the constant answer, as today: no reason, no domain, no metadata |
| `Err(e)` with any other code | `e` unchanged, as today, whatever its source |

The rules:

- **The answer is built from the typed value alone.** `rejection()` looks
  at the error's direct source (`std::error::Error::source(&e)`, downcast
  to `EndUserRejection`; causes further down are not searched) and builds
  a new error from the reason and the domain it finds there. It does not
  copy fields of `e`. Whatever else the authenticator did to the error,
  another message, `with_reason`, `with_metadata`, a retry hint, is not
  sent.
- **The message is always `END_USER_REJECTED_MESSAGE`.** The reason is the
  signal; a client matches on it (`google.rpc.ErrorInfo.reason` under gRPC
  and gRPC-Web, `AppError::reason` through a handle) and never on text. No
  free text reaches the caller through this path.
- **Metadata, retry hint and field violations are dropped.** Metadata is
  where a session id or a user id ends up, and an unauthenticated caller
  gets none of it.
- **The reason and the domain are `&'static str`**: constants of the
  program. Nothing read from the request, the credential or a store can
  reach the caller through them. The convention is `AppError`'s
  (`UPPER_SNAKE_CASE`), and like `AppError::with_reason` nothing validates
  it.
- **It fails closed.** `with_source(other)` on the converted error replaces
  the rejection, and the answer is the constant one again. An error with
  another code is never turned into a rejection.
- **The source does not leak.** The answer is a new error without a
  source; a source is never serialized in any case; and `UNAUTHENTICATED`
  is not a code the serving boundary logs.
- **Where it applies.** Only to what the App's end-user authenticator
  returns, on mandatory and on optional-end-user methods. Not to the
  tenant refusal of 7.6, not to link rejections, not to a panic or a
  passed deadline of the authenticator: those keep their answers.
- **What a reason gives away.** Every reason tells anyone who can present
  a credential which of two outcomes it met. Use one for a state the
  holder of a once-valid credential may know (the session ended, the
  password must be changed), never to tell an unknown user from a wrong
  password or a malformed token from one signed by an unknown key. The
  documentation says so next to the type.

Tests:

- `end_user.rs`: the conversion (code, message, reason, domain, the
  source downcasts back); `rejection()` for the three rows; an error
  converted and then given another message, another reason and metadata
  still answers with the rejection's reason and domain, the constant
  message and no metadata; `with_source` over it gives the constant
  answer; an `UNAVAILABLE` error whose source is an `EndUserRejection`
  passes unchanged; a rejection one level further down the chain is not
  found; `Display`, `Debug`, the accessors and use in a `const`.
- `grpc/service.rs` tests: an authenticator that returns the rejection
  answers `UNAUTHENTICATED` with the reason and the domain, the handler
  does not run and the body is not read; the same on an
  optional-end-user method called with a header; the tenant refusal of 7.6
  still has no reason when the authenticator would have used one for
  another case.
- `tests/grpc_end_user.rs`: over gRPC-Web a client reads the reason and
  the domain from the standard error details of a rejected call, and an
  ordinary bad token still gets the bare constant answer.
- `sekvent-facade-check`: `sekvent::component::EndUserRejection` converts
  into `AppError`.

Docs: `docs/modules/components.md`: the table of "What a rejected end user
sees" gets the first row above and the section the rules, the example and
the warning about what a reason gives away; "Register the end-user
authenticator" shows one use; "Error codes and reasons" says that an
`UNAUTHENTICATED` from the authenticator carries no reason unless the
authenticator returned an `EndUserRejection`. The documentation of the
`EndUserAuthenticator` trait changes with it.

## 8. Part 3: CORS by predicate (`sekvent-runtime`)

### 8.1 Public API

```rust
// crates/sekvent-runtime/src/cors.rs

impl Cors {
    /// Origins decided per request by `allowed`, with no fixed list.
    pub fn origin_predicate<F>(allowed: F) -> Self
    where
        F: Fn(&str, &http::request::Parts) -> bool + Send + Sync + 'static;

    /// Also allow the origins `allowed` accepts, next to this list. A
    /// later call replaces the function; under `any_origin` it is dropped.
    #[must_use]
    pub fn or_origin_predicate<F>(self, allowed: F) -> Self
    where
        F: Fn(&str, &http::request::Parts) -> bool + Send + Sync + 'static;
}
```

Nothing else changes: `ServerBuilder::cors(cors)`, `Cors::from_config`,
the other builders and `validate` keep their signatures, `server.rs` and
`lib.rs` are untouched, and there is no configuration key (a function has
none). A service that reads a list from configuration composes the two:

```rust
let cors = match Cors::from_config(&env, "ORDERS_CORS_")? {
    Some(listed) => listed.or_origin_predicate(site_origins),
    None => Cors::origin_predicate(site_origins),
};
```

### 8.2 Behaviour

| Built with | An `Origin` is allowed when | `Access-Control-Allow-Origin` |
|---|---|---|
| `any_origin()` | always | `*` |
| `origins(list)` | it is listed | the origin |
| `origin_predicate(f)` | `f(origin, head)` | the origin |
| `origins(list).or_origin_predicate(f)` | it is listed, else `f(origin, head)` | the origin |
| `any_origin().or_origin_predicate(f)` | always; `f` is dropped at once, with what it captures | `*` |

- **Nothing is rejected.** CORS decides response headers, never whether a
  request runs, with a function as with a list. A preflight is answered
  `200` by the layer either way, with the `access-control-*` headers or
  without them. Every other request goes on to its handler and executes;
  only the decoration of its response differs. "Not allowed" below means
  exactly that: no `access-control-*` header. A browser then keeps the
  page from reading the answer; a client that is not a browser does not
  care.
- **One function for both.** The middleware computes the decision once,
  before it tells a preflight from a request, exactly where it matches the
  list today. A preflight is answered from that decision; on a request the
  same decision decorates the response after the handler ran.
- **The list first.** A listed origin (exact, `:*` port or custom scheme)
  never reaches the function.
- **What the function gets.** The `Origin` header's text as the browser
  sent it, not normalized, and the head of the request it is on: for a
  preflight the `OPTIONS` request with its `Access-Control-Request-*`
  headers. CORS sits outside the call context and the application layers,
  so the head carries the TCP peer (`ConnectInfo<SocketAddr>`) but no
  `CallContext`, and the path still has the server's prefix. An HTTP/2
  request names its host in the URI authority (`:authority`), not in a
  `Host` header, so a rule on the host reads `head.uri.authority()` first
  and falls back to `Host`; the docs show that helper.
- **A request without `Origin`** (a same-origin page, a native client,
  another service) never reaches the function. It gets no CORS decoration,
  no `access-control-*` header, and it is not rejected: it is served like
  any other request, as today.
- **What else never reaches it** and counts as not allowed: an `Origin`
  that is not visible ASCII text (`HeaderValue::to_str` fails), and `null`
  in any case, which the list does not allow either.
- **A panic** in the function is caught, counts as "not allowed" and is
  logged at `error` without the origin. The process panic hook still runs
  before the catch (the default one prints the message to stderr), so the
  docs say the function must not panic with the origin or a secret in its
  message, and that one panicking on every call turns each anonymous
  request into a stderr dump.
- **Answers.** An allowed origin is echoed, never `*`. Every answer under
  a function carries `Vary: Origin`, as under a list. An origin that is
  not allowed gets no `access-control-*` header, as today. Headers set by
  handlers and layers are still stripped.
- **Only `Origin` is varied on.** The layer cannot know what else the
  function reads. A function that decides on another part of the head (a
  cookie, the language, the peer address) must make those responses
  uncacheable or add its own `Vary`, or a shared cache serves one client's
  decision to another; the docs say so. The host needs neither: caches key
  on the URI already.
- **Under `any_origin()`** the function would never be called, so
  `or_origin_predicate` drops it at once instead of storing it: whatever it
  captures is released, and the value equals plain `any_origin()` and
  prints `predicate: None`.
- **Credentials** are accepted with a function, as with a list: the answer
  names one origin. They stay invalid with `any_origin()`. The function is
  then the only guard, which the docs say next to it.

`Cors` keeps `Debug`, `Clone`, `PartialEq` and `Eq`. The function is held
in a private newtype over
`Arc<dyn Fn(&str, &http::request::Parts) -> bool + Send + Sync>` whose
`Debug` prints no address, and whose equality is identity (`Arc::ptr_eq`,
which compares the addresses and ignores the vtables): a clone equals its
original, two functions never equal each other. Internally a `Cors` built
by `origin_predicate` has an empty list; `Cors::origins` still refuses an
empty one.

### 8.3 Why synchronous

Decided: the function is synchronous. It runs for every cross-origin
request and preflight, before anything else in the stack and before any
authentication, so an anonymous client decides how often. An asynchronous
one would invite a lookup there. It gets the head and not just the origin
because a rule may depend on the path or the host (public routes, a host
per site), and passing it costs nothing.

Two notes for whoever writes one, which the docs repeat:

- **A function backed by a registry reads a snapshot.** When the allowed
  origins live in a store (one per tenant, added and removed at run time),
  the function reads an in-memory copy, a set behind an `ArcSwap` or an
  `RwLock`, that a job or a change notification refreshes out of band,
  the stance the call authorizer takes. The cost is a delay: a new origin
  is allowed once the snapshot has it, a removed one stops being allowed
  once it is gone. Size the refresh to that.
- **CORS headers never stop a request from executing** (8.2). They keep a
  browser page from reading an answer, and with a preflight from sending
  some requests at all; a request that needs no preflight, and any client
  that is not a browser, reaches the handler whatever the function says.
  A service that ties origins to tenants therefore does not rely on the
  function for it: where it resolves the tenant (its scope resolver, 7.5,
  or its authorizer) it also checks the request's `Origin`, when there is
  one, against the origins of that tenant and refuses a mismatch.

### 8.4 Tests

Unit tests in `cors.rs`, with the helpers already there: a function alone
allows and refuses; the preflight and the request get the same decision,
and the preflight's head (method `OPTIONS`, the request-method header) is
what the function saw; a list with a function (a listed origin never
calls it, counted; an unlisted one does); the echoed origin, never `*`;
`Vary: Origin` on allowed and not allowed answers; credentials with a
function; `null`, a non-ASCII `Origin` and a missing one never call it; a
request without `Origin` still reaches its handler (it runs, counted) and
its response has no `access-control-*` header; so does a request from an
origin the function does not allow; a panic is "not allowed";
`any_origin().or_origin_predicate` never calls it and answers `*`, and
drops it (a captured `Arc` is back to one strong count, the value equals
`any_origin()`, also from configuration `*`, while a list keeps it); inner
CORS headers are still replaced; a second `or_origin_predicate` replaces
the first; `validate` passes; equality (clone, two functions) and `Debug`.

`tests/cors_predicate.rs` (loopback `127.0.0.1:0`, real clock, 30 s
guard): a server whose function reads a shared set; a preflight and a
request from an origin get no `access-control-*` header (the request is
still answered by its handler), the origin is added to the set, and both
are allowed, without restarting anything; a request without `Origin` is
served before and after.

### 8.5 Docs

`docs/modules/server.md`, under "Configure CORS": two rows in the method
table and a subsection `#### Decide origins with a function` (that exact
heading) with the composition example, the table of 8.2, what the function
sees and never sees, reading the host from the URI authority before
`Host`, that a request without `Origin` is served undecorated and nothing
is ever rejected, that it must never panic (the panic hook prints the
message), that responses vary on `Origin` only, why the function must be
cheap and how one backed by a registry reads a snapshot (8.3), and the
matching rule (compare the whole origin with known values; `ends_with` and
`contains` allow look-alike hosts). "Pitfalls" gets the credentials sentence and the
second note of 8.3: CORS is not access control, check the origin where the
tenant is resolved.

## 9. Part 4: EdDSA tokens (`sekvent-auth`)

### 9.1 What the pinned library gives

`jsonwebtoken` 11.1.0 is pinned with `default-features = false` and
`rust_crypto`. EdDSA (Ed25519 only) signs and verifies there. Its PEM
loaders (`from_ed_pem`) need the `use_pem` feature, which is off and stays
off: it adds an ASN.1 stack for two fixed-size keys. What is left:

| Call | Takes |
|---|---|
| `EncodingKey::from_ed_der(&[u8])` | the PKCS#8 DER of the private key |
| `DecodingKey::from_ed_der(&[u8])` | despite its name, the **raw 32-byte public key**: the verifier reads the first 32 bytes, and slicing a shorter input panics |
| `jwk::Jwk::from_encoding_key(&key, Algorithm::EdDSA)` | a 48-byte PKCS#8 key; checks it and returns the public key as base64url in `OctetKeyPairParameters::x` |
| `crypto::verify(signature, message, &key, Algorithm::EdDSA)` | `Err` when the 32 bytes are not a valid public key |

So this part frames the keys itself and needs no new cryptographic
dependency: `base64` for the PEM body and `zeroize` for private key text
and bytes, both already in the workspace and both fine on
`wasm32-unknown-unknown`. The core stays runtime-free; signing needs no
randomness.

**The library does not refuse weak public keys.** Its `rust_crypto`
backend (`src/crypto/rust_crypto/eddsa.rs`) builds an `ed25519-dalek`
`VerifyingKey` with `from_bytes` and checks signatures with the
`Verifier` trait's `verify`, never `verify_strict`. In the locked
`ed25519-dalek` 2.2.0 that path does not look at the order of the public
key, and `from_bytes` decodes encodings RFC 8032 refuses (a y-coordinate
at or above the field prime, or x = 0 with the sign bit set). A verifier
that holds a key of small order accepts tokens nobody signed: under the
identity point (`01` followed by 31 zero bytes) the 64-byte signature
`01 00 … 00` verifies for every message. A public key is configuration,
so this is a misconfiguration and not a remote attack, but a silent one.
9.3 refuses such keys when they are loaded.

Both Ed25519 encodings have a fixed size and a fixed prefix, which makes
an exact comparison a complete parser:

| Key | DER | Prefix (hex) |
|---|---|---|
| private, PKCS#8 v1 (`-----BEGIN PRIVATE KEY-----`) | 48 bytes | `302e020100300506032b657004220420`, then the 32-byte seed |
| public, SubjectPublicKeyInfo (`-----BEGIN PUBLIC KEY-----`) | 44 bytes | `302a300506032b6570032100`, then the 32-byte key |

### 9.2 Public API

```rust
// crates/sekvent-auth/src/jwt/eddsa.rs, re-exported from `jwt` and the crate root

/// Length of a raw Ed25519 public key and of a private seed, in bytes.
pub const ED25519_KEY_LEN: usize = 32;

/// An Ed25519 private key that signs EdDSA tokens under one key id.
#[derive(Clone)]
pub struct EdDsaSigningKey { /* kid, jsonwebtoken::EncodingKey, public key */ }

impl EdDsaSigningKey {
    /// From the PKCS#8 PEM of the private key.
    pub fn from_pkcs8_pem(kid: impl Into<String>, pem: &Secret) -> Result<Self, AppError>;
    /// From the 32-byte seed (RFC 8032 private key).
    pub fn from_seed(kid: impl Into<String>, seed: &[u8; ED25519_KEY_LEN]) -> Result<Self, AppError>;
    /// The key id new tokens carry.
    pub fn kid(&self) -> &str;
    /// The raw public key.
    pub fn public_key(&self) -> [u8; ED25519_KEY_LEN];
    /// The public key as SubjectPublicKeyInfo PEM.
    pub fn public_key_pem(&self) -> String;
    /// Keys that verify what this key signs, under its key id.
    pub fn verification_keys(&self) -> EdDsaVerificationKeys;
    /// Sign `claims`, setting `kid` in the header.
    pub fn issue<T: Serialize>(&self, claims: &Claims<T>) -> Result<String, AppError>;
}

/// Ed25519 public keys that verify EdDSA tokens, by key id.
#[derive(Clone)]
pub struct EdDsaVerificationKeys { /* Vec<(String, jsonwebtoken::DecodingKey)> */ }

impl EdDsaVerificationKeys {
    pub fn from_public_key_pem(kid: impl Into<String>, pem: &str) -> Result<Self, AppError>;
    pub fn from_public_key(kid: impl Into<String>, key: &[u8; ED25519_KEY_LEN]) -> Result<Self, AppError>;
    /// Also accept tokens signed under `kid`.
    pub fn with_public_key_pem(self, kid: impl Into<String>, pem: &str) -> Result<Self, AppError>;
    pub fn with_public_key(self, kid: impl Into<String>, key: &[u8; ED25519_KEY_LEN]) -> Result<Self, AppError>;
    /// Every key id accepted.
    pub fn kids(&self) -> impl Iterator<Item = &str>;
    /// Verify `token` at `now_unix_secs`.
    pub fn verify<T: DeserializeOwned>(
        &self,
        token: &str,
        now_unix_secs: u64,
        validation: &Validation,
    ) -> Result<Claims<T>, TokenRejected>;
}

// Debug for both: key ids only.
```

`jwt.rs` declares `mod eddsa;` and
`pub use eddsa::{ED25519_KEY_LEN, EdDsaSigningKey, EdDsaVerificationKeys};`;
`lib.rs` adds the two types to its `pub use jwt::{..}`. `Claims`,
`Validation` and `TokenRejected` are shared with HS256 and unchanged. No
feature, no configuration key.

There are two types on purpose: a verifier is built from public keys and
has no way to sign. There is no empty verifier: the first key comes with
the constructor, as with `JwtKeys::hs256`.

### 9.3 Loading keys

| Input | Accepted |
|---|---|
| private key | the PKCS#8 PEM that `openssl genpkey -algorithm ed25519` writes, in a `Secret`; or its base64 body alone on one line, for environments that dislike line breaks |
| public key | the SubjectPublicKeyInfo PEM that `openssl pkey -pubout` writes, as plain text (it is not a secret); or its base64 body alone; or the raw 32 bytes |
| seed | 32 bytes, for tests and for a key the caller derives |

Parsing, the same for both PEM kinds: trim; when the text starts with
`-----BEGIN `, the first line must be exactly the expected label line and
the last non-blank line the matching `-----END` line, and the lines
between are the body, otherwise the whole text is the body; ASCII
whitespace is removed; the body is decoded with the standard base64
alphabet and its padding; the result must have the size and the prefix of
9.1. Anything else (an encrypted key, an OpenSSH key, an RSA key, a PKCS#8
key that carries its public half, base64url) is refused.

**Private key material is wiped at every step, text included.** The
`Secret` wipes its own text. From there the loader makes no copy that it
does not wipe:

- The label check and the body lines are borrowed slices of the secret.
  Nothing is `to_owned`, joined, `collect`ed into a `String` or a `Vec`,
  or formatted.
- The body without its whitespace, the normalized base64 text, is exactly
  64 characters for the 48-byte key. It is gathered into a
  `Zeroizing<[u8; 64]>`. A 65th character is a refusal, not a
  reallocation that would leave a copy behind; fewer than 64 is a refusal
  too.
- It is decoded with `Engine::decode_slice` into a `Zeroizing<[u8; 48]>`,
  so the decoder never allocates either.
- `from_seed` builds its PKCS#8 wrapping in the same kind of buffer.
- `EncodingKey::from_ed_der` copies the 48 bytes once more, into a key
  that wipes itself on drop and prints `[redacted]`.

What the library copies while it signs is its own (its `ed25519-dalek`
key wipes itself on drop).

**Public key text goes through the same buffers.** A public key is not
secret, but the text handed to a verifier may be a private key pasted by
mistake, and a configuration value has no size limit. So the verifier's
loader borrows the body as above, gathers it into a
`Zeroizing<[u8; 60]>` (the 44-byte key in base64, with its one `=`) and
decodes it with `decode_slice` into a `Zeroizing<[u8; 44]>`. A 61st
character is a refusal before anything is allocated, so an oversize body
costs one scan and no copy; a private key PEM fails the label check, its
bare 64-character body the length check, and neither leaves a copy behind.

A key is checked when it is loaded, so a bad one is a startup error and
never a verifier that silently refuses everything, or accepts anything:

- private: `Jwk::from_encoding_key` must accept it; its `x` is the public
  key the type keeps;
- public: exactly 32 bytes. Two checks on the bytes alone come first, with
  no curve arithmetic and no new dependency, because the library makes
  neither (9.1):

  1. **Canonical**, as RFC 8032 section 5.1.3 decodes a point. With the
     top bit of the last byte cleared, the little-endian number must be
     below the field prime 2^255 − 19: refused when `byte[0] >= 0xed`,
     bytes 1 to 30 are all `0xff` and `byte[31] & 0x7f == 0x7f`.
  2. **Not of small order.** With the top bit of the last byte cleared,
     the bytes must be none of these five, the y-coordinates of the eight
     points of small order:

     | Order | y, 32 bytes little-endian, top bit clear |
     |---|---|
     | 1 | `0100000000000000000000000000000000000000000000000000000000000000` |
     | 2 | `ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f` |
     | 4 | `0000000000000000000000000000000000000000000000000000000000000000` |
     | 8 | `26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05` |
     | 8 | `c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a` |

  Then the 32 bytes are handed to `DecodingKey::from_ed_der`, and
  `crypto::verify("", b"", &key, Algorithm::EdDSA)` must not fail (an
  invalid point is an `Err`, a mere mismatch is `Ok(false)`).

Together the two checks refuse all fourteen encodings the library would
decode to a point of small order: the eight canonical ones and the two
with x = 0 and the sign bit set (`01 00 … 00 80`, `ec ff … ff ff`) by the
second check, and `ed ff … ff 7f`, `ed ff … ff ff`, `ee ff … ff 7f`,
`ee ff … ff ff` (y at or one above the prime) by the first, which also
refuses every other y at or above the prime. The table and the count were
computed from the curve equation, not copied. The comparison need not be
constant-time: public keys are public.

A key derived from a seed needs no such check: it is the base point times
a clamped scalar, which is a multiple of eight below eight times the
group order and never zero, so the point is never of small order.
`verification_keys()` therefore cannot fail.

`from_seed` wraps the seed in the PKCS#8 prefix and continues like the
PEM path. That, with `verification_keys()`, is the key-pair helper tests
need: deterministic, no files, no randomness. A random generator is not
part of this.

### 9.4 Issuing and verifying

`issue`: `Header::new(Algorithm::EdDSA)` with `kid`, then
`jsonwebtoken::encode`; a failure is `INTERNAL`, as for HS256.

`verify`, the same order and the same answers as `JwtKeys::verify`:

| Step | Refusal |
|---|---|
| the header does not parse, or names an algorithm the library does not know (`none`) | `Malformed` |
| the header's `alg` is not `EdDSA` (an HS256 token, any other known algorithm) | `BadSignature` |
| no `kid`, or a `kid` that names no key | `UnknownKey` |
| the signature does not verify with that key | `BadSignature` |
| the claims do not deserialize | `Malformed` |
| `exp`, `nbf`, `iat` against `now_unix_secs` and the leeway | `Expired`, `NotYetValid` |
| `Validation::issuer`, `Validation::audience` when set | `WrongIssuer`, `WrongAudience` |

The algorithm is pinned twice: by the check on the header and by handing
the library a validation that lists `EdDSA` alone with a key of the Ed
family. The reverse holds already: `JwtKeys::verify` refuses any header
that is not `HS256` before it looks at a key. To share the code, `jwt.rs`
gives `signature_only` an `Algorithm` parameter and moves the time, issuer
and audience checks of `JwtKeys::verify` into one private function both
verifiers call; the key-id check (`blank`, `configured twice`) is shared
the same way.

### 9.5 Errors

Construction, all `INVALID_ARGUMENT`, naming the key id and never the key:

| Condition | Message |
|---|---|
| blank key id | `JWT key id must not be blank` (the HS256 text) |
| a key id given twice to one verifier | ``JWT key id `<kid>` is configured twice`` (the HS256 text) |
| a private key that is not what 9.3 accepts | ``JWT key `<kid>` is not an Ed25519 private key in PKCS#8 PEM`` |
| a public key that is not what 9.3 accepts, not canonical, or not a point of the curve | ``JWT key `<kid>` is not an Ed25519 public key`` |
| a public key of small order | ``JWT key `<kid>` is a weak Ed25519 public key`` |

Verification: `TokenRejected`, unchanged, every variant
`UNAUTHENTICATED` with the one caller-safe message. No new reason.

### 9.6 Tests

Unit tests in `jwt/eddsa.rs`, with the first two key pairs of RFC 8032
section 7.1 as constants:

| | Seed | Public key |
|---|---|---|
| A | `9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60` | `d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a` |
| B | `4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb` | `3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c` |

Pair A as `openssl` writes it (checked against `openssl pkey -pubout`):
the private PEM body is
`MC4CAQAwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g`, the
public one `MCowBQYDK2VwAyEA11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=`.
These are the RFC's published test keys, safe to commit. A 32-byte value
that is no public key: `02` followed by 31 zero bytes (no point of the
curve has that `y`).

A token signed outside sekvent, the known answer for both directions. It
was made with OpenSSL 4.0 (`openssl pkeyutl -sign -rawin -inkey key.pem`,
`key.pem` being pair A's private PEM, over the ASCII bytes of
`<header>.<payload>`, each the unpadded base64url of the JSON below) and
confirmed with `openssl pkeyutl -verify` and with a separate RFC 8032
implementation, which produced the same signature:

| | |
|---|---|
| key | pair A, key id `k1` |
| header | `{"typ":"JWT","alg":"EdDSA","kid":"k1"}` |
| claims | `{"sub":"user-1","iat":1700000000,"exp":1700000060,"iss":"billing","aud":"orders"}`, which is `Claims::new("user-1", 1_700_000_000, 60, NoClaims {}).with_issuer("billing").with_audience("orders")` |
| signature (hex) | `2846cd4d7c8da5a4c980a7ab1e35e111a249cf8623a6bce209fd96f02e538bbc21f3da5594f53429c97527045eb593b55fc55e4ea2d091fbc546b10deb31f505` |

The token, one line:

```text
eyJ0eXAiOiJKV1QiLCJhbGciOiJFZERTQSIsImtpZCI6ImsxIn0.eyJzdWIiOiJ1c2VyLTEiLCJpYXQiOjE3MDAwMDAwMDAsImV4cCI6MTcwMDAwMDA2MCwiaXNzIjoiYmlsbGluZyIsImF1ZCI6Im9yZGVycyJ9.KEbNTXyNpaTJgKerHjXhEaJJz4YjprziCf2W8C5Ti7wh89pVlPU0Kcl1JwRetZO1X8VeTqLQkfvFRrEN6zH1BQ
```

The JSON is byte for byte what the code under test writes: `jsonwebtoken`
serializes its header as `typ`, `alg`, `kid`, and `Claims` serializes its
fields in declaration order and leaves unset ones out.

- Known answer, keys: `from_seed(A)` has public key A; `public_key_pem()`
  is the base64 of the 12-byte prefix and the key between the label lines,
  and loads back into a verifier.
- Known answer, verifying: `EdDsaVerificationKeys::from_public_key("k1",
  &A)` accepts the token above at `now = 1_700_000_030` with
  `Validation::new().with_issuer("billing").with_audience("orders")` and
  returns exactly those claims.
- Known answer, signing: `from_seed("k1", &seed_A)?.issue(&claims)?` is
  that token, character for character, and so is the token of
  `from_pkcs8_pem` with pair A's PEM. Ed25519 signatures are
  deterministic, so any difference is a bug in the framing of the key or
  of the token.
- Round trip with and without custom claims; the header carries
  `alg: EdDSA` and the `kid`; `from_pkcs8_pem` of the PEM built from seed A
  signs what a verifier of key A accepts, with the label lines and as a
  bare body, with `\n` and `\r\n` line ends.
- The checks: expiry, not-before and issued-at boundaries, issuer and
  audience, with the values the HS256 tests use.
- Rotation: a verifier with A and B accepts both; a token under an unknown
  `kid` and one without a `kid` are `UnknownKey`; key B registered under
  A's `kid` is `BadSignature`.
- Algorithm confusion, both ways: an HS256 token from `JwtKeys` offered to
  the verifier is `BadSignature`; so is an HS256 token signed with public
  key A as the HMAC secret under A's `kid`; an EdDSA token offered to
  `JwtKeys::verify` is `BadSignature`; `alg: none` is `Malformed`.
- Tampering: a changed payload, a changed signature, a truncated token.
- Loading: every refusal of 9.5 (wrong label, an encrypted-key label,
  garbage base64, 47 and 49 bytes, a wrong prefix, a public PEM given as
  private and the reverse, 32 bytes that are no curve point, a private
  body of 63 and of 65 characters; for the verifier a private key PEM, as
  is and relabelled `PUBLIC KEY`, its bare body, a public body of 59 and of
  61 characters, and a 1 MiB body bare and between the label lines), each
  with the key id in the message and nothing of the input.
- Weak keys: each of the fourteen encodings of 9.3 is refused by
  `from_public_key` and, wrapped in the SubjectPublicKeyInfo prefix, by
  `from_public_key_pem`, also as a second key through `with_public_key`:
  the ten of the table (five values, either sign bit) with the "weak"
  message, the four at or above the prime with the "not an Ed25519 public
  key" message. `ff` repeated 31 times and then `7f` (a y above the prime
  that is no small-order point) is refused as not canonical. Keys A and B
  pass. Nothing built from a seed is refused.
- `Debug` of both types shows key ids and no key; `kids()`.

### 9.7 Docs

`docs/modules/auth.md`: the intro and "Concepts" name both algorithms; a
section `### Sign with a private key, verify with public keys (EdDSA)`
(that exact heading) after "Issue and verify JWTs" with the API, the
`openssl` commands, the accepted inputs of 9.3, rotation by `kid`, the
table of 9.4 and when to choose it over HS256 (a verifier that must not be
able to mint); "Configuration keys" still says none and shows the two keys
in a config struct (`Secret` for the private one); the rows of 9.5 in
"Errors and reasons", with one sentence on why a weak or non-canonical
public key is refused at load (a verifier holding one would accept forged
tokens); testing tips (`from_seed`, `verification_keys`); pitfalls: the
private key only in a `Secret`, never a private key in a verifier's
configuration, short lifetimes and a `jti` the receiver records when a
token must work once, `BearerAuth` is HS256 only.

## 10. Where the requirements and the code disagree

1. **There is no `Unit` trait.** A runtime unit is a factory
   `FnMut(UnitContext) -> Future`, and `Server::into_unit` is the pattern.
   The reaper is `Keyed::into_unit(self)`, which returns such a factory,
   and it also closes the cache when its stage drains.
2. **"Lease" is taken.** `sekvent-db` already exports `Lease`,
   `LeaseStore`, `HeldLease` and `LeaseGuard` for database leases. The
   handle `get` returns is `KeyedRef<V>`, the docs say "reference" and "in
   use", and the stats field is `in_use`.
3. **The request scope needs nothing from `sekvent-runtime`.** The
   component service builds its own context from the request head and
   never reads the one the runtime server attaches, and the hook must work
   under any server that mounts `App::grpc_routes()`. It is an App hook
   like the authenticator and the authorizer. REST routes call
   `App::resolve_scope`.
4. **`with_end_user` overwrites the tenant**, and clears it for a user
   without one, so a tenant set before authentication would be lost. 7.3
   and 7.6 change that and define the precedence.
5. **The authenticator sees only `&http::request::Parts`.** Rather than
   change `EndUserAuthenticator`, the scope is put into the head's
   extensions, which the trait's documentation already names as its way to
   reuse an earlier decision.
6. **"Served call" is narrowed.** The resolver does not run for link
   callers or in-process calls (7.4); the requirement left both open.
7. **A resolver error passes unchanged**, as required, which differs from
   the authenticator's `UNAUTHENTICATED`, flattened to one answer unless
   the authenticator opted out for that error (7.10). The one answer of
   this part that is always flattened is the tenant mismatch, because it
   is a verdict on the credential.
8. **`AppError` is not `Clone`.** Waiters of one creation each get the
   caller-visible part through `WireError`. The original, with its source,
   stays with the creation's task, which logs it once (6.3).
9. **`PgConnectOptions` derives `Debug` with the password, and
   `PgConnectOptions::new()` reads `PG*` variables.** "Nothing that can
   log a password" is therefore a rule for this module (never format the
   options, never keep the driver's connect error) and two pitfalls in the
   docs, not a property of the type. `classify_connect` already reduces a
   connect error to a class without its text, and `registry.rs` logs the
   class alone; the keyed pools keep that and add the server's SQLSTATE.
10. **The creation deadline is `UNAVAILABLE`**, not `DEADLINE_EXCEEDED`:
    it is not the caller's deadline, and the crate already reports a pool
    that cannot connect in time that way.
11. **`Cors` derives `PartialEq`, `Eq` and `Debug`.** The function is
    wrapped so the derives survive (8.2).
12. **`jsonwebtoken` has no PEM support as pinned, and
    `DecodingKey::from_ed_der` wants the raw key.** Part 4 frames the two
    fixed-size encodings itself (9.1) instead of enabling `use_pem` or
    adding `ed25519-dalek` directly, which would need either a second copy
    of the curve code or a pin behind its latest release.
13. **"The same mandatory checks as HS256"** means the same `Validation`:
    expiry always, issuer and audience when it names them. Neither
    verifier forces them.
14. **`sekvent-db` reaches neither `Clock` nor `truncate_for_log`.** They
    live in `sekvent-context` and `sekvent-telemetry`, and the crate map
    gives `sekvent-db` `sekvent-config`, `sekvent-error` and an optional
    `sekvent-runtime`. No dependency is added: idle time stays on tokio's
    clock, with `evict_idle_older_than` for tests on the real clock, and
    the failure log cuts its text itself (6.3).
15. **What `sqlx` does around a pool.** `PoolOptions::connect_with` opens
    one connection and gives up after `acquire_timeout`; it retries a
    refused connection until then, so an address nobody listens on is a
    timeout and not an immediate failure; a rejected password comes back
    at once as a database error with SQLSTATE `28P01`. Dropping the last
    handle of a pool marks it closed, and fires `close_event`, without
    closing its connections in order; dropping one handle while a clone
    lives closes nothing. Hence the explicit `close()` on every path, the
    guard of 6.4, and tests that hold a clone to tell a closed pool from a
    dropped one.
16. **`Server::open_stream` drops the stream's context when the open
    returns**, and tonic's request, whose extensions held the other copy
    of the scope, is consumed before the open runs. A scope value would be
    released under a running stream whose producer did not capture the
    context. `Live` holds the scope until the stream ends (7.5).
17. **`App::register` fixes its unit's name**, and
    `RuntimeBuilder::build` refuses a name used twice, so one runtime
    could not run two Apps. `register_as` takes the name (7.9).
18. **`rejection()` flattens every `UNAUTHENTICATED`, and `AppError` has no
    marker for a field that may be shown.** All of an `AppError` but its
    source is caller-visible by definition; the end-user boundary is the
    one place that strips it. The opt-in is a typed value in the source
    slot, read by that boundary alone, and `sekvent-error` does not change
    (7.10).
19. **`jsonwebtoken` verifies EdDSA with `verify`, not `verify_strict`,**
    and decodes public keys RFC 8032 refuses. A key of small order would
    verify forged tokens. Part 4 refuses such keys, and non-canonical
    encodings, when they are loaded, by comparing bytes (9.1, 9.3).
20. **`App::resolve_scope` gets `&http::request::Parts`.** It cannot put
    the scope into the request's extensions as the component service
    does, and it is not an authentication step. Both are stated, and the
    tenant check is offered as `admit_end_user` (7.5).

## 11. Decisions

### 11.1 Decided

1. **No scope for link callers and in-process calls** (7.4).
2. **A user of another tenant is refused by the framework** (7.6), not
   left to each authenticator.
3. **The CORS function is synchronous** (8.3).
4. **One cache holds the value built on the pool.** `KeyedPools<K, V>`
   runs `build_value` after `init`, keeps the pool beside the value and
   closes it at disposal (6.4). A second cache stacked on the first, its
   values holding a `KeyedRef` of the first, is not the pattern: it gives
   one key two capacities, two idle times and two eviction orders.
5. **A lookup that never creates.** `peek` returns a warm value only and
   does not count as a use (6.3).
6. **Ordered connect candidates**, the next one tried only when the server
   rejected the password (6.4).
7. **A creation carries a generation**, and a stale one neither publishes
   over nor removes a newer entry; its result is disposed (6.3).
8. **Streams keep the request scope until they end** (7.5).
9. **`App::register_as`**: one runtime runs several Apps (7.9).
10. **An authenticator may show a reason**, one error at a time; the
    constant answer stays the default (7.10).
11. **Weak public keys are refused when they are loaded** (9.3).

### 11.2 Taken in this note, open to override

1. **`Keyed` lives in `sekvent-db`** although it is generic (6.1).
2. **Creation runs on its own task and survives its callers.** The
   alternative, cancelling it with the last waiter, restarts a slow
   migration every time a client gives up.
3. **Capacity counts what the cache holds.** Retiring values are outside
   it and visible in `stats().retiring`; a bound on resources needs
   headroom (6.3).
4. **A stale creation's outcome reaches nobody, and its waiters move to
   the key's current creation** (6.3). The alternative, handing them the
   stale value and disposing it when they let go, gives callers a value
   that was just declared obsolete.
5. **`invalidate` does not cancel the creation it makes stale; `close`
   does.** Cancelling on `invalidate` would cut an `init` (a migration)
   short on an operator's action; letting it end costs at most
   `create_timeout` of one creation beside the fresh one.
6. **`28000` gets no fallback**, only `28P01` (6.4).
7. **`evict_idle_older_than` instead of an injected clock** (6.3): it is
   deterministic on any clock, needs no dependency and is useful outside
   tests.
8. **`close()` has no bound of its own.** The unit bounds it at nine
   tenths of its stop window, logs what it abandons and never fails
   (6.3).
9. **The failure log is `warn` with code, reason and source chain**, and a
   connect failure shows a class and a SQLSTATE, never the driver's text
   (6.3, 6.4).
10. **No disposal hook for a `KeyedPools` value**: it is dropped, then its
    pool is closed.
11. **The resolver gets the context as well as the head**: the deadline,
    the request id and the client address are useful there, and a closure
    that ignores it costs nothing.
12. **`App::resolve_scope` does not authenticate, and the tenant check is
    a function**, `admit_end_user`, which the component service calls
    too (7.5). The alternative, a check hidden inside
    `CallContext::with_end_user`, would need that method to fail.
13. **`EndUserRejection` rides in the error's source and takes
    `&'static str`** (7.10). The alternative, a flag on `AppError`, puts a
    notion one boundary reads into the type every crate shares; `String`
    reasons would let request data through.
14. **The tenant refusal never carries a reason** (7.6).
15. **`register_as` checks no name and promises no order between two
    Apps** (7.9); the runtime's own checks apply.
16. **Credentials are allowed with a CORS function**, as with a list.
17. **`null` is never passed to the function.** A sandboxed page cannot be
    allowed by it; the list cannot allow it either.
18. **Two EdDSA types, no `BearerAuth` support.** One `JwtKeys` that
    sometimes cannot sign would change `signing_kid` and `issue`.
19. **A PKCS#8 key that also carries its public half is refused.** The
    library's own derivation accepts the 48-byte form only.
20. **Non-canonical public keys are refused with the weak ones**, by
    bytes, without a curve dependency (9.3).

## 12. Verification

One run over the combined tree, never two at once, with the commands of
`AGENTS.md` ("Verification"): `cargo fmt --all` on the Mac, then on the
remote builder `rrb run clippy` (every lint at once), `rrb run gate`
(format check, clippy, the tests with Docker, doctests) and
`rrb run coverage`.

Failures go back out grouped by the files of section 5. Nothing in this
note regenerates snapshots, `.stderr` files or contract baselines: no
macro and no proto changes.
