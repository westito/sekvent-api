# Tenants on a database each (`sekvent-tenancy`)

A new crate, `sekvent-tenancy`: the layer above the multi-tenant building
blocks that a product compiles in to serve many tenants from one process,
each tenant with a Postgres database and a database role of its own. It
picks the tenant from the request head, opens that tenant's pool with a
derived role password, migrates on first open, caches, follows changes made
on other processes, and carries the lifecycle (provision, suspend, resume,
set hosts, purge, adopt) with an admin surface an operator service calls.

> Status: **slices 1a and 1b are built; 1c is a draft.** Builds on
> [`multi-tenant-building-blocks.md`](multi-tenant-building-blocks.md),
> [`serve-admission.md`](serve-admission.md),
> [`component-end-user.md`](component-end-user.md) and
> [`p8-service-essentials.md`](p8-service-essentials.md). Where this note
> is more specific, it wins. The note is also the work order: three slices
> landing on `master` one by one (section 5), each opened by a step 0 the coordinator
> does alone (section 6). Slices 1a and 1b (sections 8 and 9) describe
> what is built; 1c (section 10) fixes behaviour and the shape of the API
> and marks the signatures its own step 0 pins.

## 1. Problem

The building blocks give a pool per key (`KeyedPools`), a scope resolved
before authentication (`ScopeResolver`, `RequestScope`), CORS origins
decided by a function, EdDSA tokens, a second App on one runtime
(`App::register_as`), the serve admission and leases. A product with a
database per tenant still writes all of this by hand, and each piece has an
invariant that is easy to get wrong once and hard to notice:

1. **Names and credentials.** A role and a database per tenant need names
   that can be formatted into DDL safely, and a password per role that is
   stored nowhere.
2. **The cluster.** Roles and databases are created, checked and dropped by
   a role that is not a superuser, and no tenant role may ever reach a
   database that is not its own, the service's own databases included.
3. **Selection.** Which tenant a request names (a header on a shared API
   host, the host itself, a path segment), what happens when two selectors
   disagree, and which browser origins may call which tenant.
4. **The cache.** One scope per tenant, built once however many requests
   arrive for a cold tenant, migrated before anyone sees it, bounded in
   what an unauthenticated caller can make it cost, dropped when the tenant
   changes.
5. **Other processes.** A tenant suspended on one process must stop being
   served on the others, without a call between them.
6. **Lifecycle.** Provisioning is several steps across three connections
   and can fail between any two; purging destroys data and must never take
   a database that is not the tenant's.
7. **An operator.** Lifecycle calls come from another service, whose
   tokens the product can verify and never mint, each usable once.

A product's working implementation of exactly this exists and has been
through a security review. This crate is a generalised rewrite of its
behaviour, specified here in neutral terms; nothing of a product (code,
comments, names, hosts, lists, fixtures) is carried over.

## 2. Decisions

Binding; taken by the repository owner before this note.

1. **An embedded library.** The request path (select, look up, open the
   pool, migrate on first open, cache, CORS, invalidation) runs in the
   product process and never calls another service. A product-neutral
   standalone supervisor for the lifecycle is feasible and deferred; to
   keep it an additive step the lifecycle half has two layers:
   **cluster operations** (roles, databases, privilege checks, preflight,
   the drop of a purge, the ownership handover of an adoption) that take
   names and verifiers only and know no tenant row, and hook-driven
   **tenant operations** on top.
2. **Four features, no new third-party dependency.** `serving` (default),
   `lifecycle`, `admin`, `manager`, mirrored on the facade as `tenancy`,
   `tenancy-lifecycle`, `tenancy-admin`, `tenancy-manager`. HKDF-SHA-256
   and the PBKDF2 of a SCRAM-SHA-256 verifier are written in the crate over
   the pinned `hmac` and `sha2`; LISTEN/NOTIFY comes with the pinned sqlx.
3. **No configuration keys.** The crate reads none; it offers parsers and
   builders the product calls (the rule of the building-blocks note).
4. **The product owns its control schema.** The migration files of the
   control database are the product's. The crate ships a reference schema
   as text, verifies the shape at startup (as `sekvent-db` does for the
   lease table) and tolerates extra product columns on the tenants table.
5. **The admin surface is a library.** A token verifier, an end-user
   authenticator for an admin App and a guard a product wraps each of its
   own admin RPC bodies in. No service contract ships from sekvent in this
   milestone. `manager` is the other side: a token minter and a client that
   calls a product's admin RPC by its path.
6. **The scope may be expensive.** The API is validated against a second
   kind of product whose tenant scope takes seconds to build (it loads the
   tenant's working set into memory) and must have one owning process at a
   time. Therefore: the scope build is async and fallible; lifecycle hooks
   receive the tenant's info and a pool, never a built scope; a secret a
   hook returns once is a `Secret`, never bare bytes.

## 3. Scope

### In

- Tenant ids, object names, host and label canonicalisation.
- Role secrets: derivation by key id and generation, the SCRAM verifier,
  its syntax check.
- The connector, the maintenance connection, roles, databases, privilege
  inspection, the startup preflight, the ownership handover.
- Tenant info and states, the control registry and its reference schema,
  the lookup cache and the lookup admission, selector rules, the
  `ScopeResolver`, the host snapshot and the CORS predicate.
- The tenant cache (`Tenants`), fixed or keyed; the lease a call holds; the
  id-only variant for streaming methods; walks over active tenants and the
  migration sweep.
- The cross-process invalidation channel: sender, listener, resync.
- Provisioning as resumable steps, suspend, resume, set hosts, purge,
  adopt, usage; the lifecycle admission; announcing a committed change.
- The admin token verifier, authenticator and guard; the manager kit.

### Out

| Item | Note |
|---|---|
| Operators, their accounts and sessions; a registry of record; orchestration and reconcile policy; name claims and holds; signup; mail; a console | an operator service's; it uses the `manager` kit |
| A generic admin service contract shipped by sekvent | a later step, added when a second product needs the same contract; until then each product declares its own admin proto and wraps it in the guard (10.6) |
| A standalone supervisor binary; any binary; deployment | decision 1; sekvent ships no service |
| Routing tenants across clusters or edge proxies; several Postgres clusters per product | the control row keeps a cluster column with one value (9.2) |
| Role-password rotation | the seam exists: a key id and a generation per tenant row, and `Connector::candidates` takes several keys (8.5) |
| **Single-owner tenants** | the seam: `ScopeFactory::build` is async and fallible, so a product takes a lease in its control database (`sekvent_db::LeaseStore`) there and refuses with a reason of its own, which reaches the caller unchanged (9.6). Routing a request to the owning process is the product's |
| **Cache capacity by weight** | `KeyedPools` counts entries. The seam: `Tenants` exposes `stats()` and `evict_idle_older_than`, so a product sheds on its own measure until a weight lands in `sekvent-db` |
| **An eviction callback** | a scope ends its tasks in `Drop` (the building-blocks rule: the value is dropped, then its pool is closed). The seam is `KeyedPools`' disposal, a `sekvent-db` change |
| MySQL | Postgres 16 or later only (8.6) |

### Left to the product

Every configuration key; the tenant schema and its migrator; the control
migration files; the scope type and what it holds; the selector header name
and the path prefix; the hosts that select; the object-name prefix; the
HKDF context string; the NOTIFY channel name; the error domains; the token
issuer, audience, subject grammar and extra claims; the admin proto; the
reserved-label list; the CORS method, header and max-age policy; readiness
probes; jobs and their leases; the role connection limit and the tenant
cap.

## 4. Layout

### 4.1 Crate and features

`crates/sekvent-tenancy`, `#![forbid(unsafe_code)]`, lib `sekvent_tenancy`,
facade module `sekvent::tenancy`.

| Crate feature | Facade feature | Adds |
|---|---|---|
| (none) | either below | `TenantId`, `ObjectNames`, `TenantRole`, `DatabaseName`, key ids, `host`, `label`, `reasons`, `ErrorDomains` |
| `serving` (default) | `tenancy` | role secrets (derivation), `Connector`, `privileges`, `preflight`, tenant info, the control registry, selection, `TenantResolver`, `HostSnapshot`, `Tenants`, the invalidation channel |
| `lifecycle` | `tenancy-lifecycle` | `ScramVerifier`, `Maintenance`, `roles`, `databases`, `handover`, `Provisioner` and the tenant operations (needs `serving`) |
| `admin` | `tenancy-admin` | `Verifier`, `AdminClaims`, the authenticator, `AdminAudit`, `guard` (needs `serving`, not `lifecycle`: a product admin RPC that changes no tenant state uses the same guard) |
| `manager` | `tenancy-manager` | `Minter`, `AdminClient`, `CallOutcome`; no database dependency, usable without `serving` |
| `runtime` | forwarded by the facade's `runtime` | `Tenants::into_unit`, the listener as a runtime unit |

`privileges` and `preflight` sit in `serving`, not `lifecycle`: a process
that serves tenants without holding the maintenance credential still
checks at startup that no tenant role reaches its control database.

### 4.2 Dependencies

Every line takes the version from the root manifest's pins, like every
other crate; none is new to the repository.

| Dependency | Optional | Enabled by | For |
|---|---|---|---|
| `sekvent-error`, `sekvent-config`, `uuid`, `thiserror` | no | | `AppError`, `Secret`, ids |
| `sekvent-context` | yes | `serving`, `manager` | `Clock`, `CallContext`, `RequestScope` |
| `sekvent-db` (features `sqlx-postgres`, `keyed`, `migrate`) | yes | `serving` | `KeyedPools`, `ConnectCandidates`, embedded migrations, `to_app_error`, `sqlstate`, `constraint_name` |
| `sekvent-component` (path form, default features off) | yes | `serving` | `ScopeResolver`, `EndUserAuthenticator`, `EndUserRejection`, `BearerShape` |
| `sekvent-telemetry` | yes | `serving` | `truncate_for_log` |
| `sekvent-runtime` (path form, default features off) | yes | `runtime` | `UnitContext` |
| `sekvent-auth` | yes | `admin`, `manager` | `EdDsaVerificationKeys`, `EdDsaSigningKey`, `Claims`, `Validation` |
| `sekvent-client` | yes | `manager` | the HTTP stack on rustls |
| `sqlx` (Postgres), `chrono`, `tokio`, `tokio-util`, `futures`, `http`, `tracing` | yes | `serving` | pools, `PgListener`, timestamps on the wire to Postgres |
| `hmac`, `sha2`, `base64`, `zeroize` | yes | `serving` | HKDF, PBKDF2, encodings, wiping key material |
| `getrandom` | yes | `lifecycle` | the SCRAM salt |
| `serde`, `serde_json` | yes | `admin`, `manager` | claims |
| `prost`, `bytes` | yes | `manager` | request and reply messages, gRPC-Web frames |

Dev: `sekvent-testing` (feature `postgres`), `tokio` (`test-util`),
`sekvent-component` (features `runtime`, `grpc`) and `sekvent-runtime`
(feature `grpc-web`) for the tests that serve an App.

Crate-map row (`AGENTS.md`), placed after `sekvent-component`:

```markdown
| `sekvent-tenancy` | tenants on a database and a role each: ids and object names, derived role credentials, cluster operations and the startup preflight, tenant selection and the tenant cache, cross-process invalidation, lifecycle operations, admin tokens and the manager kit | config, error, context, db, component, telemetry, runtime (optional), auth (optional), client (optional) |
```

### 4.3 Modules

| Module | Files under `crates/sekvent-tenancy/src/` | Feature | Slice |
|---|---|---|---|
| `id` | `id.rs` | none | 1a |
| `names` | `names.rs` | none | 1a |
| `host` | `host.rs` | none | 1a |
| `label` | `label.rs` | none | 1a |
| `reasons`, `ErrorDomains` | `error.rs` | none | 1a (step 0) |
| `secrets` | `secrets/mod.rs`, `secrets/kdf.rs`; `secrets/scram.rs` (`lifecycle`) | `serving` | 1a |
| `connector` | `connector.rs` | `serving` | 1a |
| `cluster::privileges` | `cluster/privileges.rs` | `serving` | 1a |
| `cluster::preflight` | `cluster/preflight.rs` | `serving` | 1a |
| `cluster::maintenance` | `cluster/maintenance.rs`, `cluster/sql.rs` | `lifecycle` | 1a |
| `cluster::roles` | `cluster/roles.rs` | `lifecycle` | 1a |
| `cluster::databases` | `cluster/databases.rs` | `lifecycle` | 1a |
| `cluster::handover` | `cluster/handover.rs` | `lifecycle` | 1a |
| `info` | `info.rs` | `serving` | 1b |
| `control` | `control/mod.rs`, `control/schema.rs`, `control/rows.rs`, `control/registry.rs`, `control/lookup.rs`, `control/admission.rs` | `serving` | 1b |
| `select`, `resolver` | `select.rs`, `resolver.rs` | `serving` | 1b |
| `hosts` | `hosts.rs` | `serving` | 1b |
| `tenants` | `tenants/mod.rs`, `tenants/keyed.rs`, `tenants/lease.rs`, `tenants/walk.rs`, `tenants/sweep.rs`, `tenants/unit.rs` (`runtime`) | `serving` | 1b |
| `invalidation` | `invalidation/mod.rs`, `invalidation/message.rs`, `invalidation/sender.rs`, `invalidation/listener.rs` | `serving` | 1b |
| `lifecycle` | `lifecycle/mod.rs`, `hooks.rs`, `provisioner.rs`, `open.rs`, `provision.rs`, `state.rs`, `purge.rs`, `adopt.rs`, `usage.rs`, `announce.rs` (all under `lifecycle/`) | `lifecycle` | 1c |
| `admin` | `admin/mod.rs`, `admin/claims.rs`, `admin/verifier.rs`, `admin/authenticator.rs`, `admin/audit.rs`, `admin/guard.rs` | `admin` | 1c |
| `manager` | `manager/mod.rs`, `manager/minter.rs`, `manager/client.rs`, `manager/frame.rs` | `manager` | 1c |

`lib.rs`, `cluster/mod.rs` and every other `mod.rs` hold declarations and
re-exports only and belong to the coordinator in every slice.

## 5. How the work is run

1. **Each slice lands on `master`** once its gate and coverage are green
   (owner's decision, answering 15, question 1). The crate is new and
   additive: a project that does not enable a `tenancy` feature compiles
   nothing of it. A slice's public items may still change in a later
   slice until the first consuming product deploys the crate; the
   consumer check (item 2.4) follows the slice on `master`.
2. **Three slices**, in this order: **1a leaves**, **1b serving half**,
   **1c lifecycle and admin**. Each slice is one full loop:
   1. *Step 0*, by the coordinator alone: every file more than one writer
      needs, the module declarations, the pinned signatures as compiling
      skeletons (`unimplemented!` bodies are not allowed on the branch
      head: step 0 of a slice lands together with its writers' work), the
      test fixture.
   2. *Writers* in parallel, at most five, each inside the files of its
      row. They write code, tests and the module-page section of their
      row, and do not build, lint or test. A writer never edits a
      manifest, the lockfile, `lib.rs`, a `mod.rs`, `crates/sekvent/**`,
      `tests/common/**`, an index page, `AGENTS.md` or `skills/**`. Each
      ends its report with what in the tree contradicted this note.
   3. *Verification*: one run of the gate and of coverage (section 17),
      then fixes grouped by file.
   4. *Consumer check*: the consuming product moves its lock to the
      slice's commit on `master`, replaces its own code of the slice by
      the crate's and runs its own suite. The slice is done when that is
      green.
3. **Sizes are re-estimated after 1a.** The plan for 1b and 1c is revised
   with what 1a taught before their step 0.

Rules every writer follows: the repository's lints (`clippy::pedantic`
with `-D warnings`, `missing_docs`, `unreachable_pub`); completion signals
and `tokio::time::pause`, never a sleep; anything with a socket runs on the
real clock under a 30 s guard; 95 % line coverage of the crate, Docker
tests included; nothing logs, formats or `Debug`-prints a master secret, a
password, a verifier, connect options or a token; every public type that
holds one prints nothing of it. The text is written from this note alone:
no file of any product is open while writing, and names, comments, test
names and fixtures are new. Before each commit the coordinator checks every
changed file against a list of terms kept outside the repository.

## 6. Step 0

### 6.1 Slice 1a, by the coordinator, in this order

1. **Root manifest.** In the internal section of the pinned-dependency
   table, after the `sekvent-storage` line:
   `sekvent-tenancy = { path = "crates/sekvent-tenancy" }`. The member
   glob `crates/*` already covers the crate.
2. **`crates/sekvent-tenancy/Cargo.toml`.** Package fields inherited like
   `sekvent-storage`'s; the dependencies of 4.2; and

   ```toml
   [features]
   default = ["serving"]
   # Selection, the tenant cache, the control registry, invalidation.
   serving = [
       "dep:sekvent-context", "dep:sekvent-db", "dep:sekvent-component",
       "dep:sekvent-telemetry", "dep:sqlx", "dep:chrono", "dep:tokio",
       "dep:tokio-util", "dep:futures", "dep:http", "dep:tracing",
       "dep:hmac", "dep:sha2", "dep:base64", "dep:zeroize",
   ]
   # The maintenance connection, roles, databases, tenant operations.
   lifecycle = ["serving", "dep:getrandom"]
   # The admin token verifier, authenticator and guard.
   admin = ["serving", "dep:sekvent-auth", "dep:serde", "dep:serde_json"]
   # The token minter and the admin client; no database.
   manager = [
       "dep:sekvent-auth", "dep:sekvent-client", "dep:sekvent-context",
       "dep:serde", "dep:serde_json", "dep:prost", "dep:bytes",
       "dep:base64", "dep:tokio",
   ]
   # Runtime units for the tenant cache and the listener.
   runtime = ["serving", "dep:sekvent-runtime", "sekvent-db/runtime"]
   ```

3. **`crates/sekvent/Cargo.toml`.** The dependency in path form with
   default features off (the reason `sekvent-runtime` has that form: a
   member cannot turn off the defaults of a root pin, and
   `tenancy-manager` must not pull sqlx):
   `sekvent-tenancy = { path = "../sekvent-tenancy", default-features = false, optional = true }`.
   After the `storage` feature:

   ```toml
   # sekvent-tenancy: tenants on a database each (selection, cache, invalidation).
   tenancy = ["dep:sekvent-tenancy", "sekvent-tenancy/serving", "config", "error", "context", "db", "component"]
   # sekvent-tenancy: the maintenance connection and the lifecycle operations.
   tenancy-lifecycle = ["tenancy", "sekvent-tenancy/lifecycle"]
   # sekvent-tenancy: the admin token verifier, authenticator and guard.
   tenancy-admin = ["tenancy", "auth", "sekvent-tenancy/admin"]
   # sekvent-tenancy: the token minter and the admin client (no database).
   tenancy-manager = ["dep:sekvent-tenancy", "sekvent-tenancy/manager", "config", "error", "context", "auth", "client"]
   ```

   `"sekvent-tenancy?/runtime"` joins the `runtime` feature's list; the
   four names join `full`.
4. **`crates/sekvent/src/lib.rs`.** A row in the module table after
   `storage`; bullets for the three sub-features; and

   ```rust
   /// Tenants on a database each: selection, the tenant cache, lifecycle.
   #[cfg(any(feature = "tenancy", feature = "tenancy-manager"))]
   pub use sekvent_tenancy as tenancy;
   ```

5. **`Cargo.lock`** on the Mac: `cargo metadata --format-version 1 >/dev/null`.
   One package is added (`sekvent-tenancy`); no third-party package is
   added or moved. If one is, stop: decision 2 is broken.
6. **The crate skeleton.** `src/lib.rs` (crate docs with the feature table,
   the `mod` and `pub use` lines of slice 1a), `src/cluster/mod.rs`,
   `src/error.rs` whole (section 7), and `tests/common/mod.rs` whole
   (section 11.1).
7. **Index pages**, each row once:

   | File | Change |
   |---|---|
   | `AGENTS.md` | the crate-map row of 4.2; in "Components", `docs/design/tenancy.md` in the list of specs |
   | `README.md` | crate-table row after `sekvent-storage`: `` `sekvent-tenancy` `` / `` `tenancy`, `tenancy-lifecycle`, `tenancy-admin`, `tenancy-manager` `` / the text of the crate-map row |
   | `docs/README.md` | modules table, after the `sekvent::storage` row: `` `sekvent::tenancy` `` / `` `tenancy` `` / `[tenancy](modules/tenancy.md)` / the text of the crate-map row; "Design notes and examples", after the `design/storage.md` row: this note; a row for `examples/tenants` when it lands (1c) |
   | `docs/features.md` | "Module features": `tenancy`; "Integration features": `tenancy-lifecycle`, `tenancy-admin`, `tenancy-manager`; "Features that combine": `tenancy` with `runtime` (the units) |
   | `skills/sekvent/SKILL.md` | crate-map rows: tenants on a database each; the admin guard; the manager kit. Recipes are added by the slice that makes them true |
   | `skills/sekvent-api/SKILL.md` | section 2: rows pointing at `docs/modules/tenancy.md` by heading; section 5: the four features |
   | `docs/modules/tenancy.md` | created with the headings of 12.1 and "not built yet" under each heading no slice has filled |

   Rows for behaviour that does not exist yet say which slice brings it;
   they are rewritten in place when it lands, never left as history.

### 6.2 Slices 1b and 1c

Step 0 of each: the slice's module declarations and re-exports in
`lib.rs`, the pinned public signatures (everything this note marks
"pinned in step 0"), additions to `tests/common/` (11.1), the
`sekvent-facade-check` module `src/tenancy.rs`, and in 1c the three example
members in the root manifest. A slice that changes a signature of an
earlier slice records it in this note in place.

The facade check of 1b, through `sekvent::…` paths only: a `ScopeFactory`
for a scope that holds the tenant's `sqlx::PgPool`; the keyed builder with
its defaults; a `TenantResolver` returned as `impl ScopeResolver`; and
unit tests on a fixed `Tenants` whose scope type is not `Clone`, proving
the lease is `Clone + Deref` by itself: a unary path gets a lease and the
tenant text of `TenantId::LOCAL`, a listed streaming path gets
`StreamTenant` and no lease, a path under a bypass prefix of
`fixed_with_bypass` gets an empty scope. The crate takes its facade
features `config`, `component`, `component-grpc` and `tenancy`, and
depends directly on `http` and on `sqlx` (feature `postgres`): a scope
factory names `sqlx::PgPool` and a test builds a request head, as a
product does, and the facade re-exports neither crate. 1c adds
`TenantHooks` and a guarded method.

## 7. Errors

`error.rs`, written whole in step 0 of 1a.

```rust
/// The domains of this crate's reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorDomains {
    /// Reasons a request to a tenant can see.
    pub serving: &'static str,
    /// Reasons an admin caller can see.
    pub admin: &'static str,
}
impl ErrorDomains {
    pub const DEFAULT_SERVING: &'static str = "sekvent.tenancy.v1";
    pub const DEFAULT_ADMIN: &'static str = "sekvent.tenancy.admin.v1";
    pub const fn new() -> Self;                      // the defaults; also `Default`
    #[must_use] pub const fn with_serving(self, domain: &'static str) -> Self;
    #[must_use] pub const fn with_admin(self, domain: &'static str) -> Self;
}
pub mod reasons { /* one `pub const NAME: &str = "NAME";` per row below */ }
pub mod messages {
    pub const LIFECYCLE_BUSY: &str = "the server is busy, try again shortly";
    pub const TENANT_OPEN_FAILED: &str = "the tenant's database could not be opened";
}
```

Both are `&'static str` because `EndUserRejection` takes nothing else.

| Reason | Code | Domain | Message | When |
|---|---|---|---|---|
| `TENANT_NOT_HERE` | `NOT_FOUND` | serving | `this tenant is not served here` | unknown, purging or purged tenant; no usable request host, a host that selects nothing, the refusals of the path-label prefix (9.4) |
| `TENANT_PROVISIONING` | `UNAVAILABLE` | serving | `this tenant is being set up` | state `provisioning` |
| `TENANT_SUSPENDED` | `FAILED_PRECONDITION` | serving | `this tenant is suspended` | state `suspended` |
| `TENANT_UNAVAILABLE` | `UNAVAILABLE` | serving | `this tenant is unavailable, try again later` | the tenant's database cannot be opened |
| `TENANT_BUSY` | `UNAVAILABLE` | serving | `the server is busy, try again shortly` | no lookup permit, a caller that joined two reads of its key without an answer, or the tenant cache is full; retry hint 1 s |
| `TENANT_SELECTOR_MISSING` | `INVALID_ARGUMENT` | serving | `the request names no tenant` | an id-header host without the id header |
| `TENANT_MISMATCH` | `INVALID_ARGUMENT` | serving | `the request names two different tenants` | two selectors disagree, or a malformed id |
| `ORIGIN_NOT_ALLOWED` | `PERMISSION_DENIED` | serving | `this origin may not call this tenant` | an `Origin` that is not a host of the selected tenant |
| `ADMIN_TOKEN_INVALID` | `UNAUTHENTICATED` | admin | the component layer's constant rejection message | every failure of an admin token, whatever the cause |
| `ADMIN_TOKEN_REPLAYED` | `ALREADY_EXISTS` | admin | `the admin token was used before` | a second call with one token id |
| `TENANT_NOT_FOUND` | `NOT_FOUND` | admin | `no such tenant` | a lifecycle call for an id without a control row |
| `TENANT_STATE` | `FAILED_PRECONDITION` | admin | `the tenant's state does not allow this` | a lifecycle call the state refuses; a host, role or database name already taken |
| `TENANT_LIMIT_REACHED` | `RESOURCE_EXHAUSTED` | admin | `the tenant limit is reached` | provisioning a new id at the product's cap |
| `ADOPTION_REFUSED` | `FAILED_PRECONDITION` | admin | `the database cannot be adopted` | a handover check failed; metadata `check` names it (8.11) |
| `CONTROL_SCHEMA_MISSING` | `FAILED_PRECONDITION` | none | `the control schema is missing or incomplete` | `control::verify_schema` at startup; metadata `table` or `constraint` |

- Reasons are wire contract; messages are not. Message text is fixed in
  this milestone (15, question 6). The two lifecycle answers without a
  reason (`UNAVAILABLE` when `Registry::admit` finds no permit in time, and
  when a tenant cannot be opened for a lifecycle operation) have their
  texts in `messages`, stable, so a caller can tell them apart (14,
  decision 47).
- Cluster operations and the control rows (`TENANT_STATE`,
  `CONTROL_SCHEMA_MISSING`) return reasons without a domain (they know no
  product); the guard gives a reasoned error without a domain the admin
  domain (10.6). Serving errors get the serving domain from `Tenants`,
  which holds the `ErrorDomains` and through which the resolver selects;
  a refusal of the product's `ScopeFactory` keeps its own domain (9.6).
- Drift, a refused name and a misconfiguration are `INTERNAL` without a
  reason: the cause goes to the source, which is logged and never sent.
- Database errors map through `sekvent_db::to_app_error`. No error of this
  crate carries a URL, connect options, a role password or a verifier.

## 8. Slice 1a: leaves

First, because the coupling is smallest and the invariants are hardest.
Nothing here reads a tenant row.

### 8.1 File ownership

| Writer | Creates | Module-page section |
|---|---|---|
| A | `src/id.rs`, `src/names.rs`, `src/host.rs`, `src/label.rs` | "Names, hosts and labels" |
| B | `src/secrets/mod.rs`, `src/secrets/kdf.rs`, `src/secrets/scram.rs`, `src/connector.rs`, `tests/secrets.rs` | "Role credentials" |
| C | `src/cluster/maintenance.rs`, `src/cluster/sql.rs`, `src/cluster/roles.rs`, `src/cluster/databases.rs`, `tests/cluster_roles.rs`, `tests/cluster_databases.rs` | "The maintenance connection", "Roles and databases" |
| D | `src/cluster/privileges.rs`, `src/cluster/preflight.rs`, `tests/privileges.rs`, `tests/preflight.rs` | "The startup preflight" |
| E | `src/cluster/handover.rs`, `tests/handover.rs` | "Adopting a database" |

The module page `docs/modules/tenancy.md` is one file: each writer returns
its section as text in its report and the coordinator places it. Unit
tests sit in the file they test. `src/cluster/sql.rs` is C's and is read
by E through the crate-private items pinned in 8.6.

### 8.2 Ids and names (`id.rs`, `names.rs`)

```rust
/// A tenant's id. The nil UUID is the one tenant of a process that serves
/// a fixed scope.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(Uuid);

impl TenantId {
    pub const LOCAL: Self;                              // nil
    pub const fn from_uuid(id: Uuid) -> Self;
    pub const fn as_uuid(&self) -> &Uuid;
    pub const fn is_local(&self) -> bool;
    /// The lower-case hyphenated form only (36 characters).
    pub fn parse(text: &str) -> Option<Self>;
}
impl From<Uuid> for TenantId;  impl From<TenantId> for Uuid;
impl fmt::Display for TenantId;  impl fmt::Debug for TenantId;   // both the lower-case hyphenated form
```

`Display` is the text a selector header, a token claim, a NOTIFY payload
and the request scope's tenant carry; it passes
`sekvent_context::headers::is_valid_tenant`. `parse` refuses every other
spelling of a UUID (upper case, braces, no hyphens), so one tenant has one
text.

```rust
pub const KEY_ID_MAX: usize = 32;
/// `^[A-Za-z0-9_-]{1,32}$`: a role-secret key id, an admin key id.
pub fn is_key_id(text: &str) -> bool;

/// The product's object-name prefix and the names made from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectNames { /* private */ }

impl ObjectNames {
    /// `^[a-z][a-z0-9_]{0,29}_$`, not starting with `pg_`. `INVALID_ARGUMENT`.
    pub fn new(prefix: &str) -> Result<Self, AppError>;
    pub fn prefix(&self) -> &str;
    /// `<prefix><32 lower-case hex digits of the id>`.
    pub fn role(&self, tenant: TenantId) -> TenantRole;
    /// The same text as the role: a tenant's database is named like its role.
    pub fn database(&self, tenant: TenantId) -> DatabaseName;
    /// Exactly a name `role` can return.
    pub fn is_object_name(&self, name: &str) -> bool;
    /// What the preflight treats as tenant-named: starts with the prefix.
    pub fn has_prefix(&self, name: &str) -> bool;
    /// A stored role name, checked. `INTERNAL` for any other text.
    pub fn parse_role(&self, name: &str) -> Result<TenantRole, AppError>;
    pub fn tenant_of(&self, name: &str) -> Option<TenantId>;
}

/// A tenant role's name. Only `ObjectNames` makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantRole { /* private */ }
impl TenantRole { pub fn as_str(&self) -> &str; }

/// A database name this crate may format into a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseName { /* private */ }
impl DatabaseName {
    /// `^[a-z_][a-z0-9_]{0,62}$` and none of `postgres`, `template0`,
    /// `template1`. `INTERNAL` for any other text.
    pub fn parse(name: &str) -> Result<Self, AppError>;
    pub fn as_str(&self) -> &str;
}
/// The grammar of `DatabaseName::parse` without the three system names.
pub fn is_adoptable_name(name: &str) -> bool;
```

- A prefix of 2 to 31 characters keeps every name within Postgres' 63
  bytes. An example: `app_t_` gives `app_t_0b9c…` (38 characters).
- **The type is the check.** Utility statements (`CREATE ROLE`,
  `CREATE DATABASE`, `GRANT`, `SET ROLE`, `DROP …`) take no bind
  parameters, so every name is formatted into SQL. The only way to a
  quoted identifier is the crate-private `quoted()` of `TenantRole` and
  `DatabaseName` (8.6), and neither grammar contains a quote, a space or
  an upper-case letter. No function of the crate formats a `&str` into a
  statement.
- A stored name that fails its check means the control row was changed by
  hand: `INTERNAL`, before any statement.

### 8.3 Hosts and labels (`host.rs`, `label.rs`)

```rust
pub const HOST_INPUT_MAX: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostError { Empty, TooLong, NotAscii, Malformed }

/// The one spelling of a host this crate stores and compares.
pub fn canonical_host(input: &str) -> Result<String, HostError>;
/// `localhost`, `*.localhost`, `127.0.0.0/8`, `[::1]`, with or without a port.
pub fn is_loopback_host(canonical: &str) -> bool;
/// The canonical host of an `Origin` value this crate accepts.
pub fn host_of_origin(origin: &str) -> Option<String>;
/// `https://<host>`; `http://<host>` on loopback.
pub fn origin_of_host(canonical: &str) -> String;
```

| Rule of `canonical_host` | |
|---|---|
| Input | at most 255 bytes, ASCII only |
| Case | lowered |
| Trailing dot | one is removed |
| Name | at most 253 bytes; labels of 1 to 63 letters, digits and interior hyphens |
| Numeric last label | the whole host must be a dotted IPv4 address |
| IPv6 | bracketed, written in its canonical form |
| Port | decimal 1 to 65535 without leading zeros, validated everywhere; **kept only on a loopback host** and dropped elsewhere |

`host_of_origin` accepts `https://<host>` with no port or the literal
`:443`, and on loopback `http://` or `https://` with any port (kept). The
scheme is lower case; a path, query, fragment or user info, the value
`null` and anything that is not text give `None`. Origins are compared as
canonical hosts, never with `ends_with` or `contains`.

```rust
/// How a product's tenant labels look.
#[derive(Clone)]
pub struct LabelRules { /* private */ }
impl LabelRules {
    /// A DNS label: 1 to 63 characters, nothing reserved.
    pub fn new() -> Self;
    /// `min <= max <= 63`, `min >= 1`. `INVALID_ARGUMENT`.
    pub fn with_length(self, min: usize, max: usize) -> Result<Self, AppError>;
    /// Labels the product keeps for itself; called with the lowered label.
    #[must_use] pub fn with_reserved(self, reserved: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self;
    pub fn canonical(&self, input: &str) -> Result<String, LabelError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LabelError { Length, Characters, Hyphen, Punycode, Reserved }

/// `<label>.<base>`; no validation.
pub fn label_host(label: &str, base: &str) -> String;
/// The host a path label stands for: the listener host with its first
/// label replaced by `segment`. `None` on loopback, for a malformed
/// segment or a listener host of one label.
pub fn path_label_host(segment: &str, listener_host: &str) -> Option<String>;
```

A label is ASCII letters, digits and hyphens, lowered; no hyphen at either
end (`Hyphen`); no `--` at positions 3 and 4 (`Punycode`). The reserved
list is the product's argument and no list ships with the crate.

### 8.4 Role secrets (`secrets/`)

```rust
pub const MASTER_SECRET_MIN_BYTES: usize = 32;

/// Which master secret and which generation a role's password comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleKey { pub key_id: String, pub generation: u32 }

/// The master secrets by key id, and the product's context string.
/// Clones share the keys. `Debug` prints the number of keys.
#[derive(Clone)]
pub struct RoleSecrets { /* private */ }

impl RoleSecrets {
    /// `text` is `key_id:secret[,key_id:secret…]`; `context` is
    /// `^[A-Za-z0-9._-]{1,64}$`. `INVALID_ARGUMENT`.
    pub fn parse(text: &Secret, context: &str) -> Result<Self, AppError>;
    pub fn contains(&self, key_id: &str) -> bool;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    /// The role password of `tenant` under `key`. `INTERNAL` for a key id
    /// that is not configured or generation 0.
    pub fn derive(&self, tenant: TenantId, key: &RoleKey) -> Result<Secret, AppError>;
}
```

**Parsing.** Entries are split at `,` and trimmed, each at its first `:`.
A key id passes `is_key_id` and occurs once. A secret is at least 32
bytes, does not contain `change-me` in any letter case, and is not one
character repeated. A refusal names the entry's position (`entry 2 …`),
never a key id or a value, and no configuration key: the product prefixes
its own key name.

**Derivation.** Fixed, because a deployed tenant's database opens only
with exactly these bytes:

| Input | Value |
|---|---|
| Function | HKDF-SHA-256 (RFC 5869), extract then expand |
| Input key material | the bytes of the secret of `key.key_id` |
| Salt | the 16 bytes of the tenant's UUID |
| Info | `<context>:<key_id>:<generation>` in ASCII, generation in decimal |
| Output | 32 bytes, encoded as base64url without padding (43 characters) |

The password is ASCII, so SASLprep is the identity. **No credential is
stored**: the password exists only in a `Secret` between `derive` and the
connect call or the verifier.

```rust
// secrets/scram.rs  (feature `lifecycle`)
pub const SCRAM_ITERATIONS: u32 = 4096;

/// A SCRAM-SHA-256 verifier as Postgres stores it. `Debug` prints nothing.
#[derive(Clone)]
pub struct ScramVerifier { /* a Secret */ }

impl ScramVerifier {
    /// With a random 16-byte salt. `INTERNAL` when the random source fails.
    pub fn new(password: &Secret) -> Result<Self, AppError>;
    /// With `salt`; pure.
    pub fn with_salt(password: &Secret, salt: &[u8; 16]) -> Self;
    /// A verifier made elsewhere, after the syntax check. `INTERNAL`.
    pub fn parse(text: Secret) -> Result<Self, AppError>;
    /// The verifier text, as `Secret::expose`.
    pub fn expose(&self) -> &str;
}
/// `SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`, each part standard
/// base64 (`[A-Za-z0-9+/]+={0,2}`), nothing before or after.
pub fn is_scram_verifier(text: &str) -> bool;
```

- `SaltedPassword` is PBKDF2-HMAC-SHA-256 of one block (RFC 8018), 4096
  iterations; `ClientKey`, `StoredKey` and `ServerKey` as RFC 5802.
  Intermediate keys are wiped on drop (`zeroize`).
- A role is created and altered with the verifier only, so the password
  never appears in a statement. The verifier's alphabet holds no quote;
  `ScramVerifier` is the only type the crate formats into a string
  literal.
- 4096 iterations block for a moment: the caller runs `ScramVerifier::new`
  on the blocking pool (the role step of 10.2 does).

### 8.5 The connector (`connector.rs`)

```rust
/// Connect options for tenant pools. `Debug` prints nothing.
#[derive(Clone)]
pub struct Connector { /* the base options, the secrets */ }

impl Connector {
    /// `base` gives the server: host, port or socket, TLS settings,
    /// application name. Its user, password and database are never used.
    pub fn new(base: PgConnectOptions, secrets: RoleSecrets) -> Self;
    pub fn secrets(&self) -> &RoleSecrets;
    /// The options of one tenant under one key.
    pub fn options(
        &self,
        tenant: TenantId,
        role: &TenantRole,
        database: &DatabaseName,
        key: &RoleKey,
    ) -> Result<PgConnectOptions, AppError>;
    /// One candidate per key, in order. `INTERNAL` for no key.
    pub fn candidates(
        &self,
        tenant: TenantId,
        role: &TenantRole,
        database: &DatabaseName,
        keys: &[RoleKey],
    ) -> Result<ConnectCandidates, AppError>;
}
```

- Options are built field by field on a clone of the base: user, password
  and database are **always** set from the arguments, so the base's own
  credentials never reach a tenant connection. Never a formatted URL.
- The product parses its base URL (a `Secret`) into `PgConnectOptions`
  itself and sets every field it relies on:
  `PgConnectOptions::new()` fills gaps from `PG*` variables.
- Today every caller passes one key. Several keys are the rotation seam:
  `KeyedPools` tries the next candidate only after SQLSTATE `28P01`.

### 8.6 The maintenance connection (`cluster/maintenance.rs`, `cluster/sql.rs`)

The maintenance role is **not a superuser**: it has `LOGIN`, `CREATEDB`
and `CREATEROLE`, and it holds every tenant role
`WITH INHERIT FALSE, SET TRUE`. It can switch to a tenant role and has
none of a tenant role's privileges until it does. Postgres 16 or later is
required (the membership options, and `CREATEROLE` giving `ADMIN OPTION`
on the roles it creates).

```rust
/// The cluster's maintenance connection: one at a time. Clones share the
/// permit. `Debug` prints nothing.
#[derive(Clone)]
pub struct Maintenance { /* options, names, a semaphore of one, the application name */ }

impl Maintenance {
    pub fn new(options: PgConnectOptions, names: ObjectNames) -> Self;
    /// The `application_name` of every maintenance session and pool, clamped
    /// to printable ASCII (others become `?`) and 63 bytes; never fails.
    #[must_use]
    pub fn with_application_name(self, name: &str) -> Self;
    pub fn application_name(&self) -> &str;
    /// The maintenance role (the options' user).
    pub fn role(&self) -> &str;
    /// The maintenance database (the options' database, else the role's name).
    pub fn database(&self) -> &str;
    pub fn names(&self) -> &ObjectNames;
    pub async fn connect(&self) -> Result<MaintenanceConn, AppError>;
    /// Connected to `database` instead (the handover only).
    pub async fn connect_to(&self, database: &DatabaseName) -> Result<MaintenanceConn, AppError>;
    /// A pool of one connection on `database` that holds the permit, for a
    /// product's read-only check before a handover (10.4).
    pub async fn pool_on(&self, database: &DatabaseName) -> Result<MaintenancePool, AppError>;
}

pub struct MaintenanceConn { /* private; `Debug` prints nothing */ }
impl MaintenanceConn {
    pub fn is_poisoned(&self) -> bool;
    /// Closes the session and returns the permit, poisoned or not.
    pub async fn close(self) -> Result<(), AppError>;
}

pub struct MaintenancePool { /* private; `Debug` prints nothing */ }
impl MaintenancePool {
    pub fn pool(&self) -> &PgPool;
    pub async fn close(self);
}
```

| Bound | Value | Past it |
|---|---|---|
| wait for the permit | 30 s | `UNAVAILABLE`, `the maintenance connection is busy` |
| connect, authenticate, session setup | 10 s | `UNAVAILABLE`, `the maintenance database did not answer in time` |
| server: `lock_timeout` | 10 s | the statement's database error |
| server: `statement_timeout` | 120 s | the statement's database error |
| one public operation of `roles`, `databases`, `handover`, client side | 150 s | `UNAVAILABLE`; the connection is dropped and poisoned |
| close | 10 s | the connection is dropped; the permit returns all the same |

Rules of every maintenance session:

- `search_path = pg_catalog, pg_temp`. During a handover the session is
  connected to a database that is not the product's, and no object of that
  database may stand in for a catalog operator or function.
- Statement logging is off on the connection: role statements carry
  verifiers.
- Application name `sekvent-tenancy-maintenance`, unless the product sets
  another with `with_application_name`.
- Catalog reads are static SQL with bound names. Utility statements are
  built only in `cluster/sql.rs`, only from `TenantRole`, `DatabaseName`
  and `ScramVerifier`.
- A `pool_on` session keeps the lock and statement bounds but runs with
  the database's own `search_path`, which a product's statements need.
  The pool is therefore used only after `handover::prepare` found no
  object of a third role in that database.
- **One role.** `connect` and `connect_to` read `session_user` and
  `CURRENT_USER` once per session, within the connect bound and before
  any operation, and close and refuse a session where they differ with
  `FAILED_PRECONDITION` naming the database and both roles. The startup
  preflight is not enough: it reads the maintenance database once at
  startup, while a handover's sessions open on the candidate database
  (whose own `role` setting, or the maintenance role's setting in it, the
  preflight never sees) and any session may open after a setting changed.
  A `pool_on` session is not checked itself; `prepare`'s session on the
  same database, with the same login and options, was.
- **Poisoning.** A session-level `SET ROLE` marks the connection poisoned
  until its `RESET ROLE` succeeded; an operation that ran out of time
  poisons it for good. Every function of `roles`, `databases` and
  `handover` refuses a poisoned connection with `INTERNAL` before any
  statement; it can only be closed. Wherever a transaction will do,
  `SET LOCAL ROLE` is used instead, which resets on any outcome.

Crate-private, pinned for writers C and E (`cluster/sql.rs`,
`cluster/maintenance.rs`):

```rust
impl TenantRole   { pub(crate) fn quoted(&self) -> String; }
impl DatabaseName { pub(crate) fn quoted(&self) -> String; }
impl ScramVerifier { pub(crate) fn literal(&self) -> Secret; }          // 'SCRAM-SHA-256$…'
impl MaintenanceConn {
    pub(crate) fn live(&mut self) -> Result<&mut PgConnection, AppError>;   // refuses poisoned
    pub(crate) fn bounded<T>(&mut self, outcome: Result<Result<T, AppError>, Elapsed>) -> Result<T, AppError>;
}
pub(crate) const OPERATION_TIMEOUT: Duration;                              // 150 s
/// As `owner`, in one transaction: CONNECT revoked from PUBLIC and granted
/// to the owner, then connections allowed.
pub(crate) async fn restrict_connect(conn: &mut PgConnection, database: &DatabaseName, owner: &TenantRole) -> Result<(), AppError>;
```

### 8.7 Roles (`cluster/roles.rs`)

```rust
pub async fn exists(conn: &mut MaintenanceConn, role: &TenantRole) -> Result<bool, AppError>;
pub async fn create(
    conn: &mut MaintenanceConn,
    role: &TenantRole,
    verifier: &ScramVerifier,
    connection_limit: u32,
) -> Result<(), AppError>;
pub async fn drop(conn: &mut MaintenanceConn, role: &TenantRole) -> Result<(), AppError>;
```

`create`, in one transaction:

1. Read whether the role exists.
2. If it does, `privileges::check_tenant_role` (8.9) must find nothing;
   any finding is `INTERNAL` and nothing changes.
3. `CREATE ROLE`, or `ALTER ROLE` for an existing one, with
   `LOGIN NOINHERIT NOCREATEDB NOCREATEROLE CONNECTION LIMIT <n> PASSWORD '<verifier>'`.
   Only the statement that runs is formatted, after the read. A repeated
   call converges on the current verifier and limit.
4. `GRANT <role> TO CURRENT_USER WITH INHERIT FALSE, SET TRUE`.

`connection_limit` is 1 to `i32::MAX` (`INTERNAL` otherwise: 0 locks the
role out, -1 is unlimited). `drop` is `DROP ROLE IF EXISTS`, after the
role's database is gone (a role that owns one cannot be dropped).

**Existing objects are checked, never repaired.** The maintenance role
cannot alter every attribute (`SUPERUSER`, `REPLICATION`, `BYPASSRLS`) nor
revoke another grantor's membership, so a partial repair would report
success over a role that is still too wide. Drift is an operator's to
resolve.

### 8.8 Databases (`cluster/databases.rs`)

```rust
pub async fn create(conn: &mut MaintenanceConn, database: &DatabaseName, owner: &TenantRole) -> Result<(), AppError>;
pub async fn drop(conn: &mut MaintenanceConn, database: &DatabaseName, owner: &TenantRole) -> Result<(), AppError>;
pub async fn size(conn: &mut MaintenanceConn, database: &DatabaseName) -> Result<u64, AppError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseAccess { pub owner: String, pub public_connect: bool }
/// The owner and `PUBLIC`'s `CONNECT` of any database, in one bound
/// statement; `None` when there is none. No name check.
pub async fn access(conn: &mut MaintenanceConn, database: &str) -> Result<Option<DatabaseAccess>, AppError>;
```

**`create`** (`database` must be a tenant object name of the connection's
`ObjectNames`, else `INTERNAL`):

1. `owner` passes `privileges::check_tenant_role`.
2. No such database: `CREATE DATABASE … OWNER <owner> ALLOW_CONNECTIONS false`
   (one statement, outside a transaction). It is created closed because
   `PUBLIC` holds `CONNECT` by default.
3. It exists and `owner` owns it: `privileges::check_tenant_database`
   must find nothing. It exists with another owner: `INTERNAL`.
4. Always, as the owner in one transaction (`SET LOCAL ROLE`): `CONNECT`
   revoked from `PUBLIC` and granted to the owner, then
   `ALLOW_CONNECTIONS true`. The database opens only together with its
   restriction, and a call resumed after a failure at any point converges.

Afterwards the ACL lets no role but the owner connect.

**`drop`** destroys data, so it decides from one catalog read (the
database's owner, the session's role, whether it is a template, whether it
is the session's own database, whether `owner` still exists):

| Found | Decision |
|---|---|
| no such database | done |
| owned by `owner`, not a template, not the session's database, `owner` is not the session's role | drop **as the owner** |
| owned by `owner` but a template, the session's own database, or `owner` is the session's role | `INTERNAL`, nothing dropped |
| owned by another role while `owner` exists | `INTERNAL`, nothing dropped |
| owned by another role and `owner` is gone | done: the tenant's database was dropped before its role, and the name is someone else's now |

The drop itself is `SET ROLE <owner>`, `DROP DATABASE IF EXISTS … WITH (FORCE)`,
`RESET ROLE`, each alone (a drop runs in no transaction), on a connection
poisoned from before the first until the last succeeded. **A database is
dropped only as the tenant's role and only when that role owns it**: never
as a discovered other owner and never as the maintenance role. SQLSTATE
`42501` on the `SET ROLE` or the drop is `INTERNAL` (the maintenance role
lacks a right on its own cluster); any other failure maps as a database
error and stays retryable.

**`size`** reads `pg_database_size` in a transaction, after
`SET LOCAL ROLE <owner>` when a tenant role owns the database (the
maintenance role holds no `CONNECT` on it). An owner that is neither the
session's role nor a tenant object name is `INTERNAL`.

### 8.9 Privilege inspection (`cluster/privileges.rs`)

The one place that reads role and database privileges; everything else
calls it. Every read is static SQL with bound names on the connection it
is given, changes nothing and needs only catalog access.

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoleAttributes {
    pub superuser: bool, pub inherit: bool, pub create_role: bool, pub create_db: bool,
    pub login: bool, pub replication: bool, pub bypass_rls: bool,
}
impl RoleAttributes {
    /// The SQL names of the attributes held that a tenant role never has.
    pub fn unexpected_for_tenant(&self) -> Vec<&'static str>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MembershipEdge {
    pub role: String, pub member: String,
    pub admin_option: bool, pub inherit_option: bool, pub set_option: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleFacts {
    pub name: String,
    pub attributes: RoleAttributes,
    pub edges: Vec<MembershipEdge>,   // every edge naming the role on either side
    pub reaching: Vec<String>,        // every role that reaches it through a chain, sorted
    pub reached: Vec<String>,         // every role it reaches through a chain, sorted
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum DatabasePrivilege { Connect, Temporary, Create }
impl DatabasePrivilege { pub fn sql_name(self) -> &'static str; }
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grantee { Public, Role(String) }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseGrant { pub grantee: Grantee, pub privilege: DatabasePrivilege, pub grantable: bool }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseFacts { pub name: String, pub owner: String, pub template: bool, pub grants: Vec<DatabaseGrant> }

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoleFinding { Attribute(&'static str), Reaches(String), ReachedBy(String), MaintenanceInherits }
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DatabaseFinding { Owner(String), Template, Grant(DatabaseGrant) }
impl fmt::Display for RoleFinding;      // `it holds SUPERUSER`, `role x reaches it`, …
impl fmt::Display for DatabaseFinding;  // `it is owned by x`, `PUBLIC holds CREATE`, …

pub async fn role_facts(conn: &mut PgConnection, role: &str) -> Result<Option<RoleFacts>, AppError>;
pub async fn database_facts(conn: &mut PgConnection, database: &str) -> Result<Option<DatabaseFacts>, AppError>;
pub async fn role_may_connect(conn: &mut PgConnection, role: &str, database: &str) -> Result<Option<bool>, AppError>;
pub async fn roles_that_may_connect(conn: &mut PgConnection, database: &str) -> Result<Option<Vec<String>>, AppError>;
pub async fn public_may_connect(conn: &mut PgConnection, database: &str) -> Result<Option<bool>, AppError>;

/// Pure: what is wrong with an existing tenant role.
pub fn tenant_role_findings(facts: &RoleFacts, maintenance: &str) -> Vec<RoleFinding>;
/// Pure: what is wrong with an existing tenant database.
pub fn tenant_database_findings(facts: &DatabaseFacts, owner: &str) -> Vec<DatabaseFinding>;
/// `INTERNAL` naming every finding in its source; the session's role is the maintenance role.
pub async fn check_tenant_role(conn: &mut PgConnection, role: &TenantRole) -> Result<(), AppError>;
pub async fn check_tenant_database(conn: &mut PgConnection, database: &DatabaseName, owner: &TenantRole) -> Result<(), AppError>;
```

Rules:

- **Any membership edge is a path**, whatever its options: `ADMIN` lets
  the member grant itself the role, `SET` lets it switch, `INHERIT` gives
  it the privileges. Chains are followed to their end (a recursive query,
  one statement).
- A database without an ACL is read as its defaults: the owner's
  privileges, and `PUBLIC`'s `CONNECT` and `TEMPORARY`.
- "May connect" is the effective privilege: an own grant, `PUBLIC`'s, an
  inherited membership's, or a superuser's. It says what the ACL allows,
  not whether a sign-in succeeds (`LOGIN`, `ALLOW_CONNECTIONS` and
  host-based rules decide that too).
- A tenant role is acceptable when it has none of `SUPERUSER`, `INHERIT`,
  `CREATEROLE`, `CREATEDB`, `REPLICATION`, `BYPASSRLS`; reaches no role;
  is reached by no role but the maintenance role (a member of the
  maintenance role counts as reaching it); and the maintenance role's edge
  is without `INHERIT`. `LOGIN` is not checked: `create` sets it.
- A tenant database is acceptable when its owner is the expected role, it
  is no template, and its ACL holds nothing beyond the owner's entries and
  `PUBLIC`'s `CONNECT` and `TEMPORARY` without grant option. A grant to
  the maintenance role is a finding: it never connects to a tenant
  database outside a handover.

### 8.10 The startup preflight (`cluster/preflight.rs`)

Run by the product before any migration. A *protected* database is one of
the service's own (the control database, the maintenance database, any
other the product names): it is reached only by its dedicated roles, so a
tenant role's credentials open none of them.

```rust
/// The longest a preflight of one pool may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protected {
    pub label: String,          // for the message: `control`, `maintenance`, …
    pub database: String,
    pub allowed: Vec<String>,   // roles that may connect besides the owner and superusers
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectingRole { pub name: String, pub login: bool, pub superuser: bool, pub reached: bool }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedFacts {
    pub owner: String,
    pub grants: Vec<DatabaseGrant>,
    pub connecting: Vec<(String, bool)>,   // every LOGIN role that may connect, and whether it is a superuser
    pub tenants_connecting: Vec<String>,   // every tenant role that may connect, sorted
}
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Finding {
    Missing,
    Public(DatabasePrivilege),
    TenantOwner(String),
    TenantMayConnect(String),
    RoleMayConnect(String),
    RoleSwitched { session_user: String, current_user: String },
}

pub fn protected_facts(owner: String, grants: Vec<DatabaseGrant>, roles: &[ConnectingRole],
                       allowed: &[String], maintenance: Option<&str>, names: &ObjectNames) -> ProtectedFacts;
pub fn findings(facts: &ProtectedFacts, allowed: &[String], names: &ObjectNames) -> Vec<Finding>;
/// The maintenance session's rule: `RoleSwitched` unless both are one role.
pub fn session_findings(session_user: &str, current_user: &str) -> Vec<Finding>;
pub async fn facts(conn: &mut PgConnection, database: &str, allowed: &[String],
                   maintenance: Option<&str>, names: &ObjectNames) -> Result<Option<ProtectedFacts>, AppError>;
pub async fn check(conn: &mut PgConnection, protected: &Protected,
                   maintenance: Option<&str>, names: &ObjectNames) -> Result<(), AppError>;
/// The pool's own database, reached by the pool's session role.
pub async fn check_session_database(pool: &PgPool, label: &str,
                   maintenance: Option<&str>, names: &ObjectNames) -> Result<(), AppError>;
/// The control database on its pool, then the maintenance database on a
/// connection of `maintenance` (it may be another server), closed after.
pub async fn check_control_and_maintenance(control: &PgPool, maintenance: &PgConnectOptions,
                   names: &ObjectNames) -> Result<(), AppError>;
```

A protected database passes when:

1. `PUBLIC` holds neither `CONNECT` nor `TEMPORARY` on it.
2. No tenant role owns it or may connect to it. A **tenant role** here is
   any role whose name has the object-name prefix, and, when the
   maintenance role is known, any role the maintenance role reaches
   through a membership chain other than the database's owner, its allowed
   roles and the maintenance role itself.
3. Every other `LOGIN` role that may connect is its owner, an allowed role
   or a superuser. A superuser is the cluster operator's and not a
   finding; a tenant-named superuser is one, by rule 2.

The maintenance session passes only when its `CURRENT_USER` is its
`session_user` (`RoleSwitched` otherwise). The crate names the maintenance
role by the session user (the reach of rule 2, `check_tenant_role`) while
the operations act as, and grant to, `CURRENT_USER`; the two differ only
when a role setting switches the session at startup. The rule is the
session's, not a database's: `check_control_and_maintenance` reads it once
with the maintenance connection's session (a column of the session read,
no statement of its own) and reports it first among the findings of the
database that connection's check covers, the maintenance database or the
shared one. Other pools are not held to it: their session role is only the
allowed role of their own database.

Further rules:

- **A fixed number of statements per database**, however many roles the
  cluster holds: one query returns every role that may connect together
  with the facts the rules need; the rules are pure. Never a statement per
  role.
- When the control and the maintenance database are the same database,
  one check runs and allows both roles.
- A server older than Postgres 16 is refused.
- A finding is `FAILED_PRECONDITION` naming the label, the database and
  the roles at fault; never a URL or a credential. Past `TIMEOUT`:
  `UNAVAILABLE`.

### 8.11 The ownership handover (`cluster/handover.rs`)

The cluster half of adopting an existing database as a tenant's: a
database the maintenance role owns becomes the tenant role's, with
everything in it.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Adoption {
    /// The maintenance role still owns the database.
    Reassign,
    /// The tenant role owns it already; only the CONNECT restriction is left.
    Restrict,
}
/// The checks before the database changes hands.
pub async fn prepare(maintenance: &Maintenance, database: &DatabaseName, role: &TenantRole) -> Result<Adoption, AppError>;
/// `prepare`, the handover, the CONNECT restriction.
pub async fn adopt(maintenance: &Maintenance, database: &DatabaseName, role: &TenantRole) -> Result<(), AppError>;
/// The metadata key of a refusal: `check`.
pub const CHECK_KEY: &str = "check";
```

Each step runs on a connection of its own, one at a time, closed after.

| Step | Connected to | What |
|---|---|---|
| 1 | the maintenance database | the database exists (`missing`), is no template (`template`), is owned by the maintenance role or already by `role` (`owner`), its ACL names no grantee beyond its owner and `PUBLIC` (`grantee`); on the reassign path the maintenance role owns no other database and no tablespace, which `REASSIGN OWNED` would move too (`owns_more`) |
| 2, reassign only | the database | no object owned by a role other than the maintenance role and `role` (`foreign_object`); then `CONNECT` revoked from `PUBLIC`; then no session of another role connected (`session`) |
| 3, reassign only | the database | one transaction that repeats steps 1 and 2's checks, then: the membership granted `WITH INHERIT TRUE`, the database's owner changed, `REASSIGN OWNED BY CURRENT_USER TO <role>`, the membership back to `INHERIT FALSE, SET TRUE`. An error rolls all of it back |
| 4 | the maintenance database | the CONNECT restriction of 8.8 step 4, as the new owner |

- A refusal is `FAILED_PRECONDITION` / `ADOPTION_REFUSED` with metadata
  `check` set to the name in brackets above.
- The membership is `INHERIT TRUE` only inside step 3's transaction:
  `REASSIGN OWNED` needs the privileges of the receiving role.
- The foreign-object check covers every catalog that records an owner, not
  tables alone, and skips what every database has by design (system
  objects, temporary schemas, members of an extension).
- A refusal changes nothing but `PUBLIC`'s `CONNECT`, which step 2 revokes
  before the session check so that no new session of a role without a
  grant of its own can start. A repeated call converges; after step 4 the
  maintenance role can no longer connect to the database.

### 8.12 Invariants and the tests that prove them

Unit tests, no server:

| Invariant | Test |
|---|---|
| One text per tenant id | `TenantId::parse` accepts the lower-case hyphenated form only; `Display` round-trips; the text passes `is_valid_tenant` |
| No unchecked text reaches a statement | grammar tables for `ObjectNames::new`, `is_object_name` (upper-case hex, wrong length, another prefix), `DatabaseName::parse` (quote, space, 64 characters, each system name), `is_key_id`; golden text of every statement builder of `cluster/sql.rs` for one sample name |
| One spelling per host | `canonical_host` table: case, trailing dot, label and name limits, a numeric last label, IPv6, ports kept on loopback only, a port with a leading zero; `host_of_origin` table: scheme, `:443`, loopback ports, path, user info, `null` |
| Label rules | length bounds, edge hyphen, `--` at positions 3 and 4, a reserved function called with the lowered label |
| HKDF is RFC 5869 | test cases A.1 to A.3 of the RFC |
| The derivation layout is fixed | one committed vector for a fixed secret, context, key id, tenant and generation; the output is 43 characters; it changes when any one input changes; generation 0 and an unknown key id are `INTERNAL` |
| SCRAM is RFC 5802 and 7677 | the RFC 7677 example exchange: the server signature computed from the derived `ServerKey` and the client proof from `ClientKey` match the RFC's; a fixed salt gives a stable verifier |
| The verifier check admits nothing else | accept and reject table: another head, another iteration count, three padding characters, an empty part, a quote, leading or trailing text |
| Nothing secret is printed | `Debug` of `RoleSecrets`, `ScramVerifier`, `Connector`, `Maintenance`, `MaintenanceConn`, `MaintenancePool`; parse refusals name a position and neither a key id nor a value |
| Base credentials are never reused | `Connector::options` from a base with a user, password and database of its own carries the arguments' three and the base's host, port and application name |
| Findings are complete | `tenant_role_findings` and `tenant_database_findings`: one row per finding and a clean case; `findings` and `protected_facts`: each rule of 8.10, the owner, an allowed role, a superuser, a tenant-named superuser |
| The drop decision | the five rows of 8.8 |
| A timed-out operation poisons | `bounded` with an elapsed outcome on a connection-less `MaintenanceConn` (a crate-private test constructor): `UNAVAILABLE`, then `live()` is `INTERNAL` |

Docker tests (`#[ignore]`, the skip of 11.1, real clock, a 30 s guard per
scenario):

| File | Proves |
|---|---|
| `tests/secrets.rs` | a role created with the verifier signs in with the derived password; the password of another generation is rejected (`28P01`); a `Connector` whose base carries a superuser's credentials yields a session whose `current_user` and `session_user` are the tenant role |
| `tests/cluster_roles.rs` | a created role has exactly the four attributes, the limit, and the maintenance membership without `INHERIT`; a repeated `create` moves the verifier and the limit; an existing role is refused, and unchanged, for each widening attribute, a membership out of it, a membership into it from a third role, a chain into it, and a maintenance membership with `INHERIT`; the connection limit holds; `drop`; a refused argument is answered before the poisoned check (so before any statement) |
| `tests/cluster_databases.rs` | a created database is owned by the role and opens only with its restriction: the tenant role connects, another `LOGIN` role and the maintenance role do not; a create interrupted after `CREATE DATABASE` converges on the next call; an existing database is refused, and unchanged, for another owner, a template, a grant to a third role, a grant to the maintenance role, `CREATE` to `PUBLIC`; `size` without a superuser; `drop` with sessions connected; `drop` refuses a database of another existing role, the maintenance role's, a template; a database of another owner once the role is gone is left alone and answered `Ok`; a failed drop leaves the session as the maintenance role; a poisoned connection refuses every operation; a session switched to another role by the role's or the database's setting is refused when it opens |
| `tests/privileges.rs` | chains of two and three edges in both directions; effective `CONNECT` through an own grant, `PUBLIC`, an inherited membership and a superuser; a database without an ACL read as its defaults |
| `tests/preflight.rs` | a protected database passes; each finding of 8.10 alone; the maintenance database is checked on its own connection; one shared database; three hundred tenant-named roles checked by one call; a maintenance session switched to another role by the role's or the database's setting is refused |
| `tests/handover.rs` | a handover moves the database and a table, a sequence, a function, a type and a schema; afterwards the membership is without `INHERIT` and the maintenance role cannot connect; each refusal of 8.11 with its `check`; a refusal leaves only `PUBLIC`'s `CONNECT` revoked; a repeated call converges; a database the role already owns takes the `Restrict` path |

### 8.13 Left to the product, and the consumer check

Left out of 1a on purpose: which key id new tenants get, the connection
limit, where a tenant's names are stored, when the preflight runs and
which further databases it protects, every key that feeds
`RoleSecrets::parse`, `ObjectNames::new` and the two sets of connect
options.

**Consumer check.** The consuming product replaces its own leaf code by
these modules and keeps its provisioning path and its admin surface.
Accepted when, on its own suite: a tenant provisioned before the swap
cold-opens after it (names and the derived password are byte-identical); a
fresh tenant provisions and signs in; its preflight, privilege, purge and
adoption tests pass unchanged.

## 9. Slice 1b: the serving half

Built. The sketches below name the public surface (the crate's source has
the full signatures and their docs); the behaviour is fixed here.

### 9.1 File ownership

| Writer | Files | Module-page section |
|---|---|---|
| A | `src/info.rs`, `src/control/schema.rs`, `src/control/rows.rs`, `src/control/registry.rs`, `tests/control.rs` | "The control schema" |
| B | `src/control/lookup.rs`, `src/control/admission.rs`, `tests/admission.rs` | "Lookups and their admission" |
| C | `src/select.rs`, `src/resolver.rs`, `src/hosts.rs`, `tests/resolve.rs` | "Select the tenant", "CORS" |
| D | `src/tenants/*.rs` except `mod.rs`, `tests/tenants.rs` | "The tenant cache" |
| E | `src/invalidation/*.rs` except `mod.rs`, `tests/invalidation.rs` | "Follow changes from other processes" |

### 9.2 Tenant info and the control schema

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TenantState { Provisioning, Active, Suspended, Purging, Purged }   // as_str / parse: lower case

/// What serving needs to know of a tenant. `Debug` prints id, subdomain, state.
#[derive(Clone, PartialEq, Eq)]
pub struct TenantInfo {
    pub id: TenantId,
    pub subdomain: String,
    pub display_name: String,
    pub state: TenantState,
    pub primary_host: String,      // empty only for a tenant without hosts
    pub hosts: Vec<String>,        // canonical, byte-sorted, no duplicates; holds `primary_host`
    pub role_key: RoleKey,
}
impl TenantInfo {
    /// The fixed scope's: `TenantId::LOCAL`, `Active`, empty texts, no hosts, key id empty, generation 0.
    pub fn local() -> Self;
    /// `origin_of_host(primary_host)`; `None` without a primary host.
    pub fn origin(&self) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Selector {
    Id(TenantId),
    Host(String),                                         // canonical
    PathLabel { segment: String, listener_host: String },
}
```

The reference schema, `control::REFERENCE_SCHEMA` (`src/control/reference_schema.sql`,
one DDL text with all three tables, which the product copies into a
migration of its own):

| Table | Columns | Named constraints and indexes |
|---|---|---|
| `tenants` | `id uuid`, `subdomain text`, `display_name text`, `state text`, `provision_step text default ''`, `db_cluster text default 'main'`, `db_name text`, `db_role text`, `db_role_key_id text`, `db_role_generation integer default 1`, `schema_version bigint null`, `created_at timestamptz`, `updated_at timestamptz` | `tenants_pkey (id)`; `tenants_db_role_key` unique `(db_cluster, db_role)`; the unique index `tenants_db_name_key (db_cluster, db_name)` where `state <> 'purged'`; `tenants_state_check` (the five values); `tenants_db_role_generation_check` (`>= 1`) |
| `tenant_hosts` | `host text`, `tenant_id uuid`, `is_primary boolean default false` | `tenant_hosts_pkey (host)`; `tenant_hosts_tenant_id_fkey` to `tenants` with `ON DELETE CASCADE`; the unique index `tenant_hosts_primary_key (tenant_id)` where `is_primary`; the index `tenant_hosts_tenant_id_idx (tenant_id)` |
| `admin_audit` | `token_id uuid`, `occurred_at timestamptz`, `operation text`, `actor text`, `outcome text default 'started'`, `tenant_id uuid null` (no foreign key: an audit row outlives its tenant's row) | `admin_audit_pkey (token_id)`: the replay refusal of 10.6 |

- `control::verify_schema(&PgPool)` selects every column the crate uses
  from each table with a false predicate (the method of
  `LeaseStore::verify_schema`) and reads from the catalog that
  `tenants_pkey`, `tenants_db_role_key`, `tenant_hosts_pkey`,
  `tenants_db_name_key` and `tenant_hosts_primary_key` are each a valid
  unique index on their table, since error mapping goes by constraint name
  and the reads rely on the uniqueness: partial exactly for the two partial
  keys of the reference schema (`tenants_db_name_key`,
  `tenant_hosts_primary_key`), total for the others. A missing, non-unique,
  invalid, wrongly partial or wrongly total index is a gap naming it.
  `admin_audit` and `admin_audit_pkey` (total) are checked only with the
  feature `admin`, which the facade's `full` feature turns on; a product
  without it may leave the table out. A gap is `FAILED_PRECONDITION` /
  `CONTROL_SCHEMA_MISSING` with metadata `table` or `constraint`; extra
  columns and indexes pass.
- **Extra columns are the product's.** Every statement of the crate names
  its columns; an extra column on `tenants` must be nullable or have a
  default. The product reads and writes its columns with statements of its
  own, each holding a control connection for that one statement.
- Statements are static SQL with bound values. Repository functions take
  `&mut PgConnection`, never commit, and the writes that set a timestamp
  (`insert`, `set_state`, `set_hosts`) take `now: SystemTime`.

The repository is the public module `control::rows`:

```rust
pub const MAIN_CLUSTER: &str = "main";

/// `tenants.provision_step`, in the order the steps are recorded (`Ord`).
#[non_exhaustive]
pub enum ProvisionStep { Inserted, Role, Database, Migrated, Seeded, Active }
// as_str / parse: the empty text, `role`, `database`, `migrated`, `seeded`, `active`

#[non_exhaustive]
pub struct TenantRow {
    pub info: TenantInfo,
    pub provision_step: ProvisionStep,
    pub db_cluster: String,      // as stored; serving opens only `MAIN_CLUSTER`
    pub db_name: String,         // as stored; checked with `DatabaseName::parse` before use
    pub db_role: String,         // as stored; must equal `ObjectNames::role(info.id)` before use
    pub schema_version: Option<i64>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}
/// A tenant to insert: `Provisioning`, step `Inserted`.
pub struct NewTenant { pub id, pub subdomain, pub display_name, pub hosts: Vec<String>,
    pub primary_host, pub database: DatabaseName, pub role: TenantRole, pub role_key: RoleKey }
#[non_exhaustive]
pub enum VersionStored { Stored, Unchanged, Busy }

pub async fn insert(conn, &NewTenant, now) -> Result<(TenantRow, bool), AppError>;
pub async fn get(conn, TenantId) -> Result<Option<TenantRow>, AppError>;
pub async fn lock(conn, TenantId) -> Result<Option<TenantRow>, AppError>;   // FOR UPDATE
pub async fn by_host(conn, &str) -> Result<Option<TenantRow>, AppError>;
pub async fn by_loopback_label(conn, &str) -> Result<Option<TenantRow>, AppError>;
pub async fn list_active(conn) -> Result<Vec<TenantInfo>, AppError>;
pub async fn set_state(conn, TenantId, TenantState, now) -> Result<bool, AppError>;
pub async fn set_hosts(conn, TenantId, subdomain, hosts, primary_host, now) -> Result<(), AppError>;
pub async fn store_schema_version(conn, TenantId, i64) -> Result<VersionStored, AppError>;
```

Stored rows are held to what the crate writes. A stored state outside the
five values, a step outside the six texts, a generation that is not
positive, or a missing primary host (accepted only for a row without hosts
that is `purging` or `purged`) is `INTERNAL`, naming the column and never
a value. `list_active` fails as a whole on one such active row, and with it
every reader of the active list: the walks, the sweep, `refresh_hosts` and
the listener's loads (question 11 of 15). Stored hosts are trusted to be canonical and returned
byte-sorted. The rules of the writes:

- `insert` validates the hosts before any statement (canonical, at least
  one, sorted and de-duplicated, the primary among them; `INVALID_ARGUMENT`
  with field violations, a refused host named by its position from 0,
  never its text) and the role key's generation (1 to 2147483647); a
  repeated id changes nothing and returns the existing row with `false`. A host, role or database name another tenant
  holds is `FAILED_PRECONDITION` / `TENANT_STATE` without a domain, mapped
  from `tenant_hosts_pkey`, `tenants_db_role_key` and `tenants_db_name_key`.
  A database name is free again once its row is `purged`; a role name
  never is.
- `lock` reads the hosts with a second statement after the lock is held, so
  a waiter sees the hosts the holder committed.
- `set_hosts` replaces the host set and the subdomain whole, with
  `insert`'s host checks and conflicts; a missing row is `NOT_FOUND`
  without a reason.
- `store_schema_version` takes the row `FOR UPDATE SKIP LOCKED` and never
  waits: it only raises the version (`Stored`, `Unchanged`), answers `Busy`
  for a locked or missing row, and never touches `updated_at`.
- No statement writes `db_cluster` or `provision_step`: the defaults give
  `main` and `Inserted`, and no 1b code compares a row's state with its
  step. `by_loopback_label` is the rule of 9.4.

The control pool has at least 3 connections; `Registry::new` refuses a
smaller one (the two admissions below each leave room for the other).

### 9.3 Lookups and their admission

```rust
/// Clones share one lookup cache with its reads in flight, one set of lookup permits and one
/// set of lifecycle permits; each value keeps its own label rules.
#[derive(Clone)]
pub struct Registry { /* private */ }
impl Registry {
    pub const MIN_CONNECTIONS: u32 = 3;
    pub fn new(control: PgPool, clock: Arc<dyn Clock>) -> Result<Self, AppError>; // no I/O
    /// The product's label rules for path labels (default `LabelRules::new()`), the same as
    /// `SelectorRules::labels`.
    pub fn labels(self, rules: LabelRules) -> Self;
    pub fn pool(&self) -> &PgPool;
    /// A request's lookup: the cache, then a read under the lookup admission. Any state.
    pub async fn lookup(&self, selector: &Selector) -> Result<Option<TenantRow>, AppError>;
    /// An internal read by id: the cache, no permit, never joined with a request's read.
    pub async fn tenant(&self, id: TenantId) -> Result<Option<TenantRow>, AppError>;
    /// The row as committed now: no cache, no permit, one statement.
    pub async fn tenant_now(&self, id: TenantId) -> Result<Option<TenantRow>, AppError>;
    pub async fn active(&self) -> Result<Vec<TenantInfo>, AppError>;
    pub async fn store_schema_version(&self, id: TenantId, version: i64) -> Result<VersionStored, AppError>;
    pub fn forget(&self, tenant: TenantId);
    pub fn clear_lookups(&self);
    /// The lifecycle admission of 10.5.
    pub async fn admit(&self) -> Result<LifecyclePermit, AppError>;
}
```

| | Value |
|---|---|
| Lookup cache | two key spaces (id, exact host); each with a partition of 4,096 found and one of 1,024 not-found entries; an entry is fresh while `start <= now < start + 30 s` on the registry's `Clock` (a clock set back never extends it); errors are never cached; no stale answer is ever served |
| Filling | a host lookup that finds a row also fills the id space; an id lookup does not fill the host space; a found entry replaces a not-found entry of the same key and the reverse. The key spaces are therefore not independent: a found host read fills the id space and `forget` drops from both |
| Replacing | an entry is replaced only by the answer of a read that started later than the read that made it; on a tie the stored entry stays, so a slower, older read (a host read's cross-fill included) never overwrites a newer observation |
| Freshness on sight | an entry found not fresh (expired, or dated after now because the clock was set back) is removed when it is found and never served later; an answer already past its lifetime when its read ends goes to its caller and is not stored |
| When a partition is full | expired entries are dropped, then only the partition that is still full is cleared, so not-found entries never push found entries out |
| `forget(tenant)` | drops the tenant's id entry, its found host entries and every not-found host entry; `clear_lookups` drops everything. The cache has one epoch and both raise it, so no read already running, of any tenant or key, stores its answer (its caller still gets it) |
| An invalid selector | a host that is not canonical, a segment the registry's `LabelRules` refuse (on every listener; with `Registry::labels` the product's reserved labels and lengths too), or a path label on a listener that is neither loopback nor one a host can be derived from: no row, without a permit and without a read |
| **Lookup admission** | `max(1, control max_connections / 2)` permits for selector cache misses |
| No permit | `TENANT_BUSY` at once, **before** any control connection is taken |
| Same key in flight | the caller joins that read, then looks again; a caller that joined twice without an answer is `TENANT_BUSY`. Joiners are woken when the read ends, however it ends, and never receive its error. A finished read gives its permit back first, then leaves the reads in flight, and wakes its joiners last, so a same-key caller in between joins instead of getting `TENANT_BUSY` |
| A caller that goes away | the read runs on a task of its own, keeps its permit and fills the cache; nothing is cancelled and no slot is freed early |
| A read task that panics | its caller gets `INTERNAL`; its permit and its place among the reads in flight are given back |
| Internal reads by id (`tenant`: lifecycle, the first read inside a tenant cache's creation) | share the cache, take no permit, never join or wait for a request's read; they run on the caller's own task. `Tenants::get`'s state check is a request's lookup, not an internal read (9.6) |
| The loopback path label | never cached, never joined, and each read spends a lookup permit (9.4) |
| A failed control read | returned as the database error; serving maps it to `TENANT_UNAVAILABLE` |
| A control connection | is held for a single statement and returned before a tenant is opened; none spans a tenant open |

This is the crate's bound inside the scope resolver. The App's
`ServeAdmission` is in front of it and unchanged: it bounds how many
unauthenticated calls are pending at all; the lookup admission bounds how
much of the control pool they can take, so lifecycle calls and jobs keep
theirs.

### 9.4 Selection and the resolver

```rust
/// Which request names which tenant. Built by the product from its own keys.
#[derive(Clone)]
pub struct SelectorRules { /* private */ }
impl SelectorRules {
    pub fn new() -> Self;                                   // selects nothing; also `Default`
    /// Requests to these hosts name the tenant by id in `header`.
    pub fn id_header(self, header: http::HeaderName, hosts: impl IntoIterator<Item = String>) -> Result<Self, AppError>;
    /// Requests to these hosts under `prefix` name the tenant by a path label.
    pub fn path_label(self, prefix: &str, hosts: impl IntoIterator<Item = String>) -> Result<Self, AppError>;
    /// Names so that `<prefix>.well-known/<name>/<label>` selects like `<prefix><label>`.
    pub fn well_known(self, names: impl IntoIterator<Item = String>) -> Result<Self, AppError>;
    /// The product's label rules (default `LabelRules::new()`).
    pub fn labels(self, rules: LabelRules) -> Self;
    /// Any other host is looked up as a tenant's own host.
    pub fn tenant_hosts(self, enabled: bool) -> Self;
    /// RPC path prefixes that belong to no tenant: an empty scope, no selection.
    pub fn bypass(self, prefixes: &'static [&'static str]) -> Self;
}
```

The builders refuse with `INVALID_ARGUMENT`, naming a position and never a
host: a host `canonical_host` refuses, a second `id_header` or
`path_label` call, a host in both lists that is not loopback, a prefix that
does not start and end with `/`, is not longer than `/` (the prefix is
reserved on every host, so `/` alone would reserve every path) or is not
visible ASCII without `%`, `?` and `#`, a well-known name outside 1 to 64
of `[A-Za-z0-9._-]`. A second
`well_known` or `bypass` call replaces the list.

The request host is the canonical form of the URI authority and of the one
`Host` header; when both are present they must agree. Forwarded-host
headers are never read. Absent, malformed, repeated, carrying user info or
disagreeing: `TENANT_NOT_HERE`.

The path-label prefix is a reserved namespace on every host: a path under
it is decided by row 2 and never falls through to another row.

| Order | Condition | Selector | Further checks |
|---|---|---|---|
| 1 | the path starts with a bypass prefix (decided before the host is read) | none; the scope is empty | |
| 2 | the path is under the path-label prefix | `PathLabel` when the host is a path-label host and the label is usable; otherwise `TENANT_NOT_HERE` (another host, a missing or malformed label, a label the product's `LabelRules` refuse, an unknown well-known name) | the id header is optional; when present it must name the tenant found (`TENANT_MISMATCH`, also for a malformed or repeated one) |
| 3 | the host is an id-header host | `Id` from the header: exactly one value, a `TenantId::parse` text (`TENANT_SELECTOR_MISSING` when absent, `TENANT_MISMATCH` when malformed or repeated) | an `Origin`, when present, must be exactly one value whose canonical host is a host of the selected tenant (`ORIGIN_NOT_ALLOWED`, also for a malformed or repeated one); a request without `Origin` passes |
| 4 | the host is a path-label host (the path is outside the prefix) | | `TENANT_NOT_HERE`: a path-label host serves nothing outside the prefix |
| 5 | `tenant_hosts` is on | `Host` | the id header as in row 2 |
| 6 | otherwise | | `TENANT_NOT_HERE` |

A bypass prefix is a plain string prefix of the request path as received
(it starts with `/`, it is never percent-decoded): `"/"` would bypass every
path, a prefix without a trailing `/` also matches longer service names,
and an empty prefix matches nothing, so a product ends its prefixes with
`/`. The one crate-private comparison serves both forms of `Tenants` (9.6),
and a bypass path gets an empty scope even when the host or the id header
is missing or malformed.

The path label is the first segment after the prefix (or after
`.well-known/<name>/`); what follows is ignored, a `%` in the segment
selects nothing, and a label the product's rules refuse causes no read. It
stands for the host `path_label_host` derives from it. On a loopback
listener, where `path_label_host` derives none, exactly one tenant that is
not purged may carry the label, else nothing is selected; that tenant must
have a registered loopback host (any port) stored in its canonical
spelling, so a stored host that `canonical_host` would change (such as
`UPPER.localhost:5173`) never makes a tenant selectable. That read is never
cached and never joined, and spends a lookup permit. The resolver checks
the segment with `SelectorRules::labels`; `Registry::lookup` checks it again
with the registry's own rules (`Registry::labels`), so a product that
looks path labels up itself refuses the same segments without a read.

A host may be in both lists only on loopback. The tenant's existence and
state are checked before a header or origin comparison, so a mismatch
tells a caller nothing about a tenant it cannot name. Only an id-header
selection checks the `Origin`; a path-label selection does not, also on a
loopback host listed for both selectors, for which the CORS predicate
(9.5) answers all the same.

```rust
/// What the scope holds for a streaming method instead of a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTenant(pub TenantId);

pub struct TenantResolver<S> { /* private */ }
impl<S: Send + Sync + 'static> TenantResolver<S> {
    /// `streaming`: RPC paths (`<package>.<Service>/<Rpc>`, with or without one leading slash),
    /// each compared whole with the routed path, that get the id only.
    pub fn new(tenants: Tenants<S>, streaming: &'static [&'static str]) -> Self;
}
impl<S: Send + Sync + 'static> ScopeResolver for TenantResolver<S>;
```

The resolver calls `Tenants::select`; `None` (a bypass path) is an empty
`RequestScope`. Otherwise the scope's tenant is `TenantId`'s text, so
`cx.tenant()` is the id and an end user of another tenant is refused by
the framework. Its extension is a `TenantLease<S>` (the handler reads
`cx.scoped::<TenantLease<S>>()`), or a `StreamTenant` for the listed paths.
**The id-only variant** exists because a served stream keeps its scope
until it ends (up to the stream lifetime): a lease in it would keep the
tenant's pool in use, and a suspended tenant's stream alive, for that
long. A streaming resolution opens nothing; a stream takes
`tenants.get(id)` for each read and gives it back. The resolver passes
errors on unchanged (only `Tenants` adds the serving domain), logs nothing
and opens no span.

### 9.5 The host snapshot and CORS

```rust
/// Every host of every active tenant, replaced as a whole. Starts empty; clones share it.
#[derive(Clone, Default)]
pub struct HostSnapshot { /* private */ }
impl HostSnapshot {
    pub fn new() -> Self;
    pub fn tenant_of(&self, host: &str) -> Option<TenantId>;   // compared as given
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn replace(&self, hosts: impl IntoIterator<Item = (String, TenantId)>); // last pair wins
}
pub async fn refresh_hosts(snapshot: &HostSnapshot, registry: &Registry) -> Result<Vec<TenantInfo>, AppError>;
pub fn predicate(snapshot: HostSnapshot, rules: SelectorRules)
    -> impl Fn(&str, &http::request::Parts) -> bool + Send + Sync + 'static;
```

`hosts::predicate` is the function for `Cors::origin_predicate`:
synchronous, no I/O, and it borrows nothing (the snapshot and the rules
are moved in; a product passes clones). It answers for **id-header hosts
only** and never looks at the path: it allows a request when its host is
an id-header host of the rules, it has exactly one `Origin`, and that
origin's canonical host is in the snapshot. Any active tenant's origin
passes it; whether that origin may call *the selected tenant* is decided
again at resolution (row 3 above), because CORS only shapes response
headers. A request to another kind of host gets no cross-origin headers
from it; a product that wants more composes its own function. On a
loopback host in both lists it answers as for an id-header host, also for
a request under the path-label prefix, whose selection has no origin
check (9.4). Methods,
allowed and exposed headers and the max age are the product's `Cors`
settings. `Debug` of the snapshot prints its size only.

`refresh_hosts` reads the active tenants with one statement
(`Registry::active`), returns the connection, then swaps the map and
returns what it read (the listener compares it, 9.7); on failure the old
snapshot stays. Reloads are serialised across clones.

### 9.6 The tenant cache

```rust
pub type ScopeFuture<'a, S> = Pin<Box<dyn Future<Output = Result<S, AppError>> + Send + 'a>>;

/// Builds a tenant's scope on its migrated pool. May take seconds; may fail.
pub trait ScopeFactory: Send + Sync + 'static {
    type Scope: Send + Sync + 'static;
    fn build<'a>(&'a self, info: &'a TenantInfo, pool: PgPool) -> ScopeFuture<'a, Self::Scope>;
}

/// The tenants this process serves: one fixed scope, or a cache of scopes. Clones share everything.
pub struct Tenants<S> { /* private */ }
impl<S: Send + Sync + 'static> Tenants<S> {
    pub fn fixed(scope: S) -> Self;                                  // `fixed_with_bypass(scope, &[])`
    pub fn fixed_with_bypass(scope: S, bypass: &'static [&'static str]) -> Self;
    pub fn builder<F: ScopeFactory<Scope = S>>(registry: Registry, connector: Connector,
        names: ObjectNames, rules: SelectorRules, migrator: &'static sqlx::migrate::Migrator,
        factory: F) -> TenantsBuilder<S>;
    /// The lookup, the state check, the header and origin checks; `None` for a bypass path.
    pub async fn select(&self, request: &http::request::Parts) -> Result<Option<TenantId>, AppError>;
    pub async fn resolve(&self, request: &http::request::Parts) -> Result<Option<TenantLease<S>>, AppError>;
    pub async fn get(&self, id: TenantId) -> Result<TenantLease<S>, AppError>;
    pub fn peek(&self, id: TenantId) -> Option<TenantLease<S>>;
    pub async fn invalidate(&self, id: TenantId) -> bool;
    pub async fn evict_idle_older_than(&self, idle: Duration) -> usize;
    pub async fn refresh_hosts(&self) -> Result<Vec<TenantInfo>, AppError>;
    pub async fn for_each_active<F, Fut>(&self, visit: F) -> Result<WalkReport, AppError>;
    pub async fn migration_sweep(&self) -> Result<WalkReport, AppError>;
    pub fn stats(&self) -> TenantsStats;
    pub fn hosts(&self) -> Option<&HostSnapshot>;
    pub fn registry(&self) -> Option<&Registry>;
    pub fn domains(&self) -> ErrorDomains;
    pub async fn close(&self);
    #[cfg(feature = "runtime")] pub fn into_unit(self) -> impl FnMut(UnitContext) -> BoxFuture<..>;
}

/// The keyed form's settings; `build` refuses what `KeyedPoolsBuilder::build` refuses.
pub struct TenantsBuilder<S> { /* private */ }
// domains, capacity, max_connections, acquire_timeout, create_timeout, evict_idle_after,
// reap_interval, connection_idle_timeout, connection_max_lifetime; build() -> Result<Tenants<S>, AppError>

/// Counters only grow; the rest are gauges.
#[non_exhaustive]
pub struct TenantsStats { pub capacity, pub warm, pub in_use, pub creating, pub reserved, pub retiring: usize,
    pub creations, pub create_failures, pub evictions, pub capacity_refusals: u64 }

/// A tenant's scope, held for as long as it is used. `Clone` and `Deref` whatever `S` is.
pub struct TenantLease<S> { /* private */ }
impl<S> TenantLease<S> { pub fn id(&self) -> TenantId; pub fn info(&self) -> &TenantInfo; }

#[non_exhaustive]
pub struct WalkReport { pub visited: usize, pub busy: usize, pub failed: Vec<TenantId> }
```

The builder's defaults are those of the framework's `KeyedPools`:
capacity 64, 5 connections per tenant, a 5 s acquire, a 30 s create
deadline, idle eviction after 10 min, reaping every 30 s, a connection
idle timeout of 10 min and a lifetime of 30 min; the error domains are
`ErrorDomains::new()` unless `domains` is set.

The keyed form is a `KeyedPools<TenantId, _>`: its connector reads the
row (`Registry::tenant`) and returns `Connector::candidates`; its `init`
runs the tenant migrator **as the tenant's role**; its `build_value`
rereads the row as committed now (`Registry::tenant_now`) and calls the
factory. The cached value carries the `TenantInfo` it was built from, so
the scope type needs no trait. Both reads refuse a row that is not
`active`, so **a creation never caches a tenant that is not active**. A
warm scope of a tenant that later left `active` is refused at the next
`get` by its state check (once that lookup shows the new state) and its
entry is retired by the listener's invalidation, by `invalidate` or by idle
eviction; `peek` reads no row and can still return it until then.
**A cold open is consistent with the row it is published for**: the
committed re-read must name the same database, role and role key (id and
generation) the pool was opened with, else the open is
`TENANT_UNAVAILABLE` (log stage `row`), the factory is not called, nothing
is cached and the next `get` opens from the current row. The tenant
migrator must not be built with `ignore_missing`, which would let a
database ahead of the binary pass.
Both also check the stored row, the connector's read before anything
connects and the build's read before the factory runs: the cluster is
`MAIN_CLUSTER`, the stored role equals `ObjectNames::role(id)` exactly
(the role used is the derived one, never a parse of the text), and the
stored database name passes `DatabaseName::parse` (an adopted name is
allowed). A row that fails the first check connects nothing; a failed
check answers `TENANT_UNAVAILABLE` (logged with the stage `row`), as a row
the registry cannot decode does.

| State (the row's, and the built value's) | `get` and `select` |
|---|---|
| `active` | served |
| `provisioning` | `TENANT_PROVISIONING` |
| `suspended` | `TENANT_SUSPENDED` |
| `purging`, `purged`, no row | `TENANT_NOT_HERE` |

`get` checks the state before the open with a request's lookup by id
(`Registry::lookup`): through the lookup cache, joining a read of the same
id in flight, otherwise under a lookup permit taken without waiting; no
free permit (or a second join without an answer) is `TENANT_BUSY`. The
reads inside a creation are internal reads without a permit. A
creation's outcome is decided by where an error came from:

| A creation ends with | The caller gets |
|---|---|
| a refusal of `ScopeFactory::build` that carries a reason | that error exactly as the factory wrote it (code, message, reason, domain, metadata, field violations, and the retry hint to the millisecond), whatever its reason text, even one equal to a framework or serving reason, and without the serving domain added: the single-owner seam |
| a serving reason of this crate (the row changed under it) | that error, in the serving domain |
| `KEYED_CAPACITY` (or, before the creation, no free lookup permit) | `TENANT_BUSY` |
| a panic anywhere in the creation, the factory's included | `INTERNAL` |
| anything else: connect, rejected credentials, a failed migration, a database ahead of the binary, a refused stored row, a row that no longer names the pool's database, role or role key, the create deadline, a closed cache, a build error without a reason | `TENANT_UNAVAILABLE` |

The product's refusal is recognised by a private mark it carries across
the keyed cache, never by its reason, so an unmarked error can only be the
framework's or this crate's. **What a failed open logs**: the framework's
keyed cache writes one event with the error's code, reason and source
chain; inside a creation the only source is a fixed stage text
(`opening the tenant failed: control_read`, `row`, `candidates`,
`migrate` or `build`, the first and the fourth followed by
` (sqlstate <code>)` when the failure carried a five-character SQLSTATE),
or the framework's own connect failure, which carries no driver text; a
state refusal has no source. The
product's error loses its source and the cache does not log it; a factory
that wants its cause logged writes its own event, without connection
detail.

**Cold-open semantics**, inherited from `Keyed` and relied on:

- One creation per tenant however many callers wait; all get its outcome.
- The creation runs on a task of its own under one deadline
  (`create_timeout`, 30 s by default) that covers the connect, the
  migration and the build. A product with a slow build raises it.
- **Cancelling a waiter cancels nothing and frees no slot.** Warm and
  creating entries share the capacity, so a caller that disconnects cannot
  turn cold opens into unbounded work.
- A failure is never cached; the next `get` tries again. A product whose
  build refuses on purpose bounds that cost itself.
- `invalidate` forgets the tenant's lookups and retires a warm entry or a
  creation; holders keep their lease until they drop it; a retired
  creation runs to its end and its value is discarded.
- `peek` never opens, reads no row and does not count as a use, so it can
  return the warm scope of a tenant whose row changed a moment ago.
- `select` and a streaming resolution never open a tenant.

`Tenants::fixed(scope)` and `Tenants::fixed_with_bypass(scope, prefixes)`
serve `TenantId::LOCAL` alone through the same calls: `select` answers
`None` for a path under a bypass prefix (the comparison of 9.4) and
`Some(TenantId::LOCAL)` otherwise, reading nothing else of the request;
`get` of any other id is `TENANT_NOT_HERE`; walks visit the one scope;
`stats` reports `warm` and `capacity` 1; there is no registry, no snapshot
and no cache (`invalidate` is `false`, `refresh_hosts` reads nothing).

**Walks.** `for_each_active(f)` visits the active tenants in id order, one
after another, with a lease each; a tenant the cache or the lookup
refuses on its own as busy (the cache full, no lookup permit) is counted
and skipped, any other failure (a refusal of the product's factory
included, whatever its reason: told apart by provenance, never by reason
text) is logged and recorded, and only a failed list read fails the walk
(`WalkReport { visited, busy, failed }`, `visited` counting every tenant
tried). **`migration_sweep()`** opens every active tenant (which migrates
it), at most two at a time and never more than the cache's capacity, drops
the lease, then stores the migrator's latest up version with `FOR UPDATE
SKIP LOCKED`: it never waits on a lifecycle call's row lock. A skipped
(locked or missing) row's write is retried once after the others, then
counted `busy`; an open the cache or the lookup refuses as busy is counted
`busy` without a retry; a product refusal and a failed write are in
`failed`. A migrator without an up migration, and the fixed form, store
nothing. A cold open itself writes no version.

Seams for what is out of scope (3, "Out"): single-owner tenants are
`ScopeFactory::build` and its refusal above; capacity by weight is
`stats()` with `evict_idle_older_than`; an eviction callback is the
scope's `Drop`, which runs before its pool closes, and a disposal hook
would be a `sekvent-db` change passed through `TenantsBuilder::build`.

Capacity against the server: `capacity × max_connections`, plus headroom
for retiring values, plus the lifecycle pools of 10.1, stays below the
cluster's limit. The role's connection limit is the server's, counted over
every process that serves the tenant; the crate checks it against nothing
(`p` processes can hold up to `p × max_connections` of the role's
connections).

### 9.7 The invalidation channel

One NOTIFY channel on the control database; the product names it
(`invalidation::Channel::new`, the grammar of `DatabaseName`).

```rust
pub struct ChangeKind(/* private */);   // `^[a-z][a-z0-9_]{0,15}$`; `STATE`, `HOSTS`, `new(&str)`
pub struct Topic(/* private */);        // `^[A-Za-z0-9_-]{1,64}$`; `new(&str)`
#[non_exhaustive]
pub enum Message {
    Change { tenant: TenantId, kind: ChangeKind },
    Hint { tenant: TenantId, topic: Topic, seq: i64 },
}   // parse(&str) -> Option<Self>, encode(&self) -> String
```

| Message | Payload | Meaning |
|---|---|---|
| change | `t:<tenant>:<kind>` | the tenant's control row or hosts changed. Kinds `state` and `hosts` are the crate's; a product adds kinds of its own |
| hint | `e:<tenant>:<topic>:<seq>` | product data of the tenant moved; `seq` a non-negative `i64` in decimal without a sign or a leading zero |

`Message::parse` accepts exactly these shapes, the tenant in `TenantId`'s
one text; anything else is ignored at `debug`. The grammar is checked once,
when a `ChangeKind` or `Topic` is made (`INVALID_ARGUMENT` outside it).

**The sender** (`Sender::new(control, channel)`, `change(tenant, &kind)`,
`hint(tenant, &topic, seq)`; clones share the queue): both calls are
synchronous, cannot fail and **never make the caller wait**. They queue
under a short mutex; at most one statement is in flight and it carries the
whole queue (`pg_notify` with bound values); the sender takes its
connection before it takes the queue, so what was queued while it waited
goes out in the same statement. The highest `seq` per tenant and topic
wins, a negative `seq` is ignored, a change is kept once per tenant and
kind, and changes go out in the order queued and before hints. A failed
acquire or statement drops that batch with one warning and newer work is
still sent; a dropped batch is not re-sent. The resync below repairs what
a dropped change said about a tenant's row, not a hint or a
product-defined kind. The warning names the error's class and the number
of messages dropped, never a payload. Outside a tokio runtime a call only
queues: the message goes out with the next round of a send already
running, or with the next call made inside a runtime. A send task that
never ran (its runtime shut down) or panicked does not stop later sends:
the flag that says a send runs is cleared by a guard made before the task
is spawned. The queue has no limit on the number of distinct keys (tenant
and kind, tenant and topic) while a send is stalled (question 12 of 15).

**The listener** (`Listener::new(tenants, channel)` with `.sink(sink)` and
`.resync_every(interval)`, default 60 s, zero refused; `run(ready,
shutdown)`; `into_unit()` with the feature `runtime`; `listen(tenants,
channel, sink, ready, shutdown)` as the short form), a `PgListener` on the
control pool:

| Event | Built in | Then |
|---|---|---|
| a change | forget the tenant's lookups, reload the active list (which refreshes the host snapshot and that tenant's entry in the baseline), then retire the tenant's scope | `sink.change(tenant, kind)`, also when the reload failed |
| a hint | nothing | `sink.hint(tenant, topic, seq)` |
| each resync interval | clear every lookup, reload the snapshot, compare the active tenants (`TenantInfo` only) with the last load, retire each one that changed, joined or left | `sink.resync(&changed)` after a successful load |
| a read of the channel that failed without ending the listener (a lost connection, or any other read error) | drop the session, take a new connection from the pool and listen on it, then a resync as above | `sink.resync(&changed)` when that resync succeeds, then `sink.reconnected()`, also when it failed: the product treats every waiter as stale |

- The first `LISTEN` and the first load must succeed before `ready` is
  called; a failure before that is `run`'s result. The first load is the
  baseline: no sink call, no invalidation. So the listener is started
  before the cache serves requests: a scope opened before the baseline is
  never compared with anything. A fixed `Tenants` is
  `FAILED_PRECONDITION` without I/O.
- Retiring takes the scope out of the cache at once; its pool closes in the
  background on the cache's own task and never blocks the listener (it
  relies on the keyed cache removing the entry before it awaits the
  disposal: question 13 of 15).
- The resync timer is checked before the next notification. The first
  periodic resync is one interval after `ready`; each next one is one
  interval after the previous resync ended, a reconnect's resync included;
  a tick that came due during other work runs once, at once, and is never
  replayed. `resync_every` refuses zero; an interval above 100 years never
  ticks.
- After any failed read that does not end the listener, the listener drops
  its session, takes a new connection from the pool (sessions are built
  without the driver's eager reconnect, so a loss is reported by the read
  itself) and listens again, then resyncs (with the sink's `resync`, as
  any resync) and then calls `sink.reconnected()`, also when that resync
  failed. A lost connection is listened on again at once; a failed read
  and a failed attempt to listen again are logged at `warn` with their
  code and followed by a 1 s wait, during which the timer and `shutdown`
  still apply.
- A resync's load has **no deadline**: it runs as long as the control read
  takes and a tick does not interrupt it. A load that fails is a failed
  resync and keeps the previous baseline; a successful one resets the
  count. **A second consecutive failed resync empties the snapshot**: an
  origin is then refused rather than allowed from stale data.
- A change-driven reload that fails does not count towards the failed
  resyncs, still retires the scope, leaves the baseline unchanged, and the
  sink is still told. A successful one updates the baseline for that tenant
  only, so another tenant's lost notification is still found by the next
  resync. The scope is retired after that read, so a scope opened before
  the read never outlives it and a later lost change still differs from
  the baseline.
- `shutdown` ends the listener with `Ok(())`, also while a load is
  awaited; a closed control pool ends it with `UNAVAILABLE`.

```rust
/// The product's (waking its streams). Every method defaults to nothing; each is called on the
/// listener's task, must return quickly and must not panic.
pub trait Sink: Send + Sync + 'static {
    fn change(&self, tenant: TenantId, kind: &ChangeKind);
    fn hint(&self, tenant: TenantId, topic: &Topic, seq: i64);
    fn resync(&self, changed: &[TenantId]);
    fn reconnected(&self);
}
impl Sink for () {}
impl<T: Sink + ?Sized> Sink for Arc<T> { /* forwards every call to `T` */ }   // one sink shared by the listener and the product
```

The guarantee across processes is therefore conditional: one notification
drops a tenant's scope and lookups on every listening process in the
normal case, and a lost notification about what a tenant's row holds
(state, hosts, primary host, role key: the fields of `TenantInfo`) is
repaired by the next successful resync, also when its batch was dropped by
the sender. A lost hint or a lost change of a product-defined kind is not
repaired: after `resync` and `reconnected` the product's consumers catch up
from their own durable data. A resync happens only when the control
database answers, so there is no bound in seconds during an outage or a
hanging read. The 30 s lookup
lifetime bounds how long a cached lookup answer is reused and nothing
else: it is not a bound on how fast a changed tenant stops being served
(the caller that leads a read in flight still gets that read's answer, and
a lease already held survives an invalidation until it is dropped).

### 9.8 Tests

`tests/common` gains the control fixture and the test product (11.1). Unit
tests, without a server, sit next to the code: the serving errors and the
state table (`error.rs`); state texts, `local()`, `origin()` and narrow
`Debug` (`info.rs`); the row-decoding rules and `ProvisionStep`'s texts and
order (`rows.rs`); every verified name in the schema text (`schema.rs`);
`Registry::new` below 3 and permits shared across clones; the lookup cache
(expiry, both partitions, replacement, a clock set back, `forget`, the
epoch) on a `ManualClock`, and joining, the second join, a cancelled, a
failed and a panicked read on a gated fake read; the lifecycle admission's
wait, timeout and cancelled wait on a paused clock; the selector table of
9.4 row by row, `is_bypass`, the request host, the claims and the builder
refusals (`select.rs`); the snapshot and the predicate (`hosts.rs`); the
resolver on a fixed `Tenants`, bypass included; the fixed form, the error
classification, the mark and the stored-row check (`tenants/`); the
message codec; the sender's coalescing and ordering on a paused clock with
a gated fake drain; the listener's rules on a fake source and target on a
paused clock (baseline, change, hint, the timer, one and two failed
resyncs, a load that never returns and is not cut short, reconnect,
shutdown, a sink shared through an `Arc`).

Docker tests, one file each:

| File | Proves |
|---|---|
| `tests/control.rs` | registry reads; uniqueness mapped to `TENANT_STATE` (the partial index name confirmed by a real violation); a repeated insert; the lock sees committed hosts; the active list; the version store (raised, unchanged, locked, missing); a database name reused only after purge; the stored step and cluster; `verify_schema` with a table, a column and a constraint missing and an extra column, `admin_audit` checked only with `admin` |
| `tests/admission.rs` | no permit is `TENANT_BUSY` with no control connection taken (observed on `pg_stat_activity`), also through another clone; joiners; a cancelled caller's read fills the cache; internal reads take no permit; the loopback label rule; a host lookup warms the id space |
| `tests/resolve.rs` | every state through each selector; the id check after the state check; the origin check; the reserved prefix; snapshot refresh and the predicate after a host change; a keyed lease in the scope; a streaming or bypass resolution builds nothing |
| `tests/tenants.rs` | the state table of 9.6; twenty cold `get`s migrate and build once, as the tenant's role; a gated slow build; a reasoned refusal reaches the caller unchanged, also with a framework reason; one without a reason; a panic; the captured log of failed opens holds fixed texts only; a refused stored row connects nothing; a cancelled waiter (dropped after the build arrived) neither cancels nor frees; `invalidate` under a held lease; capacity; eviction; rejected credentials; a failed migration retried; a database ahead of the binary; no tenant reaches another's data; walks; the sweep with a row locked by the test; the unit |
| `tests/invalidation.rs` | a change sent on one `Tenants` drops the scope on another; a hint reaches the sink; a lost notification repaired by the next successful resync on a short interval; a real reconnect; a reconnect after failed read attempts (the control owner refused a login for a while) reported to the sink; no `ready` when the first `LISTEN` fails; closing the control pool ends a running listener; a fixed `Tenants` refused; the unit |

**Consumer check.** The product's serving path runs on `Tenants`,
`TenantResolver`, the snapshot and the channel; its isolation suite (no
tenant reaches another's data, a suspended tenant stops being served on
every process) passes unchanged.

## 10. Slice 1c: lifecycle and admin

What the serving half gives this slice, with no signature of 9 changed:

- `TenantRow::provision_step` is the typed `control::ProvisionStep` with
  the stored texts of 10.2, and `TenantRow::db_cluster` is read.
  `control::rows` records a step (`set_step`), stores a role key
  (`set_role_key`), clears the hosts (`clear_hosts`), counts the rows that
  are not `purged` (`count_live`) and pages a list (`page`). Repository
  writes that set a timestamp take `now: SystemTime`, which the
  provisioner takes from its `Clock`.
- `admin_audit` is in `REFERENCE_SCHEMA`, and `verify_schema` checks it
  with the feature `admin`; the admin audit writes it.
- `Registry::admit` with `LifecyclePermit` (10.5), `Registry::tenant_now`
  (the committed row, no cache) and `Registry::forget`.
- A creation of the tenant cache never caches a tenant that is not
  `active`, and `get` refuses one (9.6), so the lifecycle pools of 10.1 are
  the only way to open one, and `LifecycleCapability` gates those, not
  `Tenants`. The cache opens a tenant only when its stored role is the
  derived one, its database name parses and its cluster is `main`:
  provisioning and adoption store exactly that.
- `Tenants::invalidate` forgets the tenant's lookups; `Sink` is object-safe
  and implemented for `Arc<T>`, so the listener and the announce of 10.5
  share one sink; `ChangeKind::STATE` and `ChangeKind::HOSTS` are the kinds
  the announce sends.
- The admin App has no scope resolver and the admin authenticator's end
  user names no tenant (10.6), so a fixed `Tenants` naming
  `TenantId::LOCAL` never meets an admin end user; a process that serves a
  fixed scope installs `Verifier::refusing()`.

### 10.1 Files

| Files | Module-page section |
|---|---|
| `src/lifecycle/hooks.rs`, `provisioner.rs`, `open.rs`, `provision.rs`, `tests/provision.rs` | "Provision a tenant" |
| `src/lifecycle/state.rs`, `purge.rs`, `announce.rs`, `usage.rs`, `tests/lifecycle.rs`, `tests/purge.rs` | "Suspend, resume, change hosts, purge" |
| `src/lifecycle/adopt.rs`, `tests/adopt.rs` | "Adopt an existing database" |
| `src/admin/*.rs`, `tests/admin.rs` | "The admin surface" |
| `src/manager/*.rs`, `tests/manager.rs` | "Call a product's admin API" |

#### Hooks, the capability and the handle

```rust
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AppError>> + Send + 'a>>;

/// The `check` of the default adoption refusal.
pub const NOT_CHECKED: &str = "not_checked";

/// The product's side of the lifecycle. Every method has a default; each does
/// nothing and succeeds, except `check_adoptable`, which refuses.
pub trait TenantHooks: Send + Sync + 'static {
    type Seed: Send + Sync + 'static;
    fn seed<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool, seed: &'a Self::Seed) -> HookFuture<'a, SeedOutcome>;
    fn check_adoptable<'a>(&'a self, candidate: &'a AdoptCandidate, pool: &'a PgPool) -> HookFuture<'a, ()>;
    fn adopted<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()>;
    fn before_suspend<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()>;
    fn before_resume<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()>;
    fn describe<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, Description>;
    fn usage<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, UsageCounters>;
}

/// What a seed hands back once. `Debug` prints whether there is a handover.
#[derive(Clone, Default)]
pub struct SeedOutcome { pub handover: Option<Secret> }   // SeedOutcome::with_handover(secret)

#[non_exhaustive]
pub struct AdoptCandidate { pub tenant: TenantId, pub database: DatabaseName, pub subdomain: String, pub display_name: String }
pub struct Description { pub display_name: String, pub attributes: BTreeMap<String, String> }
pub struct UsageCounters { pub counters: BTreeMap<String, i64>, pub last_activity: Option<SystemTime> }
```

- **Hooks get a pool, never a scope.** Every hook but `check_adoptable`
  receives a pool on the tenant's **migrated** database as the tenant's
  role. For an `active` tenant that the provisioner's tenant cache holds
  warm (with the database, role and role key the row names) it is that
  entry's pool, held through a crate-private lease (`TenantCache::lease_pool`
  returns a `PoolLease` keeping the `KeyedRef`, so the entry is in use and
  never evicted meanwhile; `Built` keeps a clone of its pool for this).
  Otherwise it is a pool opened for this one operation (at most two
  connections, its first connected within 10 s, closed within 10 s when the
  operation ends) outside the tenant cache, which first reserves one slot
  of the cache's capacity (`TenantCache::reserve` over `KeyedPools::reserve`)
  and holds it until the pool is closed: a full cache answers
  `UNAVAILABLE` / `TENANT_BUSY` (retry 1 s, no domain) before any
  connection. A provisioning or suspended tenant never gets a cached scope,
  and an expensive scope is never built to seed, suspend or measure. A
  failure to open is `UNAVAILABLE` without a reason; its source is a stage
  text (`reserve`, `connect`, `migrate`) and at most a SQLSTATE.
- `check_adoptable` is the exception: its pool is `Maintenance::pool_on`
  on the candidate database, before the handover and before any migration
  of ours; its sessions start read-only and keep the server-side bounds of
  8.6. The default refuses every database (decision 36).
- The hooks' defaults: `seed` writes nothing and returns no handover,
  `describe` returns the display name and no attribute, `usage` no counter;
  the others do nothing.
- **`LifecycleCapability`** (crate-private): opening a tenant that is not
  `active` requires one. Its constructor is called only in
  `provision.rs`, `state.rs`, `adopt.rs` and `usage.rs`; `select`,
  `resolver`, `tenants`, `purge.rs` and `admin` never call it. A purge
  never opens the tenant.
- `Provisioner<H: TenantHooks>` is the handle, cloned cheaply, built by
  `Provisioner::builder(registry, maintenance, connector, migrator: &'static
  Migrator, sender, hooks) -> ProvisionerBuilder<H>` with the required
  `role_key_id(&str)` (a key of the connector's secrets),
  `connection_limit(u32)` (1 to 2147483647) and `tenant_cap(u64)` (at
  least 1), and the optional `provision_lock(i64)` (default
  `PROVISION_LOCK`), `display_name_max_bytes(usize)` (default
  `DISPLAY_NAME_MAX`, 255; zero refused), `labels(LabelRules)`, `tenants(Tenants<S>)`,
  `sink(impl Sink)` and `clock(Arc<dyn Clock>)`; `build()` does no I/O and
  refuses a missing or out-of-range setting with `INVALID_ARGUMENT`. The
  cache it announces to, borrows warm pools from and reserves slots in is
  held as a crate-private object-safe trait over `Tenants::invalidate`,
  `Tenants::refresh_hosts`, `lease_pool` and `reserve`
  (`tenants/keyed.rs`), so the provisioner is not generic over the scope
  type. It also offers
  `tenant(id) -> Result<TenantRow, AppError>` (`TENANT_NOT_FOUND` without a
  row), `list(after: Option<TenantId>, limit: u32) -> Result<Vec<TenantRow>,
  AppError>`, `registry()`, `hooks()` and `names()`.

### 10.2 Provisioning

```rust
pub async fn provision(&self, request: ProvisionRequest<H::Seed>) -> Result<Provisioned, AppError>;

pub struct ProvisionRequest<S> {
    pub tenant: TenantId,
    pub subdomain: String,
    pub display_name: String,          // 1 to display_name_max_bytes (default 255) bytes, not blank, no control character
    pub hosts: Vec<String>,
    pub primary_host: String,
    pub seed: S,                       // not stored; Debug leaves it out
}
pub struct Provisioned { pub row: TenantRow, pub handover: Option<Secret> }
```

Before the steps: the request is validated (the label by the product's
`LabelRules`; the display name; hosts canonical, sorted, without
duplicates, the primary among them; every violation as a field violation of
one `INVALID_ARGUMENT`, a host named by its position); the lifecycle
admission is taken; under a transaction-level advisory lock the row is
inserted when the id is new, and a new id at the tenant cap (rows not
`purged`) is `TENANT_LIMIT_REACHED`. The lock's key is the provisioner's
`provision_lock`; every process of a deployment must use the same one. A host, role or database name another
tenant holds is `TENANT_STATE`.

Each step locks the row (`FOR UPDATE`), re-reads it, does its work,
records the step and commits. A call for an existing id resumes at the
recorded step when the request matches the row (label, display name,
hosts, primary host, database name), and is `TENANT_STATE` when it does
not, or when the tenant is `suspended`, `purging` or `purged`. The steps
are `control::ProvisionStep` (9.2): `Inserted` (the empty stored text, a
new row) and the five of the table below, in that order; an unknown stored
text is `INTERNAL`. Before each step the stored cluster, role and database
name are checked (`INTERNAL`, naming the column, when changed by hand).

| Step recorded | Connection | Work |
|---|---|---|
| `role` | maintenance; control | derive the password under the configured key id, generation 1; `ScramVerifier::new` on the blocking pool; `roles::create` with the connection limit; store the key id and generation |
| `database` | maintenance | `databases::create` |
| `migrated` | a lifecycle pool as the tenant | the tenant migrator; store the schema version |
| `seeded` | a lifecycle pool as the tenant | `TenantHooks::seed` |
| `active` | control | step and state together; then announce (10.5) |

- **The handover secret is returned once.** A call that resumes after the
  seed committed returns `handover: None`; the product's admin reply says
  the secret is withheld rather than inventing one.
- A call for an id that is already `active` and matches returns the row
  and announces again; it never re-seeds.
- A refusal can follow committed work: steps are resumable, not one
  transaction.

### 10.3 Suspend, resume, set hosts, purge, usage

```rust
pub async fn suspend(&self, tenant: TenantId) -> Result<TenantRow, AppError>;
pub async fn resume(&self, tenant: TenantId) -> Result<TenantRow, AppError>;
pub async fn set_hosts(&self, tenant: TenantId, change: HostsChange) -> Result<TenantRow, AppError>;
pub async fn purge(&self, tenant: TenantId) -> Result<TenantRow, AppError>;
pub async fn usage(&self, tenant: TenantId) -> Result<Usage, AppError>;
pub async fn describe(&self, tenant: TenantId) -> Result<Description, AppError>;

pub struct HostsChange { pub subdomain: String, pub hosts: Vec<String>, pub primary_host: String }
#[non_exhaustive]
pub struct Usage { pub database_bytes: u64, pub counters: UsageCounters }   // Usage::new(bytes, counters)
```

All but `usage` and `describe` take the lifecycle admission first, then the
row lock, and decide from the locked row. A missing row is
`TENANT_NOT_FOUND`; a state the table does not list is `TENANT_STATE`.

| Call | From | To | Before the row changes |
|---|---|---|---|
| `suspend` | `active` | `suspended` | `before_suspend` on the warm entry's pool or a lifecycle pool (10.1). Strict: a tenant whose database cannot be opened (`TENANT_BUSY` for a full cache included), or whose hook fails, stays `active` |
| `resume` | `suspended` | `active` | `before_resume` on a lifecycle pool, same strictness |
| `set_hosts` | `active`, `suspended` | unchanged | validation after the lock; the host set is replaced whole; announced as `ChangeKind::HOSTS` |
| `purge` | `suspended`, `purging` | `purging`, then `purged` | nothing of the product runs |

A call that finds the target state already set changes nothing and
announces again. **Purge**: mark `purging`, commit, announce; lock again;
the stored role must equal the derived one (`INTERNAL` otherwise); on one
maintenance connection `databases::drop(database, role)` then
`roles::drop(role)`; clear the hosts, commit `purged` (the row stays as a
tombstone, its database name free again), announce. A failure after the
first commit resumes at `purging`. `active` and `provisioning` cannot be
purged.

`usage` returns the database's size (`databases::size` on the maintenance
connection) and the product's `UsageCounters`; `describe` returns the
product's `Description` for an operator's view. Both read the row as
committed now, accept exactly `active` and `suspended` tenants (a product
that refuses a suspended one does it in its adapter), take no lifecycle
admission, and run the hook on the warm entry's pool of a warm `active`
tenant, otherwise on a read-only lifecycle pool: the migrator runs first on
a read-write pool of one connection, closed before the pool with
`default_transaction_read_only = on` and `statement_timeout = 5s` is opened
(decisions 39, 43).

### 10.4 Adopt

```rust
pub async fn adopt(&self, request: AdoptRequest) -> Result<TenantRow, AppError>;

pub struct AdoptRequest {
    pub tenant: TenantId,
    pub subdomain: String,
    pub display_name: String,
    pub database: String,     // is_adoptable_name, no tenant prefix, not the control or maintenance database
    pub hosts: Vec<String>,
    pub primary_host: String,
}
```

1. Validation (the `database` field among the violations, never echoed),
   then the lifecycle admission.
2. Without a row yet: `handover::prepare`, then, while the maintenance role
   still owns the database, `check_adoptable` on the maintenance pool,
   bounded by a maintenance operation's 150 s (`UNAVAILABLE` past it). A
   refusal leaves **no control row and no role** (only `PUBLIC`'s
   `CONNECT`, 8.11).
3. The row is inserted (the cap applies); the steps of 10.2 run with the
   `database` step replaced by the checks of step 2 again
   (`check_adoptable` only while the maintenance role still owns the
   database) and `handover::adopt`, and the `seeded` step by
   `TenantHooks::adopted`. No handover secret exists.

### 10.5 The lifecycle admission and announcing

- **Admission first, then the row lock.** Every lifecycle call that
  changes a row takes `Registry::admit()` before its first control
  connection and holds it to its end: one semaphore of `max(1, (control
  max_connections − 1) / 2)` permits, a wait of 10 s by default
  (`Registry::with_admission_wait(Duration)`, a value's own setting like
  `labels`), then `UNAVAILABLE` without a reason.
  `Registry::lifecycle_available()` reports the free permits, so a product
  drops a semaphore of its own and its tests observe the admission. A lifecycle call holds a control connection while it
  waits on the maintenance connection, a tenant open or a hook; the
  admission keeps such calls from holding the whole pool.
- **Brief holders never wait while holding.** Every other holder of a
  control connection (lookups, the sender, the sweep, `tenant`, `list`, a
  product's jobs) holds it for one statement.
- **Announcing a committed change** runs in a spawned task the call
  awaits, so a caller that goes away still announces, in this order:
  forget the tenant's lookups; invalidate its scope; `Sink::change` on
  this process; `Sender::change` for the others; reload this process's
  host snapshot. A failed reload is a warning, never the call's error.

### 10.6 The admin surface (feature `admin`)

The product declares its admin service in its own proto, serves it from a
second App (`App::register_as`) on a listener of its own with
`SERVE_AUTH=bearer`, registers the authenticator below as that App's one
end-user authenticator, and wraps each RPC body in the guard.

```rust
pub const MAX_LIFETIME: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoExtra {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminClaims<X = NoExtra> {
    pub token_id: Uuid,
    pub subject: String,
    pub tenant: Option<TenantId>,
    pub op: String,
    pub expires_at: SystemTime,
    pub extra: X,
}

impl<X: DeserializeOwned + Send + Sync + 'static> Verifier<X> {
    pub fn new(issuer: &str, audience: &str, keys: EdDsaVerificationKeys) -> Self;
    pub fn refusing() -> Self;
    pub fn with_subject_check(self, check: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self;
    pub fn with_domains(self, domains: ErrorDomains) -> Self;
    pub fn verify(&self, token: &Secret, op: &str, now: SystemTime) -> Result<AdminClaims<X>, AppError>;
}
pub fn parse_keys(text: &str) -> Result<EdDsaVerificationKeys, AppError>;   // "kid:<base64url>[,…]"
```

| Claim | Rule |
|---|---|
| header | EdDSA; `kid` required and known (`EdDsaVerificationKeys`) |
| `iss`, `aud` | equal the verifier's, each one string |
| `iat`, `exp` | `iat <= now < exp`; `0 < exp − iat <= 60 s`; leeway 0 |
| `nbf` | optional; `<= now` |
| `sub` | a non-empty string that passes the product's subject check |
| `jti` | required; a lower-case hyphenated UUID: the token id |
| `tid` | a `TenantId` text, not the nil id; absent only for a call that addresses no tenant |
| `op` | the RPC path without its leading slash, `<package>.<Service>/<Rpc>`; equal to the path called |
| extra | the product's type, flattened |

An optional claim given as `null` is a refusal, not an absence. **Every
failure, whatever the cause, is the one answer** `UNAUTHENTICATED` /
`ADMIN_TOKEN_INVALID` with the constant end-user message in the admin
domain; the cause is logged at `debug` without the token. `parse_keys`
refuses an entry with `INVALID_ARGUMENT` naming its position only.

```rust
pub fn authenticator<X>(verifier: Verifier<X>, services: &[&str], clock: Arc<dyn Clock>)
    -> Result<AdminAuthenticator<X>, AppError>;
```

`AdminAuthenticator<X>` is an `EndUserAuthenticator`: the routed path must
be `/<service>/<Rpc>` of one of `services` (names of `[A-Za-z0-9_.]`, at
least one); the token is `BearerShape::token`; the claims become an
extension of an `EndUser` whose subject is `sub` and which names no tenant
(decision 37). Its rejection is an `EndUserRejection` with
`ADMIN_TOKEN_INVALID` and the admin domain. A call without a bearer never
reaches it: the App's default `BearerShape` precheck answers the plain
constant `UNAUTHENTICATED`.

```rust
impl AdminAudit {
    pub fn new(control: PgPool, clock: Arc<dyn Clock>) -> Self;
    pub fn with_domains(self, domains: ErrorDomains) -> Self;
    pub async fn record<X>(&self, claims: &AdminClaims<X>) -> Result<(), AppError>; // before the body
    pub async fn finish(&self, token_id: Uuid, ok: bool);                          // after the body
}

pub mod guard {
    pub async fn run<X, T, F, Fut>(audit: &AdminAudit, cx: &CallContext, op: &str,
        tenant: Option<TenantId>, body: F) -> Result<T, AppError>
    where X: Clone + Send + Sync + 'static, F: FnOnce(AdminClaims<X>) -> Fut,
          Fut: Future<Output = Result<T, AppError>>;
    pub fn claims<X: Send + Sync + 'static>(cx: &CallContext) -> Option<&AdminClaims<X>>;
    pub fn end_user<X: Clone + Send + Sync + 'static>(claims: AdminClaims<X>) -> EndUser;
}
```

`guard::run`, in this order:

1. The claims from the context; none is `ADMIN_TOKEN_INVALID`.
2. `claims.op == op` and `claims.tenant == tenant` exactly (`None` only
   for a call that addresses no tenant), else `ADMIN_TOKEN_INVALID`.
3. **The token id is recorded before the body runs**: a row in
   `admin_audit` with the time, operation, tenant and subject, outcome
   `started`. A second insert of the same id violates `admin_audit_pkey`
   and is `ALREADY_EXISTS` / `ADMIN_TOKEN_REPLAYED`; the body does not run.
4. The body, with the claims. It never checks the token again.
5. The outcome is stored (`ok` or `failed`); a failure to store it is a
   warning; a cancelled call leaves `started`.
6. An error with a reason and no domain gets the admin domain.

Which subjects an operation accepts stays the operation's rule. Pruning
`admin_audit` is the product's, never below the token lifetime. Steps 3
and 5 are `AdminAudit::record` and `AdminAudit::finish`, public for a body
a product runs outside the guard: record before the body (a duplicate
token id is the replay reason and the body must not run), finish after it.
Both take the time from the audit's clock, so no `record_at` exists.

### 10.7 The manager kit (feature `manager`)

```rust
pub const TOKEN_LIFETIME: Duration = Duration::from_secs(60);

impl Minter {
    pub fn new(issuer: &str, key: EdDsaSigningKey, clock: Arc<dyn Clock>) -> Self;
    pub fn mint<X: Serialize>(&self, audience: &str, tenant: Option<TenantId>, op: &str,
        subject: &str, extra: &X) -> Result<Minted, AppError>;
    pub fn public_key_entry(&self) -> String;   // "kid:<base64url>", what parse_keys reads
}
pub struct Minted { pub token: Secret, pub token_id: Uuid, pub expires_at: SystemTime }

impl AdminClient {
    pub fn new(base_url: &str, audience: &str, minter: Minter) -> Result<Self, AppError>;
    pub fn in_cluster(base_url: &str, audience: &str, minter: Minter) -> Result<Self, AppError>;
    pub async fn call<Req: prost::Message, Rep: prost::Message + Default, X: Serialize>(
        &self, op: &str, tenant: Option<TenantId>, subject: &str, extra: &X,
        deadline: Duration, request: &Req) -> CallOutcome<Rep>;
}

#[non_exhaustive]
pub enum CallOutcome<T> { Replied(T), NotSent(AppError), Refused(AppError), Unknown(AppError) }
// CallOutcome::into_result(self) -> Result<T, AppError>
```

A token lives 60 s, has a fresh random id, and names one operation and at
most one tenant; absent optional claims are omitted, never `null`; extra
claims must be an object that names none of the minter's claims. One call
mints one token, sends one gRPC-Web frame to `<base URL>/<op>` and reads
at most 4 MiB, all within `deadline`; **no automatic retry**, no
redirects. `new` accepts `https`, and `http` for a loopback host only;
`in_cluster` accepts `http` for any host (decision 38).

| `CallOutcome` | When | What the caller may assume |
|---|---|---|
| `Replied` | status OK with exactly one message that decodes as the reply | the product acted |
| `NotSent` | an invalid `op`, encoding or minting failed, or no connection was made | nothing happened; safe to try again |
| `Refused` | the product answered a status other than OK, except `UNAVAILABLE` without a reason | the product decided; not proof that nothing changed (a step may have committed before the refusal) |
| `Unknown` | a failure after sending, an HTTP status other than 200, a malformed, compressed or oversized reply, the deadline, or `UNAVAILABLE` without a reason | anything; read the tenant back, and repeat only an operation that converges |

A refusal carries the product's code, message, reason, domain, metadata,
retry hint and field violations from `grpc-status-details-bin`. Transport:
gRPC-Web over `sekvent_client::reqwest_builder()` (rustls, certificates
verified), driven directly (decision 38).

### 10.8 Tests

Docker, on the fixture of 11.1: provisioning end to end and resumed after
a failure injected at each step (the seed is committed once and its secret
returned once); the cap; a mismatching repeat; the state table of 10.3
with idempotent repeats; a strict suspend with the tenant's database
closed; a purge resumed after each step, one that refuses a database its
role does not own, one that leaves a reused name alone; adoption with each
refusal leaving no row and no role, the default `check_adoptable`
refusing; the lifecycle admission (permits exhausted waits, then
`UNAVAILABLE`); every committed change invalidates this process before the
call returns and reaches a second `Tenants`; a dropped call still
announces. Admin, without a server: a verifier table with one row per rule
above, each failure the one answer; `null` claims; two keys during a
rotation. With a server: the guard's order (a wrong `op` or tenant writes
no audit row; a replay does not run the body; the outcome is stored); an
admin App served on `127.0.0.1:0` and called by `AdminClient` with each
`CallOutcome` (`127.0.0.1:1` for `NotSent`, a server that closes after
reading for `Unknown`).

**Consumer check.** A product's lifecycle and its admin RPC bodies run on
`Provisioner` and the guard, with its wire contracts untouched; its full
suite passes, and an operator service built on the manager kit provisions,
suspends, resumes and purges a tenant against it.

## 11. Tests on Docker

### 11.1 The fixture (`tests/common/`, the coordinator's)

Built on `sekvent_testing::PostgresHarness::shared()` (one server per test
binary, Postgres 17 by default), `#[ignore]`d and skipped unless
`docker_tests_enabled()`, as the tests of `sekvent-db` are.

- **One database per test** and one cluster identity per test, because
  roles are server-wide: the control database comes from
  `create_database()`; from its unique name the fixture derives an
  object-name prefix and the names of the roles below, so tests in one
  server never see each other's tenant roles.
- **The maintenance role is not a superuser.** The fixture creates it
  through `admin_url()` as `LOGIN CREATEDB CREATEROLE` with a known
  password, with a maintenance database of its own, and a separate
  `LOGIN` role that owns the control database. `CONNECT` and `TEMPORARY`
  are revoked from `PUBLIC` on both, so the preflight passes on a fresh
  fixture. The harness's superuser is used only to set up and to observe
  (`pg_stat_activity`, catalog reads, creating drift on purpose).
- **The control fixture** (from 1b): `control_pool_of(n)`, a control
  owner's pool of `n` connections (the plain control pool has 2, below
  `Registry::MIN_CONNECTIONS`); the reference schema applied; a registry
  on a pool of 4; the fixture's master secrets, role key and connector;
  control rows inserted by hand in any state; a whole tenant (role,
  database and an `active` row) built from the cluster operations of 1a
  with the feature `lifecycle`, which 1c replaces by the provisioner;
  `tenants(..)` and `tenants_builder(..)` over the test product.
- **The test product** (`tests/common/product.rs`, from 1b): an embedded
  tenant migrator of two migrations (an `orders` table, then one more
  column, so a sweep has a version to move); a scope that holds the
  tenant's pool, is not `Clone`, and whose `Drop` is counted and
  signalled; a factory whose build the test can gate, count, and fail with
  any error it builds (or a panic). 1c adds hooks whose seed inserts one
  row and returns a `Secret` on its first call and nothing on a second,
  and whose usage counter is the number of rows in `orders`.
- Time is a `ManualClock` (from `sekvent-context`) wherever the crate
  takes a `Clock`; tokio's paused clock serves the unit tests of timers on
  fakes; blocking is observed on `pg_stat_activity` with `await_until!`,
  never slept for; the factory's gate and counters are completion signals;
  anything with a socket binds `127.0.0.1:0`.
- Nothing is dropped per test; the container is reaped with the process.

### 11.2 Coverage

95 % of the crate's own lines, measured with all four features on and the
Docker tests included (the coverage run sets `SEKVENT_DOCKER_TESTS=1` and
passes `--include-ignored`). The configured run applies one floor to the
whole repository, so the crate's own figure is read from the summary and
is the acceptance (15, question 8): the sum of covered over total lines of
the rows under `crates/sekvent-tenancy/src/`. Pure rules (`findings`, the
drop decision, the selector table, the row decoding, the error
classification, the message codec, the listener's loop on fakes, the claim
rules) are
unit-tested so coverage does not depend on reaching every branch through a
server.

## 12. Docs, skills, example

### 12.1 `docs/modules/tenancy.md`

Created in step 0 of 1a with these headings; each slice fills its own, in
the same commit as the code: "Tenants on a database each
(`sekvent::tenancy`)", "Enable it", "What you bring", "Quick example",
"Names, hosts and labels", "Role credentials", "The maintenance
connection", "Roles and databases", "The startup preflight", "Adopting a
database", "The control schema", "Lookups and their admission", "Select
the tenant", "CORS", "The tenant cache", "Follow changes from other
processes", "Provision a tenant", "Suspend, resume, change hosts, purge",
"Adopt an existing database", "The admin surface", "Call a product's admin
API", "Configuration keys" (there are none; the table lists what the
product must read itself), "Errors and reasons", "Sizing" (pools,
connection limits, the control pool), "Security rules", "Testing", "See
also". Step 0's index rows link to these headings.

### 12.2 Same commit as the behaviour

| File | With |
|---|---|
| `docs/features.md`, `docs/README.md`, `README.md`, `AGENTS.md` | step 0 of 1a; rewritten in place when a later slice makes a row true |
| `docs/modules/db.md`, `docs/modules/components.md` | one "See also" line each to the tenancy page (1b) |
| `skills/sekvent/SKILL.md` | a recipe per slice: the preflight and cluster operations (1a); `Tenants` with a resolver, the snapshot and the listener (1b); a guarded admin RPC and a provisioning call through the manager kit (1c); pitfalls: hold the lease, the id-only scope for streams, never format a name, never store a credential |
| `skills/sekvent-api/SKILL.md` | task rows and the feature picker |

### 12.3 `examples/tenants`

A neutral product, `billing`, in three members of the root manifest:

| Member | Holds |
|---|---|
| `billing-api` | the proto: the public `Invoices` service (`billing.invoices.v1`) and the product's own `TenantAdmin` service (`billing.admin.v1`) |
| `billing` | the scope and its `ScopeFactory`, `BillingHooks` (the seed hands an owner invitation over once; `usage` counts invoices; `describe` names the tenant), the tenant migrations and the control migration copied from `REFERENCE_SCHEMA`, `InvoicesService` on the call's scope, and `TenantAdminService`, each body in `guard::run` calling the `Provisioner` |
| `billing-svc` | the service: the public App and the admin App on one runtime, the tenant cache and listener units, its own configuration keys, hosts under `example.com`, the prefix `app_t_` |

Its `sekvent.toml` carries `[contract]` roots, and `contracts/` the
baselines of both services. One Docker test in `billing-svc` provisions a
tenant through the manager kit, calls `Invoices` for it and suspends it.
Examples are outside coverage.

## 13. Relation to existing notes

| Note | Relation |
|---|---|
| [`multi-tenant-building-blocks.md`](multi-tenant-building-blocks.md) | this crate is its first consumer inside the repository. `Tenants` is a `KeyedPools`; `TenantResolver` is a `ScopeResolver`; the CORS function is a `Cors::origin_predicate`; admin tokens are its EdDSA tokens; the admin App uses `register_as`; the one visible admin reason uses `EndUserRejection`. Nothing in those APIs changes. Its "Out" rows on configuration keys, a disposal hook for `KeyedPools` values and a limit on concurrent creations stand; this note adds bounds above them (9.3, 9.6) instead |
| [`serve-admission.md`](serve-admission.md) | the resolver runs where its section 4 puts it: after the precheck, the serve admission and the bounded read. The lookup admission (9.3) is a second, narrower bound inside the resolver; the lifecycle admission (10.5) is unrelated to serving. The admin App keeps the default `BearerShape` precheck |
| [`component-end-user.md`](component-end-user.md), [`end-user-extensions.md`](end-user-extensions.md) | admin claims travel as an `EndUser` extension; the tenant rule (`admit_end_user`) is what refuses an end user of another tenant once the scope names one |
| [`component-streaming.md`](component-streaming.md) | a stream keeps its scope until it ends, which is why streaming methods get `StreamTenant` (9.4) |
| [`p8-service-essentials.md`](p8-service-essentials.md) | leases stay in `sekvent-db` and are the product's tool for singleton jobs and for the single-owner seam; this crate takes none |
| [`component-c2.md`](component-c2.md) | no component and no service contract ships from this crate; the manager client speaks to a served component by path, outside the `grpc` binding |

## 14. Decisions taken in this note, open to override

1. Checked names and the verifier are **types** (`TenantRole`,
   `DatabaseName`, `ScramVerifier`); functions take them, never `&str`.
   Catalog reads that only bind a name as a value (`privileges`,
   `databases::access`) take any `&str`.
2. Statements are static SQL with bound values; no `sea-query` dependency.
3. `privileges` and `preflight` belong to `serving`; `admin` needs
   `serving`, not `lifecycle`.
4. The ownership handover is a cluster operation and lands in 1a; the
   tenant operation `adopt` lands in 1c.
5. Two reasons are new relative to the reference behaviour:
   `ADOPTION_REFUSED` with a `check`, and `CONTROL_SCHEMA_MISSING`.
6. Hooks run on a dedicated lifecycle pool, outside the tenant cache.
7. The pool of `check_adoptable` starts its sessions read-only.
8. A `ScopeFactory::build` error with a reason reaches the caller
   unchanged; without one it is `TENANT_UNAVAILABLE`.
9. The cached value carries its `TenantInfo`; the scope type implements no
   trait of this crate.
10. Postgres 16 is the minimum, and the preflight refuses an older server.
11. `SCRAM_ITERATIONS` is the constant 4096; the syntax check pins it.
12. Public timestamps are `SystemTime` (what `Clock` gives).
13. The facade depends on the crate in path form with default features
    off, so `tenancy-manager` compiles without a database driver.
14. The control table names and column names are fixed by the reference
    schema (question 3 asks whether that holds).
15. **The path-label prefix is a reserved namespace on every host**, with
    no fall-through: under it, a host that is not a path-label host, a
    missing or malformed label and an unknown well-known name are
    `TENANT_NOT_HERE`, and a path-label host serves nothing outside it
    (owner's decision; 9.4).
16. **The CORS predicate answers for id-header hosts only** and never
    looks at the path (owner's decision; 9.5).
17. **The loopback path label**: exactly one tenant that is not purged may
    carry the label, and it must have a registered loopback host; the read
    is never cached or joined and spends a lookup permit (owner's
    decision; 9.4).
18. **A caller that joined a read twice** without an answer is
    `TENANT_BUSY` (owner's decision, confirming 9.3).
19. **Found and not-found lookups are separate partitions**; when one is
    full, expired entries are dropped and only that partition is cleared
    (owner's decision; 9.3).
20. **A bypass path gets an empty scope in every form** of `Tenants`; the
    fixed form takes its prefixes through `Tenants::fixed_with_bypass`
    (owner's decision; 9.6).
21. **A resync attempt has no deadline**, and the cross-process guarantee
    is worded conditionally: a lost notification is repaired by the next
    successful resync; the 30 s lookup lifetime bounds only the reuse of a
    cached answer and is no revocation bound (owner's decision; 9.7;
    question 10).
22. The tenant cache's defaults are the framework's `KeyedPools` defaults
    (capacity 64, 5 connections per tenant, 10 min idle eviction, a 30 s
    create deadline); the 30 s create deadline this note fixes is that
    default, and the builder sets none of its own.
23. The tenant cache drops the source of a `ScopeFactory` error and logs
    only a fixed stage text (with at most a SQLSTATE); a product that
    wants its cause logged writes its own event.
24. `shutdown` ends the listener even while a resync's load is awaited,
    since that load has no deadline.
25. The provisioning step is a type (`ProvisionStep`) in the serving half,
    and the reference schema keeps all three tables in one text, so 1c
    changes neither.
26. **A change is handled as forget, reload, retire, tell**: the scope is
    retired after the read that becomes the tenant's baseline, so a scope
    opened in between can never hide a later lost change (9.7).
27. **`get`'s state check is a request's lookup** (admitted, coalesced,
    `TENANT_BUSY` without a free permit), so concurrent cold `get`s of one
    id cause one control read; only the reads inside a creation are
    internal (9.6).
28. **A cold open checks the committed row against its pool**: same
    database, role and role key, else `TENANT_UNAVAILABLE` and nothing
    cached (9.6).
29. **Walks and the sweep tell busy answers apart by provenance**: only the
    cache's full answer and the lookup's busy answer count as `busy`; a
    product refusal is a failure whatever its reason (9.6).
30. **The sweep opens at most two tenants at once and never more than the
    cache's capacity**, so it never refuses itself on a small cache (9.6).
31. **A lookup entry is replaced only by a read that started later**; an
    entry found not fresh is removed, and an answer past its lifetime is
    not stored (9.3).
32. **A path-label prefix of `/` is refused**: the prefix is reserved on
    every host, and `/` would reserve every path (9.4).
33. **After a failed read the listener listens on a new connection** from
    the pool, its sessions built without the driver's eager reconnect, so
    a dead connection is never read again and a loss is never hidden by a
    tick (9.7).
34. **The resync deadline restarts when a resync ends**: the next periodic
    resync is one interval after the previous one ended, so a slow resync
    never starves the notifications; an interval above 100 years never
    ticks (9.7).
35. **The minter carries its issuer**: `Minter::new(issuer, key, clock)`,
    so every token of one operator service names the same `iss` and a call
    site cannot mint under another (10.7).
36. **The default `check_adoptable` refuses** every database
    (`ADOPTION_REFUSED`, `check` = `not_checked`): adoption fails closed
    until the product says which databases are its own. The check runs
    again when a resumed adoption reaches its `database` step, while the
    maintenance role still owns the database (10.1, 10.4).
37. **The admin authenticator's end user names no tenant**: the admin App
    has no scope resolver, and the guard compares the call's tenant with
    the token's `tid` exactly; a nil `tid` is refused. A call without a
    bearer gets the plain constant `UNAUTHENTICATED` of the App's
    `BearerShape` precheck, not `ADMIN_TOKEN_INVALID` (10.6).
38. **The admin client drives `reqwest` directly**, built by
    `sekvent_client::reqwest_builder` with redirects off, so a failure
    before the request left (`NotSent`) is told apart from one after
    (`Unknown`); `CallOutcome<T>` is `Replied`, `NotSent`, `Refused`,
    `Unknown`, and `UNAVAILABLE` without a reason counts as `Unknown`
    (10.7).
39. **Read-only lifecycle pools migrate first**: `usage` and `describe`
    run the migrator on a read-write pool of one connection, then serve
    the hook read-only sessions with a 5 s statement timeout; both answer
    only for `active` or `suspended` tenants (10.3).
40. **Display names are 1 to the provisioner's maximum in bytes**
    (`ProvisionerBuilder::display_name_max_bytes`, default 255, zero
    refused), not blank, without a control character, stored as given (the
    reference schema's column is `text`); provisioning and adoption check
    them with the same setting. A product whose own limit is in characters
    sets the bytes they take in UTF-8. Labels and hosts are canonicalised
    (hosts sorted and de-duplicated) before they are stored or compared
    with a row (10.2).
41. **A maintenance session acts as the role it signed in as**: the
    preflight refuses a maintenance session whose `CURRENT_USER` is not
    its `session_user` (`RoleSwitched`), and every `connect` and
    `connect_to` session is checked the same way when it opens (owner's
    decision; 8.10, 8.6).
42. **The provision lock key is a setting**: `ProvisionerBuilder::
    provision_lock(i64)`, default `PROVISION_LOCK`, so a deployment that
    already orders its inserts under a key of its own keeps it; every
    process of a deployment must use the same key (owner's decision; 10.2).
43. **A warm active tenant lends its cache pool** to `suspend`, `usage` and
    `describe` through a held lease, instead of a second pool of the same
    role; only a cold or non-active tenant opens a lifecycle pool. On a
    warm tenant `usage` and `describe` therefore run on the cache's
    read-write sessions, and decision 39's read-only sessions apply to the
    lifecycle pool only (owner's decision; 10.1, 10.3).
44. **Lifecycle pools count against the tenant cache's capacity**: each
    holds one reserved slot (`KeyedPools::reserve`, an entry in use for the
    eviction and the refusal) for its life; a full cache is `TENANT_BUSY`
    before any connection, an idle warm entry is evicted to make room, and
    a closed cache is `UNAVAILABLE` without a reason. Without a tenant
    cache there is no budget to keep (owner's decision; 10.1).
45. **No public open of a tenant.** With 43 and 44 every lifecycle need
    of a product (seed, adopt, suspend, resume, usage, describe) has a
    hook that receives the pool, so `lifecycle::open` stays crate-private
    behind `LifecycleCapability`: a public open would hand a pool on a
    non-active tenant to code outside the review rule of 10.1 and outside
    the cache's budget. A product that needs more work on a non-active
    tenant does it in a hook (10.1).
46. **Admission tuning is per value, permits are shared**:
    `Registry::with_admission_wait` and `lifecycle_available` (10.5); the
    usage and describe states stay `active` and `suspended` (10.3).
47. **The texts of the reason-less lifecycle answers are stable
    constants**: `messages::LIFECYCLE_BUSY` and
    `messages::TENANT_OPEN_FAILED`; they get no reason, which would change
    the wire (7, 10.5).

## 15. Open questions for the repository owner

1. **Decided: slices land on `master`** (section 5, item 1), not on a
   feature branch.
2. **One crate or two.** Keep the manager kit in `sekvent-tenancy` behind
   a feature, or give it a crate of its own (`sekvent-tenancy-manager`)
   so an operator service never sees the serving code in its dependency
   graph. *Recommended: one crate;* the features already keep sqlx out,
   and ids, key ids and the token format stay in one place.
3. **Control table names.** Fixed (`tenants`, `tenant_hosts`,
   `admin_audit`, found through the pool's `search_path`), or
   configurable. *Recommended: fixed;* a product that needs other names
   puts the tables in a schema of their own. Slice 1b is built on the
   recommendation; the question stays open.
4. **The host selector.** Row 5 of 9.4 (a tenant's own host selects it)
   is not exercised by the first consumer. Build it in 1b behind its
   switch, or specify it now and build it with the product that needs it.
   *Recommended: build it in 1b;* it is one lookup that already exists
   and a table row of tests, and selection rules are the hardest part to
   change once products depend on them. Slice 1b is built on the
   recommendation (`SelectorRules::tenant_hosts`); the question stays
   open.
5. **The manager client's transport.** gRPC-Web over the `sekvent-client`
   HTTP stack (works through HTTP proxies and with TLS today; the product
   enables gRPC-Web on its admin listener), or native gRPC through tonic
   (no gRPC-Web on the admin listener, but the component binding is
   plaintext until TLS lands there). *Recommended: gRPC-Web.*
6. **Message text.** Fixed neutral messages (the table of 7), or a way
   for a product to supply its own per reason. *Recommended: fixed;*
   reasons and codes are the contract, and a product that wants other
   words maps by reason at its edge. Slice 1b is built on the
   recommendation; the question stays open.
7. **The example.** `examples/tenants` as in 12.3 with 1c, or a smaller
   one (serving only) with 1b and the lifecycle added later.
   *Recommended: once, with 1c,* so it is written against final
   signatures.
8. **The coverage floor.** `AGENTS.md` states 95 % per library crate; the
   configured coverage run applies one floor to the whole tree. Accept
   the crate's own figure from the summary as this work's gate, or change
   the run to per-package floors first. *Recommended: the crate's own
   figure now, the run fixed separately.* Slice 1b is measured on the
   recommendation; the question stays open.
9. **The repository rule on origin.** `AGENTS.md` says the framework is
   written from scratch. This crate is specified from a product's working
   behaviour and written anew from this note. Leave the rule as it is, or
   add a sentence that a behaviour may be re-specified in a design note
   and implemented from the note alone. *Recommended: add the sentence,*
   so the next such port has a stated method.
10. **A deadline on a resync attempt.** A resync's load has none for now
    (decision 21 of 14): a control read that hangs postpones the second
    failed resync, and with it the fail-closed emptying of the host
    snapshot, for as long as it hangs. Keep it so, or cut a resync's load
    off after a deadline and count that as a failed resync.
11. **An undecodable active row.** One active row the crate cannot decode
    fails the whole active list today, and with it the walks, the sweep,
    `refresh_hosts` and the listener's loads. Skip such a row with a
    warning, or have the reference schema constrain the stored step (it
    has no check on `provision_step`) so the row cannot be written.
12. **The sender's pending keys.** While a send is stalled the queue keeps
    one entry per distinct tenant and kind and per tenant and topic,
    without a limit. Bound it (and drop what does not fit, as a failed
    batch is dropped), or keep it unbounded.
13. **Retiring without waiting.** The listener takes a scope out of the
    cache with one poll of `Tenants::invalidate`, relying on `sekvent-db`'s
    keyed cache removing the entry before it awaits the disposal. Should
    the keyed cache offer a retire that removes the entry and starts the
    disposal without waiting for it, so this does not rest on an
    implementation detail?
14. **Feature sets in the gate.** The gate runs every command with all
    features, so the code compiled without `admin` (and the default
    feature set) is only checked, never tested, by it. Should the gate also
    run the crate's tests without `admin`?

## 16. Security review

The implementation this note generalises was reviewed; **the rewrite is
not**. Before any product deploys the crate, an independent security
review covers at least: the privilege inspection and the tenant role and
database checks (8.9), the preflight (8.10), role creation and the
verifier path (8.4, 8.7), the database drop and the handover (8.8, 8.11),
name and verifier formatting (8.2, 8.6), selection and the origin check
(9.4, 9.5), the lookup and cold-open bounds (9.3, 9.6), and the admin
token, the replay record and the guard (10.6). Its scope includes a
loopback host listed for both selectors: a path-label selection there
carries no `Origin` check, while the CORS predicate answers for that host
as for an id-header host. The first product
deployment of the crate waits for it.

## 17. Verification

Per slice, on the remote builder, one command at a time:

```sh
~/.kodein/skills/build-on-rtx/bin/rrb run gate
~/.kodein/skills/build-on-rtx/bin/rrb run coverage
```

Then the consumer check of the slice (8.13, 9.8, 10.8) in the consuming
product's own tree. A slice is committed only when the gate and coverage
are green and the crate's own line coverage is at least 95 %; it then
lands on `master`. A product deploys the crate only when all three
consumer checks and the review of section 16 are done.
