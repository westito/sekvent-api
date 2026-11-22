# Tenants on a database each (`sekvent-tenancy`)

A new crate, `sekvent-tenancy`: the layer above the multi-tenant building
blocks that a product compiles in to serve many tenants from one process,
each tenant with a Postgres database and a database role of its own. It
picks the tenant from the request head, opens that tenant's pool with a
derived role password, migrates on first open, caches, follows changes made
on other processes, and carries the lifecycle (provision, suspend, resume,
set hosts, purge, adopt) with an admin surface an operator service calls.

> Status: **draft for approval; nothing is implemented.** Builds on
> [`multi-tenant-building-blocks.md`](multi-tenant-building-blocks.md),
> [`serve-admission.md`](serve-admission.md),
> [`component-end-user.md`](component-end-user.md) and
> [`p8-service-essentials.md`](p8-service-essentials.md). Where this note
> is more specific, it wins. The note is also the work order: three slices
> landing on `master` one by one (section 5), each opened by a step 0 the coordinator
> does alone (section 6). Slice 1a (section 8) is specified in full; 1b and
> 1c (sections 9 and 10) fix behaviour and the shape of the API and mark the
> signatures their own step 0 pins.

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
`sekvent-facade-check` module `src/tenancy.rs` (1b: a `ScopeFactory` and a
`TenantResolver` compiled through the facade alone; 1c: `TenantHooks` and a
guarded method), and in 1c the three example members in the root manifest.
A slice that changes a signature of an earlier slice records it in this
note in place.

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
```

Both are `&'static str` because `EndUserRejection` takes nothing else.

| Reason | Code | Domain | Message | When |
|---|---|---|---|---|
| `TENANT_NOT_HERE` | `NOT_FOUND` | serving | `this tenant is not served here` | unknown, purging or purged tenant; a host that selects nothing |
| `TENANT_PROVISIONING` | `UNAVAILABLE` | serving | `this tenant is being set up` | state `provisioning` |
| `TENANT_SUSPENDED` | `FAILED_PRECONDITION` | serving | `this tenant is suspended` | state `suspended` |
| `TENANT_UNAVAILABLE` | `UNAVAILABLE` | serving | `this tenant is unavailable, try again later` | the tenant's database cannot be opened |
| `TENANT_BUSY` | `UNAVAILABLE` | serving | `the server is busy, try again shortly` | no lookup permit, or the tenant cache is full; retry hint 1 s |
| `TENANT_SELECTOR_MISSING` | `INVALID_ARGUMENT` | serving | `the request names no tenant` | a selecting host without its selector |
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
  this milestone (15, question 6).
- Cluster operations return reasons without a domain (they know no
  product); the guard gives a reasoned error without a domain the admin
  domain (10.6). Serving errors are built with the serving domain by the
  cache and the resolver, which hold the `ErrorDomains`.
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
pub struct Maintenance { /* options, names, a semaphore of one */ }

impl Maintenance {
    pub fn new(options: PgConnectOptions, names: ObjectNames) -> Self;
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
- Application name `sekvent-tenancy-maintenance`.
- Catalog reads are static SQL with bound names. Utility statements are
  built only in `cluster/sql.rs`, only from `TenantRole`, `DatabaseName`
  and `ScramVerifier`.
- A `pool_on` session keeps the lock and statement bounds but runs with
  the database's own `search_path`, which a product's statements need.
  The pool is therefore used only after `handover::prepare` found no
  object of a third role in that database.
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
pub async fn access(conn: &mut MaintenanceConn, database: &DatabaseName) -> Result<Option<DatabaseAccess>, AppError>;
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
}

pub fn protected_facts(owner: String, grants: Vec<DatabaseGrant>, roles: &[ConnectingRole],
                       allowed: &[String], maintenance: Option<&str>, names: &ObjectNames) -> ProtectedFacts;
pub fn findings(facts: &ProtectedFacts, allowed: &[String], names: &ObjectNames) -> Vec<Finding>;
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
| `tests/cluster_databases.rs` | a created database is owned by the role and opens only with its restriction: the tenant role connects, another `LOGIN` role and the maintenance role do not; a create interrupted after `CREATE DATABASE` converges on the next call; an existing database is refused, and unchanged, for another owner, a template, a grant to a third role, a grant to the maintenance role, `CREATE` to `PUBLIC`; `size` without a superuser; `drop` with sessions connected; `drop` refuses a database of another existing role, the maintenance role's, a template; a database of another owner once the role is gone is left alone and answered `Ok`; a failed drop leaves the session as the maintenance role; a poisoned connection refuses every operation |
| `tests/privileges.rs` | chains of two and three edges in both directions; effective `CONNECT` through an own grant, `PUBLIC`, an inherited membership and a superuser; a database without an ACL read as its defaults |
| `tests/preflight.rs` | a protected database passes; each finding of 8.10 alone; the maintenance database is checked on its own connection; one shared database; three hundred tenant-named roles checked by one call |
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

Behaviour is fixed here. Signatures marked † are pinned in the slice's
step 0, after 1a.

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
    pub primary_host: String,
    pub hosts: Vec<String>,        // canonical, sorted
    pub role_key: RoleKey,
}
impl TenantInfo {
    pub fn local() -> Self;                 // the fixed scope's: nil id, active
    pub fn origin(&self) -> Option<String>; // `origin_of_host(primary_host)`
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Selector {
    Id(TenantId),
    Host(String),
    PathLabel { segment: String, listener_host: String },
}
```

The reference schema, `control::REFERENCE_SCHEMA` (DDL text the product
copies into a migration of its own):

| Table | Columns | Named constraints the crate relies on |
|---|---|---|
| `tenants` | `id uuid`, `subdomain text`, `display_name text`, `state text`, `provision_step text default ''`, `db_cluster text default 'main'`, `db_name text`, `db_role text`, `db_role_key_id text`, `db_role_generation integer default 1`, `schema_version bigint null`, `created_at timestamptz`, `updated_at timestamptz` | `tenants_pkey (id)`; `tenants_db_role_key` unique `(db_cluster, db_role)`; `tenants_db_name_key` unique `(db_cluster, db_name)` where `state <> 'purged'`; checks on `state` (the five values) and `db_role_generation >= 1` |
| `tenant_hosts` | `host text`, `tenant_id uuid`, `is_primary boolean default false` | `tenant_hosts_pkey (host)`; a foreign key to `tenants` with `ON DELETE CASCADE`; one primary per tenant (partial unique index) |
| `admin_audit` | `token_id uuid`, `occurred_at timestamptz`, `operation text`, `actor text`, `outcome text default 'started'`, `tenant_id uuid null` | `admin_audit_pkey (token_id)`: the replay refusal of 10.6 |

- `control::verify_schema(&PgPool)`† selects every column the crate uses
  from each table with a false predicate (the method of
  `LeaseStore::verify_schema`) and reads from the catalog that the named
  unique constraints and indexes exist, since error mapping goes by
  constraint name. A gap is `FAILED_PRECONDITION` / `CONTROL_SCHEMA_MISSING`.
- **Extra columns are the product's.** Every statement of the crate names
  its columns; an extra column on `tenants` must be nullable or have a
  default. The product reads and writes its columns with statements of its
  own, each holding a control connection for that one statement.
- `db_cluster` holds the one value `main`; `admin_audit` exists only for a
  product that enables `admin`; verification checks it only then.
- Statements are static SQL with bound values. Repository functions take
  `&mut PgConnection` and never commit.
- The control pool has at least 3 connections; `Registry::new`† refuses a
  smaller one (the two admissions below each leave room for the other).

### 9.3 Lookups and their admission

`Registry`† holds the control pool, a `Clock`, the lookup cache and the two
admissions.

| | Value |
|---|---|
| Lookup cache | two key spaces (id, exact host); 4,096 found and 1,024 not-found entries each; 30 s from the start of the read; errors are never cached; no stale answer is ever served |
| When a space is full | expired entries are dropped, then the space is cleared |
| `forget(tenant)` | drops the tenant's entries and every not-found host entry, and bumps an epoch so a read already running does not store its answer |
| **Lookup admission** | `max(1, control max_connections / 2)` permits for selector cache misses |
| No permit | `TENANT_BUSY` at once, **before** any control connection is taken |
| Same key in flight | the caller joins that read, then looks again; a caller that joined twice without an answer is `TENANT_BUSY` |
| A caller that goes away | the read runs on a task of its own, keeps its permit and fills the cache; nothing is cancelled and no slot is freed early |
| Internal reads by id (lifecycle, the connector) | share the cache, take no permit |
| A control connection | is held for a single statement and returned before a tenant is opened; none spans a tenant open |

This is the crate's bound inside the scope resolver. The App's
`ServeAdmission` is in front of it and unchanged: it bounds how many
unauthenticated calls are pending at all; the lookup admission bounds how
much of the control pool they can take, so lifecycle calls and jobs keep
theirs.

### 9.4 Selection and the resolver

```rust
/// Which request names which tenant. Built by the product from its own keys.
pub struct SelectorRules { /* private */ }
impl SelectorRules {
    pub fn new() -> Self;
    /// Requests to these hosts name the tenant by id in `header`.
    pub fn id_header(self, header: http::HeaderName, hosts: impl IntoIterator<Item = String>) -> Result<Self, AppError>;
    /// Requests to these hosts under `prefix` name the tenant by a path label.
    pub fn path_label(self, prefix: &str, hosts: impl IntoIterator<Item = String>) -> Result<Self, AppError>;
    /// Any other host is looked up as a tenant's own host.
    pub fn tenant_hosts(self, enabled: bool) -> Self;
    /// RPC path prefixes that belong to no tenant: an empty scope, no selection.
    pub fn bypass(self, prefixes: &'static [&'static str]) -> Self;
}
```

(† `SelectorRules` is pinned in step 0.)

The request host is the canonical form of the URI authority and of the one
`Host` header; when both are present they must agree. Forwarded-host
headers are never read. Absent, malformed, repeated or disagreeing:
`TENANT_NOT_HERE`.

| Order | Condition | Selector | Further checks |
|---|---|---|---|
| 1 | the path starts with a bypass prefix | none; the scope is empty | |
| 2 | the host is a path-label host and the path is under the prefix | `PathLabel` | the id header is optional; when present it must name the tenant found (`TENANT_MISMATCH`) |
| 3 | the host is an id-header host | `Id` from the header: exactly one value, a `TenantId::parse` text (`TENANT_SELECTOR_MISSING` when absent, `TENANT_MISMATCH` when malformed or repeated) | an `Origin`, when present, must be a host of the selected tenant (`ORIGIN_NOT_ALLOWED`); a request without `Origin` passes |
| 4 | `tenant_hosts` is on | `Host` | the id header as in row 2 |
| 5 | otherwise | | `TENANT_NOT_HERE` |

The path label is the first segment after the prefix and stands for the
host `path_label_host` derives from it (on a loopback listener, for the
one tenant of that label that has a loopback host). A product may list
well-known names so that `<prefix>.well-known/<name>/<label>` selects the
same tenant.

A host may be in both lists only on loopback. The tenant's existence and
state are checked before a header comparison, so a mismatch tells a caller
nothing about a tenant it cannot name. The path is never percent-decoded.

```rust
/// What the scope holds for a streaming method instead of a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTenant(pub TenantId);

pub struct TenantResolver<S> { /* private */ }
impl<S: Send + Sync + 'static> TenantResolver<S> {
    /// `streaming`: RPC paths (`<package>.<Service>/<Rpc>`) that get the id only.
    pub fn new(tenants: Tenants<S>, streaming: &'static [&'static str]) -> Self;
}
impl<S: Send + Sync + 'static> ScopeResolver for TenantResolver<S>;
```

The scope's tenant is `TenantId`'s text, so `cx.tenant()` is the id and an
end user of another tenant is refused by the framework. Its extension is a
`TenantLease<S>` (the handler reads `cx.scoped::<TenantLease<S>>()`), or a
`StreamTenant` for the listed paths. **The id-only variant** exists
because a served stream keeps its scope until it ends (up to the stream
lifetime): a lease in it would keep the tenant's pool in use, and a
suspended tenant's stream alive, for that long. A stream takes
`tenants.get(id)` for each read and gives it back. The resolver logs
nothing and opens no span.

### 9.5 The host snapshot and CORS

`HostSnapshot`†: every host of every active tenant in memory, replaced as a
whole. `hosts::predicate(snapshot, rules)`† is the function for
`Cors::origin_predicate`: synchronous, no I/O. It allows a request when its
host is a selecting host of the rules, it has exactly one `Origin`, and
that origin's canonical host is in the snapshot. Whether that origin may
call *the selected tenant* is decided again at resolution (row 3 above),
because CORS only shapes response headers. Methods, allowed and exposed
headers and the max age are the product's `Cors` settings.

`refresh_hosts`† reads the active hosts with one statement, returns the
connection, then swaps the map; on failure the old snapshot stays. Reloads
are serialised.

### 9.6 The tenant cache

```rust
pub type ScopeFuture<'a, S> = Pin<Box<dyn Future<Output = Result<S, AppError>> + Send + 'a>>;

/// Builds a tenant's scope on its migrated pool. May take seconds; may fail.
pub trait ScopeFactory: Send + Sync + 'static {
    type Scope: Send + Sync + 'static;
    fn build<'a>(&'a self, info: &'a TenantInfo, pool: PgPool) -> ScopeFuture<'a, Self::Scope>;
}

/// The tenants this process serves: one fixed scope, or a cache of scopes.
pub struct Tenants<S> { /* private; clones share */ }

/// A tenant's scope, held for as long as it is used.
pub struct TenantLease<S> { /* private */ }
impl<S> Clone for TenantLease<S>;
impl<S> std::ops::Deref for TenantLease<S> { type Target = S; }
impl<S> TenantLease<S> { pub fn id(&self) -> TenantId; pub fn info(&self) -> &TenantInfo; }
```

`Tenants<S>`†: `fixed(scope)`; a builder for the keyed form over a
`Registry`, a `Connector`, `ObjectNames`, `SelectorRules`, `ErrorDomains`,
the tenant schema's `&'static sqlx::migrate::Migrator` and a
`ScopeFactory`; `select(&Parts) -> TenantId` (never opens), `resolve`,
`get(TenantId)`, `peek`, `invalidate`, `evict_idle_older_than`,
`for_each_active`, `migration_sweep`, `stats`, `hosts`, `registry`,
`into_unit` (feature `runtime`).

The keyed form is a `KeyedPools<TenantId, _>`: its connector reads the
committed row (through the lookup cache, no permit) and returns
`Connector::candidates`; its `init` runs the tenant migrator **as the
tenant's role**; its `build_value` rereads the row and calls the factory.
The cached value carries the `TenantInfo` it was built from, so the scope
type needs no trait.

| State (the row's, and the built value's) | `get` and `select` |
|---|---|
| `active` | served |
| `provisioning` | `TENANT_PROVISIONING` |
| `suspended` | `TENANT_SUSPENDED` |
| `purging`, `purged`, no row | `TENANT_NOT_HERE` |

| A creation ends with | The caller gets |
|---|---|
| a serving reason of 7 (the row changed under it) | that error |
| `KEYED_CAPACITY` | `TENANT_BUSY` |
| an error of `ScopeFactory::build` that carries a reason | that error, unchanged: the product's refusal (the single-owner seam) |
| a panic | `INTERNAL` |
| anything else: connect, rejected credentials, a failed migration, the create deadline, a closed cache, a build error without a reason | `TENANT_UNAVAILABLE`; the cause is in the creation's log event |

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
- `invalidate` retires a warm entry or a creation; holders keep their
  lease until they drop it; a retired creation runs to its end and its
  value is discarded.
- `peek` never opens and does not count as a use.

`Tenants::fixed(scope)` serves `TenantId::LOCAL` alone through the same
calls: `select` ignores the request, `get` of any other id is
`TENANT_NOT_HERE`, walks visit the one scope, there is no registry and no
snapshot.

**Walks.** `for_each_active(f)`† visits the active tenants one after
another with a lease each; a tenant answered `TENANT_BUSY` is counted and
skipped, any other failure is logged and recorded, and only a failed list
read fails the walk (`WalkReport { visited, busy, failed }`).
**`migration_sweep()`**† opens every active tenant (which migrates it),
two at a time, drops the lease, then stores the schema version with
`FOR UPDATE SKIP LOCKED`: it never waits on a lifecycle call's row lock. A
skipped row is retried once after the others, then counted `busy`. A cold
open itself writes no version.

Capacity against the server: `capacity × max_connections`, plus headroom
for retiring values, plus the lifecycle pools of 10.1, stays below the
cluster's limit; the role's connection limit covers every process that
serves the tenant.

### 9.7 The invalidation channel

One NOTIFY channel on the control database; the product names it
(`Channel::new`†, the grammar of `DatabaseName`).

| Message | Payload | Meaning |
|---|---|---|
| change | `t:<tenant>:<kind>` | the tenant's control row or hosts changed. Kinds `state` and `hosts` are the crate's; a product adds kinds of its own, `^[a-z][a-z0-9_]{0,15}$` |
| hint | `e:<tenant>:<topic>:<seq>` | product data of the tenant moved; `topic` is `^[A-Za-z0-9_-]{1,64}$`, `seq` a non-negative `i64` |

`Message::parse` accepts exactly these shapes; anything else is ignored at
`debug`.

**The sender**† (`Sender::new(control, channel)`, `change(tenant, kind)`,
`hint(tenant, topic, seq)`): both calls are synchronous and **never make
the caller wait**. They queue under a short mutex; at most one statement
is in flight and it carries the whole queue (`pg_notify` with bound
values); the highest `seq` per tenant and topic wins, a change is kept
once per tenant and kind and goes out before hints. A failed acquire or
statement drops the batch with one warning: the resync below is the
guarantee, a notification is the fast path.

**The listener**† (`listen(tenants, channel, sink, ready, shutdown)`), a
`PgListener` on the control pool:

| Event | Built in | Then |
|---|---|---|
| a change | forget the tenant's lookups, invalidate its scope, reload the host snapshot | `sink.change(tenant, kind)` |
| a hint | nothing | `sink.hint(tenant, topic, seq)` |
| every 60 s | clear every lookup, reload the snapshot, compare the active tenants with the last load, invalidate each one that changed, joined or left | `sink.resync(&changed)` |
| a reconnect | as the 60 s resync | `sink.reconnected()`: the product treats every waiter as stale |

The first `LISTEN` and the first load must succeed before `ready` is
called; a later failure retries after 1 s. A second consecutive failed
resync empties the snapshot: an origin is then refused rather than
allowed from stale data. `Sink`† is the product's (waking its streams);
a product without one passes `()`.

The guarantee across processes is therefore: one notification in the
normal case, at most 60 s otherwise, plus the 30 s lookup cache where no
message arrived.

### 9.8 Tests

`tests/common` gains the control fixture and the test product (11.1).
Unit: selector table of 9.4 row by row; message codec round trips and
refusals; the sender's coalescing on a paused clock with a gated fake
drain; lookup cache expiry, capacity and the epoch on a `ManualClock`.
Docker: registry reads and uniqueness mapped to `TENANT_STATE`;
`verify_schema` with a column and a constraint missing and with an extra
column; lookup admission (permits exhausted is `TENANT_BUSY` with no
control connection taken, observed on `pg_stat_activity`; joiners; a
cancelled caller's read still fills the cache); the state table of 9.6;
twenty cold `get`s migrate and build once; a gated slow build; a build
refusal with a reason reaches the caller; a cancelled waiter neither
cancels the creation nor frees its slot; `invalidate` under a held lease;
the origin check; the snapshot and predicate; a change sent on one
`Tenants` drops the scope on another; the 60 s resync on a paused
interval seam; walks and the sweep, with a row locked by the test.

**Consumer check.** The product's serving path runs on `Tenants`,
`TenantResolver`, the snapshot and the channel; its isolation suite (no
tenant reaches another's data, a suspended tenant stops being served on
every process) passes unchanged.

## 10. Slice 1c: lifecycle and admin

### 10.1 File ownership

| Writer | Files | Module-page section |
|---|---|---|
| A | `src/lifecycle/hooks.rs`, `provisioner.rs`, `open.rs`, `provision.rs`, `tests/provision.rs` | "Provision a tenant" |
| B | `src/lifecycle/state.rs`, `purge.rs`, `announce.rs`, `usage.rs`, `tests/lifecycle.rs`, `tests/purge.rs` | "Suspend, resume, change hosts, purge" |
| C | `src/lifecycle/adopt.rs`, `tests/adopt.rs` | "Adopt an existing database" |
| D | `src/admin/*.rs` except `mod.rs`, `tests/admin.rs` | "The admin surface" |
| E | `src/manager/*.rs` except `mod.rs`, `tests/manager.rs` | "Call a product's admin API" |

`examples/tenants` and the facade check follow in a second wave by one
writer, once the five are verified.

#### Hooks, the capability and the handle

```rust
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AppError>> + Send + 'a>>;

/// The product's side of the lifecycle. Every method has a default that does nothing.
pub trait TenantHooks: Send + Sync + 'static {
    /// The product's seed payload.
    type Seed: Send + Sync + 'static;
    /// Fill a new tenant's database. Must be idempotent: a second call
    /// after a committed first returns no handover.
    fn seed<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool, seed: &'a Self::Seed) -> HookFuture<'a, SeedOutcome>;
    /// Whether a candidate database is the product's and may be adopted.
    /// Read-only; `pool` is the maintenance role's, before the handover.
    fn check_adoptable<'a>(&'a self, candidate: &'a AdoptCandidate, pool: &'a PgPool) -> HookFuture<'a, ()>;
    fn adopted<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()>;
    fn before_suspend<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()>;
    fn before_resume<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()>;
    fn describe<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, Description>;
    fn usage<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, UsageCounters>;
}

/// What a seed hands back once.
pub struct SeedOutcome { pub handover: Option<Secret> }
```

(† the hook signatures are pinned in step 0; the shape above is binding:
async, fallible, a pool and the tenant's info, a `Secret` for a handover.)

- **Hooks get a pool, never a scope.** Every hook but `check_adoptable`
  receives a pool on the tenant's **migrated** database, opened as the
  tenant's role for this one operation (at most two connections, closed
  when the operation ends) and outside the tenant cache: a provisioning
  or suspended tenant never gets a cached scope, and an expensive scope is
  never built to seed, suspend or measure.
- `check_adoptable` is the exception and is named as such: its pool is
  `Maintenance::pool_on` on the candidate database, before the handover
  and before any migration of ours; its sessions start read-only and keep
  the server-side bounds of 8.6.
- **`LifecycleCapability`**†: opening a tenant that is not `active`
  requires one. Its constructor is crate-private, and the review rule is
  that it is called only in `provision.rs`, `state.rs`, `adopt.rs` and
  `usage.rs`; `select`, `resolver`, `tenants`, `purge.rs` and `admin`
  never call it. A purge never opens the tenant.
- `Provisioner<H>`† is the handle: the registry, the `Maintenance`, the
  `Connector` and `ObjectNames`, the tenant migrator, the hooks, the key
  id for new tenants, the role connection limit, the tenant cap, the
  `LabelRules`, the invalidation `Sender`, a `Clock`, and the cache to
  invalidate as a trait object (it is not generic over the scope type).

### 10.2 Provisioning

`provision(&self, ProvisionRequest<H::Seed>) -> Result<Provisioned, AppError>`†,
with `Provisioned { row: TenantRow, handover: Option<Secret> }`.

Before the steps: the request is validated (the label by the product's
`LabelRules`; hosts canonical, sorted, without duplicates, the primary
among them; violations as field violations); the lifecycle admission is
taken; under a transaction-level advisory lock the row is inserted when
the id is new, and a new id at the tenant cap (rows not `purged`) is
`TENANT_LIMIT_REACHED`. A host, role or database name another tenant
holds is `TENANT_STATE`.

Each step locks the row (`FOR UPDATE`), re-reads it, does its work,
records the step and commits. A call for an existing id resumes at the
recorded step when the request matches the row (label, display name,
hosts, primary host), and is `TENANT_STATE` when it does not.

| Step recorded | Connection | Work |
|---|---|---|
| `role` | maintenance; control | derive the password under the configured key id, generation 1; `ScramVerifier::new` on the blocking pool; `roles::create`; store the key id and generation |
| `database` | maintenance | `databases::create` |
| `migrated` | a lifecycle pool as the tenant | the tenant migrator; store the schema version |
| `seeded` | the same pool | `TenantHooks::seed` |
| `active` | control | step and state together; then announce (10.5) |

- **The handover secret is returned once.** A call that resumes after the
  seed committed returns `handover: None`; the product's admin reply says
  the secret is withheld rather than inventing one.
- A call for an id that is already `active` and matches returns the row
  and announces again; it never re-seeds.
- A refusal can follow committed work: steps are resumable, not one
  transaction.

### 10.3 Suspend, resume, set hosts, purge, usage

All take the lifecycle admission first, then the row lock, and decide from
the locked row. A missing row is `TENANT_NOT_FOUND`; a state the table
does not list is `TENANT_STATE`.

| Call | From | To | Before the row changes |
|---|---|---|---|
| `suspend` | `active` | `suspended` | `before_suspend` on a lifecycle pool. Strict: a tenant whose database cannot be opened stays `active` |
| `resume` | `suspended` | `active` | `before_resume` on a lifecycle pool, same strictness |
| `set_hosts` | `active`, `suspended` | unchanged | validation after the lock; the host set is replaced whole |
| `purge` | `suspended`, `purging` | `purging`, then `purged` | nothing of the product runs |

A call that finds the target state already set changes nothing and
announces again. **Purge**: mark `purging`, commit, announce; lock again;
the stored role must equal the derived one (`INTERNAL` otherwise); on the
maintenance connection `databases::drop(database, role)` then
`roles::drop(role)`; clear the hosts, commit `purged` (the row stays as a
tombstone, its database name free again), announce. A failure after the
first commit resumes at `purging`. `active` and `provisioning` cannot be
purged.

`usage`† returns the database's size (`databases::size`) and the
product's `UsageCounters` (named `i64` counters and a last-activity time)
read on a lifecycle pool in a read-only transaction with a 5 s statement
timeout. `describe`† returns the product's `Description` (a display name
and string attributes) for an operator's view. Neither takes the lifecycle
admission.

### 10.4 Adopt

`adopt(&self, AdoptRequest) -> Result<TenantRow, AppError>`†: an existing
database becomes a tenant's. The request names the database
(`is_adoptable_name`, not a tenant object name, not the control or the
maintenance database).

1. Validation, then the lifecycle admission.
2. Without a row yet: `handover::prepare`, then `check_adoptable` on the
   maintenance pool. A refusal leaves **no control row and no role**
   (only `PUBLIC`'s `CONNECT`, 8.11).
3. The row is inserted; the steps of 10.2 run with the `database` step
   replaced by `handover::adopt`, and the `seeded` step by
   `TenantHooks::adopted`. No handover secret exists.

### 10.5 The lifecycle admission and announcing

- **Admission first, then the row lock.** Every lifecycle call takes
  `Registry::admit()` before its first control connection and holds it to
  its end: one semaphore of `max(1, (control max_connections − 1) / 2)`
  permits, a wait of 10 s, then `UNAVAILABLE` without a reason. A
  lifecycle call holds a control connection while it waits on the
  maintenance connection or a tenant open; the admission keeps such calls
  from holding the whole pool.
- **Brief holders never wait while holding.** Every other holder of a
  control connection (lookups, the sender, the sweep, a product's jobs)
  holds it for one statement.
- **Announcing a committed change** runs in a spawned task the call
  awaits, so a caller that goes away still announces, in this order:
  forget the tenant's lookups; invalidate its scope; `Sink::change` on
  this process; `Sender::change` for the others; reload this process's
  host snapshot. A failed reload is a warning, never the call's error.

### 10.6 The admin surface (feature `admin`)

The product declares its admin service in its own proto, serves it from a
second App (`App::register_as`) on a listener of its own, registers the
authenticator below as that App's one end-user authenticator, and wraps
each RPC body in the guard.

```rust
/// What a verified admin token says. `X` is the product's extra claims.
#[derive(Clone, Debug)]
pub struct AdminClaims<X = NoExtra> {
    pub token_id: Uuid,
    pub subject: String,
    pub tenant: Option<TenantId>,
    pub op: String,
    pub expires_at: SystemTime,
    pub extra: X,
}
```

`Verifier<X>`†: `new(issuer, audience, keys)`, `with_subject_check(f)`,
`refusing()` (accepts nothing: a process that serves a fixed scope),
`parse_keys(text)` for a list `key_id:<base64url Ed25519 public key>[,…]`,
and `verify(&Secret, op, now)`.

| Claim | Rule |
|---|---|
| header | EdDSA; `kid` required and known (`EdDsaVerificationKeys`) |
| `iss`, `aud` | equal the verifier's, each one string |
| `iat`, `exp` | `iat <= now < exp`; `exp − iat <= 60 s`; leeway 0 |
| `nbf` | optional; `<= now` |
| `sub` | a non-empty string that passes the product's subject check |
| `jti` | required; a lower-case hyphenated UUID: the token id |
| `tid` | a `TenantId` text; absent only for a call that addresses no tenant |
| `op` | the RPC path without its leading slash, `<package>.<Service>/<Rpc>`; equal to the path called |
| extra | the product's type, flattened |

An optional claim given as `null` is a refusal, not an absence. **Every
failure, whatever the cause, is the one answer** `UNAUTHENTICATED` /
`ADMIN_TOKEN_INVALID`; the cause is logged at `debug` without the token.

`admin::authenticator(verifier, services, clock)`† is an
`EndUserAuthenticator`: the routed path must belong to one of `services`;
the token is `BearerShape::token`; the claims become an extension of the
`EndUser` whose subject is `sub`. Its rejection is an `EndUserRejection`
with the reason and the admin domain.

`guard::run(audit, cx, op, tenant, body)`†, in this order:

1. The claims from the context; none is `ADMIN_TOKEN_INVALID`.
2. `claims.op == op`, else `ADMIN_TOKEN_INVALID`.
3. `claims.tenant == tenant` exactly (`None` only for a call that
   addresses no tenant), else `ADMIN_TOKEN_INVALID`.
4. **The token id is recorded before the body runs**: a row in
   `admin_audit` with the time, operation, tenant and subject, outcome
   `started`. A second insert of the same id violates `admin_audit_pkey`
   and is `ADMIN_TOKEN_REPLAYED`; the body does not run.
5. The body, with the claims. It never checks the token again.
6. The outcome is stored (`ok` or `failed`); a failure to store it is a
   warning; a cancelled call leaves `started`.
7. An error with a reason and no domain gets the admin domain.

Which subjects an operation accepts stays the operation's rule. Pruning
`admin_audit` is the product's, never below the token lifetime.

### 10.7 The manager kit (feature `manager`)

`Minter`†: `new(key: EdDsaSigningKey, clock)`;
`mint(target, tenant, op, subject, extra) -> Minted { token: Secret, token_id, expires_at }`;
`public_key_entry()` in the list format `Verifier::parse_keys` reads. A
token lives 60 s, has a fresh random id, and names one operation and at
most one tenant; absent optional claims are omitted, never `null`.

`AdminClient`†: one unary call to a product's admin RPC by its path,
`call(op, tenant, subject, extra, deadline, &request)`, with request and
reply as `prost` messages. A fresh token per attempt; **no automatic
retry**, no redirects.

| `CallOutcome` | When | What the caller may assume |
|---|---|---|
| `NotSent` | encoding or minting failed, or no connection was made | nothing happened; safe to try again |
| `Refused` | the product answered a status other than OK | the product decided; not proof that nothing changed (a step may have committed before the refusal) |
| `Unknown` | a failure after sending, a malformed reply, the deadline, or `UNAVAILABLE` without a reason | anything; read the tenant back, and repeat only an operation that converges |

A refusal carries the product's reason, domain and field violations.
Transport: gRPC-Web over the `sekvent-client` HTTP stack (15, question 5);
a URL that leaves the cluster must be `https`, and the certificate is
verified.

### 10.8 Tests

Docker, on the fixture of 11.1: provisioning end to end and resumed after
a failure injected at each step (the seed is committed once and its secret
returned once); the cap; a mismatching repeat; the state table of 10.3
with idempotent repeats; a strict suspend with the tenant's database
closed; a purge resumed after each step, one that refuses a database its
role does not own, one that leaves a reused name alone; adoption with each
refusal leaving no row and no role; the lifecycle admission (permits
exhausted waits, then `UNAVAILABLE`, on a paused wait seam; a blocked call
observed on `pg_stat_activity`); every committed change invalidates this
process before the call returns and reaches a second `Tenants`; a dropped
call still announces. Admin, without a server: a verifier table with one
row per rule above, each failure the one answer; `null` claims; two keys
during a rotation. With a server: the guard's order (a wrong `op` or
tenant writes no audit row; a replay does not run the body; the outcome is
stored); an admin App served on `127.0.0.1:0` and called by `AdminClient`
with each `CallOutcome` (a listener at `127.0.0.1:1` for `NotSent`, a
server that closes after reading for `Unknown`).

**Consumer check.** The product's lifecycle and its admin RPC bodies run
on `Provisioner` and the guard, with its wire contracts untouched; its
full suite passes, and an operator service built on the manager kit
provisions, suspends, resumes and purges a tenant against it.

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
- **The test product** (from 1b): an embedded tenant migrator of two
  migrations (an `orders` table, then one more column, so a sweep has a
  version to move); a scope type whose async build can be gated and failed
  by the test and whose `Drop` signals; hooks whose seed inserts one row
  and returns a `Secret` on its first call and nothing on a second, and
  whose usage counter is the number of rows in `orders`.
- Time is a `ManualClock` wherever the crate takes a `Clock`; blocking is
  observed on `pg_stat_activity` with `await_until!`, never slept for;
  anything with a socket binds `127.0.0.1:0`.
- Nothing is dropped per test; the container is reaped with the process.

### 11.2 Coverage

95 % of the crate's own lines, measured with all four features on and the
Docker tests included (the coverage run sets `SEKVENT_DOCKER_TESTS=1` and
passes `--include-ignored`). The configured run applies one floor to the
whole repository, so the crate's own figure is read from the summary and
is the acceptance (15, question 8). Pure rules (`findings`, the drop
decision, the selector table, the message codec, the claim rules) are
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

Lands with 1c. A neutral product, `billing`, in three members added to the
root manifest: `billing-api` (its proto: a public `Invoices` service and
its own `TenantAdmin` service), `billing` (the scope, the factory, the
hooks, the tenant migrations, the control migration made from
`REFERENCE_SCHEMA`), `billing-svc` (two Apps on one runtime, the units,
its own configuration keys, hosts under `example.com`, the prefix
`app_t_`). Its `sekvent.toml` carries `[contract]` roots and baselines for
the example's own services, emitted on the Mac. One Docker test provisions
a tenant through the manager kit and calls it. Examples are outside
coverage.

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
   puts the tables in a schema of their own.
4. **The host selector.** Row 4 of 9.4 (a tenant's own host selects it)
   is not exercised by the first consumer. Build it in 1b behind its
   switch, or specify it now and build it with the product that needs it.
   *Recommended: build it in 1b;* it is one lookup that already exists
   and a table row of tests, and selection rules are the hardest part to
   change once products depend on them.
5. **The manager client's transport.** gRPC-Web over the `sekvent-client`
   HTTP stack (works through HTTP proxies and with TLS today; the product
   enables gRPC-Web on its admin listener), or native gRPC through tonic
   (no gRPC-Web on the admin listener, but the component binding is
   plaintext until TLS lands there). *Recommended: gRPC-Web.*
6. **Message text.** Fixed neutral messages (the table of 7), or a way
   for a product to supply its own per reason. *Recommended: fixed;*
   reasons and codes are the contract, and a product that wants other
   words maps by reason at its edge.
7. **The example.** `examples/tenants` as in 12.3 with 1c, or a smaller
   one (serving only) with 1b and the lifecycle added later.
   *Recommended: once, with 1c,* so it is written against final
   signatures.
8. **The coverage floor.** `AGENTS.md` states 95 % per library crate; the
   configured coverage run applies one floor to the whole tree. Accept
   the crate's own figure from the summary as this work's gate, or change
   the run to per-package floors first. *Recommended: the crate's own
   figure now, the run fixed separately.*
9. **The repository rule on origin.** `AGENTS.md` says the framework is
   written from scratch. This crate is specified from a product's working
   behaviour and written anew from this note. Leave the rule as it is, or
   add a sentence that a behaviour may be re-specified in a design note
   and implemented from the note alone. *Recommended: add the sentence,*
   so the next such port has a stated method.

## 16. Security review

The implementation this note generalises was reviewed; **the rewrite is
not**. Before any product deploys the crate, an independent security
review covers at least: the privilege inspection and the tenant role and
database checks (8.9), the preflight (8.10), role creation and the
verifier path (8.4, 8.7), the database drop and the handover (8.8, 8.11),
name and verifier formatting (8.2, 8.6), selection and the origin check
(9.4, 9.5), the lookup and cold-open bounds (9.3, 9.6), and the admin
token, the replay record and the guard (10.6). The first product
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
