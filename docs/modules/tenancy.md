# Tenants on a database each (`sekvent::tenancy`)

`sekvent::tenancy` is for a product that serves many tenants from one
process and gives each tenant a Postgres database and a database role of its
own. It names those roles and databases so they can be put into DDL safely,
derives each role's password from a master secret instead of storing it,
creates, checks and drops roles and databases through a maintenance role
that is not a superuser, hands an existing database over to a tenant role,
and checks at startup that no tenant role can reach a database that is not
its own, the service's own databases included.

On the request path it keeps one row per tenant in a control database the
product owns, picks the tenant a request names from the request head before
the caller is authenticated, opens and caches each tenant's pool and scope,
and follows the changes other processes make through a Postgres `NOTIFY`
channel. Provisioning, suspend, resume, purge, the admin surface and the
manager kit are not built yet.

The crate reads no configuration keys and runs on Postgres 16 or later.

## Enable it

| | |
|---|---|
| Facade feature | `tenancy`; `tenancy-lifecycle` for the maintenance connection, roles, databases and the handover; `tenancy-admin` and `tenancy-manager` (not built yet) |
| Module | `use sekvent::tenancy::…;` |
| Internal crate | `sekvent-tenancy` (features `serving` (default), `lifecycle`, `admin`, `manager`, `runtime`) |
| Test container | `sekvent-testing` feature `postgres` ([testing](testing.md)) |

| Facade feature | Crate feature | Adds |
|---|---|---|
| `tenancy` | `serving` | Tenant ids, object names, hosts and labels, role secrets, the connector, privilege inspection and the startup preflight; the control schema and the `Registry`, `SelectorRules`, the `TenantResolver`, the host snapshot and its CORS predicate, the tenant cache `Tenants`, the invalidation channel |
| `tenancy-lifecycle` | `lifecycle` (with `serving`) | The maintenance connection, roles, databases, the SCRAM verifier, the ownership handover |
| `tenancy-admin` | `admin` (with `serving`) | Nothing of the admin surface is built yet; with it on, `control::verify_schema` also requires the `admin_audit` table and its key `admin_audit_pkey`. The facade's `full` feature turns it on |
| `tenancy-manager` | `manager` | Not built yet; needs no database driver |
| `runtime` (default) | `runtime` (with `serving`) | `Tenants::into_unit` and `invalidation::Listener::into_unit`: the tenant cache and the invalidation listener as runtime units |

`tenancy-manager` keeps sqlx and `sekvent-db` out of the build only when the
facade is used with `default-features = false`: the facade's default
`runtime` feature forwards to the crate's `runtime` feature, which turns on
`serving`.

The snippets on this page also use `http` (`HeaderName`) and `sqlx` (`PgPool`,
`sqlx::migrate!`, queries) directly; the facade re-exports neither, so the
application lists both, `sqlx` at the version sekvent pins:

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["tenancy-lifecycle"] }
http = "1"
sqlx = { version = "0.9", default-features = false, features = ["postgres", "runtime-tokio", "macros", "migrate"] }

[dev-dependencies]
sekvent-testing = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["postgres"] }
```

## What you bring

The crate is a set of parsers, builders and caches; the product supplies
every value from its own configuration and owns every database:

- **A Postgres 16 or later cluster** with a maintenance role that is not a
  superuser (see [The maintenance connection](#the-maintenance-connection)),
  and per tenant a role and a database made with
  [Roles and databases](#roles-and-databases).
- **A control database** whose schema the product migrates itself from
  `control::REFERENCE_SCHEMA`, and a pool on it of at least 3 connections
  ([The control schema](#the-control-schema)). The product records each
  tenant's row with `control::rows` and sets it `active` once its role and
  database exist.
- **The object-name prefix** (`app_t_` below), **the master secrets** with
  their context string, and **base connect options** for the tenant server
  ([Role credentials](#role-credentials)).
- **A tenant schema** as a sqlx `Migrator`, a **scope type** (what a request
  to a tenant gets: its pool and whatever the product builds on it) and a
  **`ScopeFactory`** that builds it ([The tenant cache](#the-tenant-cache)).
- **Selector rules**: which hosts name the tenant by an id header, which by
  a path label, whether tenants' own hosts select, and which RPC paths
  belong to no tenant ([Select the tenant](#select-the-tenant)).
- **A CORS policy** for the methods and headers, and **a channel name** for
  the invalidation channel ([CORS](#cors),
  [Follow changes from other processes](#follow-changes-from-other-processes)).

A single-tenant deployment needs none of this: `Tenants::fixed(scope)`
serves one scope through the same calls ([The two forms](#the-two-forms)).

## Quick example

A product `billing` serves its tenants' own hosts under `example.com`, an
API host that names the tenant by a header, and a link host that names it
by a path label:

```rust
use std::sync::Arc;

use http::HeaderName;
use sekvent::context::SystemClock;
use sekvent::runtime::{Cors, Stage, UnitPolicy};
use sekvent::tenancy::invalidation::{Channel, Listener};
use sekvent::tenancy::label::LabelRules;
use sekvent::tenancy::{
    Connector, ObjectNames, Registry, RoleSecrets, ScopeFactory, ScopeFuture, SelectorRules,
    TenantInfo, TenantResolver, Tenants, control, hosts,
};
use sqlx::PgPool;

/// The tenant schema: run on each tenant's database as the tenant's role.
static TENANT_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("migrations/tenant");

/// What a request to a tenant gets. It needs no trait and need not be `Clone`.
struct BillingScope {
    pool: PgPool,
}

struct BillingFactory;

impl ScopeFactory for BillingFactory {
    type Scope = BillingScope;

    fn build<'a>(&'a self, _info: &'a TenantInfo, pool: PgPool) -> ScopeFuture<'a, BillingScope> {
        Box::pin(async move { Ok(BillingScope { pool }) })
    }
}

// control_pool: the product's pool on its control database, at least 3 connections.
// base: PgConnectOptions of the tenant server (host, port, TLS); its credentials are never used.
// master: the product's master-secret setting, a Secret read from its own key.
control::verify_schema(&control_pool).await?;
let registry = Registry::new(control_pool.clone(), Arc::new(SystemClock))?;
let rules = SelectorRules::new()
    .id_header(HeaderName::from_static("x-tenant-id"), ["api.example.com".to_owned()])?
    .path_label("/t/", ["go.example.com".to_owned()])?
    .labels(LabelRules::new().with_reserved(|label| matches!(label, "admin" | "api" | "go")))
    .tenant_hosts(true)
    .bypass(&["/billing.health.v1.Health/"]);
let tenants = Tenants::builder(
    registry,
    Connector::new(base, RoleSecrets::parse(&master, "billing.role.v1")?),
    ObjectNames::new("app_t_")?,
    rules.clone(),
    &TENANT_MIGRATOR,
    BillingFactory,
)
.build()?;

// The App's scope resolver; the listed streaming method gets the tenant id only.
builder.scope_resolver(TenantResolver::new(
    tenants.clone(),
    &["billing.invoices.v1.Invoices/Watch"],
))?;

// CORS for the id-header host, from the cache's host snapshot.
let snapshot = tenants.hosts().expect("the keyed form has a host snapshot").clone();
let cors = Cors::origin_predicate(hosts::predicate(snapshot, rules));
// … Server::builder().cors(cors) …

// The cache and the listener as runtime units; the listener fills the snapshot when it starts.
let listener = Listener::new(tenants.clone(), Channel::new("app_changes")?);
let runtime = runtime
    .unit("tenant-cache", Stage::Infrastructure, UnitPolicy::Critical, tenants.clone().into_unit())
    .unit("invalidation", Stage::Infrastructure, UnitPolicy::Critical, listener.into_unit());
```

A handler reads its tenant's scope from the context:

```rust
use sekvent::tenancy::TenantLease;

let lease = cx
    .scoped::<TenantLease<BillingScope>>()
    .ok_or_else(|| AppError::internal("no tenant scope"))?;
let open: i64 = sqlx::query_scalar("SELECT count(*) FROM invoices WHERE paid_at IS NULL")
    .fetch_one(&lease.pool)
    .await
    .map_err(sekvent::db::to_app_error)?;
```

## Names, hosts and labels

Tenant ids, object names, hosts and labels are in every build of the crate,
`tenancy-manager` included.

### Tenant ids

`TenantId` wraps a UUID. Its one text is the lower-case hyphenated form (36
characters): `Display` and `Debug` both print it, and `TenantId::parse`
accepts nothing else. Upper case, braces, the form without hyphens and URNs
give `None`, so a tenant has exactly one text in a header, a token claim or a
notification payload. `TenantId::LOCAL` is the nil UUID, the one tenant of a
process that serves a fixed scope.

```rust
use sekvent::tenancy::TenantId;

let tenant = TenantId::parse("0b9c6f1e-2d4a-4c8b-9e3f-5a6b7c8d9e0f").expect("canonical id");
assert_eq!(tenant.to_string(), "0b9c6f1e-2d4a-4c8b-9e3f-5a6b7c8d9e0f");
assert_eq!(TenantId::parse("0B9C6F1E-2D4A-4C8B-9E3F-5A6B7C8D9E0F"), None);
assert!(TenantId::LOCAL.is_local());
```

### Object names

Role and database statements (`CREATE ROLE`, `CREATE DATABASE`, `GRANT`,
`SET ROLE`, `DROP …`) take no bind parameters, so the crate formats names
into SQL. It only ever formats a checked `TenantRole` or `DatabaseName`, and
neither grammar admits a quote, a space or an upper-case letter.

`ObjectNames::new(prefix)` takes the product's prefix: 2 to 31 characters of
`a-z`, `0-9` and `_`, starting with a letter, ending with `_`, not starting
with `pg_` (anything else is `INVALID_ARGUMENT`). A tenant's role is the
prefix followed by the 32 lower-case hex digits of its id, and its database
has the same name; with the longest prefix that is 63 bytes, Postgres'
limit.

```rust
use sekvent::tenancy::{ObjectNames, TenantId};

let names = ObjectNames::new("app_t_")?;
let tenant = TenantId::parse("0b9c6f1e-2d4a-4c8b-9e3f-5a6b7c8d9e0f").expect("canonical id");
let role = names.role(tenant);
assert_eq!(role.as_str(), "app_t_0b9c6f1e2d4a4c8b9e3f5a6b7c8d9e0f");
assert_eq!(names.database(tenant).as_str(), role.as_str());
assert_eq!(names.tenant_of(role.as_str()), Some(tenant));
```

- `is_object_name` is true exactly for a name `role` can return: this
  prefix and 32 lower-case hex digits. `tenant_of` maps such a name back to
  its tenant.
- `has_prefix` is true for any name that starts with the prefix; the
  startup preflight treats such roles and databases as tenant-named.
- `parse_role` checks a role name read from storage. A name that is not an
  object name means the stored row was changed by hand, and the answer is
  `INTERNAL` before any statement is sent.
- `DatabaseName::parse` accepts `^[a-z_][a-z0-9_]{0,62}$` except
  `postgres`, `template0` and `template1`, and answers `INTERNAL` otherwise.
  `is_adoptable_name` is the same test as a `bool`.
- `is_key_id` checks a key id (role-secret or admin): 1 to `KEY_ID_MAX` (32)
  characters of `A-Z`, `a-z`, `0-9`, `_` and `-`.

### Hosts and origins

`host::canonical_host` gives the one spelling of a host that the crate
stores and compares, or a `HostError` (`Empty`, `TooLong`, `NotAscii`,
`Malformed`):

| Rule | |
|---|---|
| Input | at most 255 bytes (`HOST_INPUT_MAX`), ASCII only |
| Case | lowered |
| Trailing dot | one is removed |
| Name | at most 253 bytes; labels of 1 to 63 letters, digits and interior hyphens |
| Numeric last label | the whole host must be a dotted IPv4 address, octets without leading zeros |
| IPv6 | bracketed, written in its canonical form (`[2001:db8::1]`) |
| Port | 1 to 65535 without a leading zero, always validated; kept only on a loopback host |

Loopback hosts (`host::is_loopback_host`) are `localhost`, any
`*.localhost`, `127.0.0.0/8` and `[::1]`, with or without a port.

```rust
use sekvent::tenancy::host::{canonical_host, host_of_origin, origin_of_host};

assert_eq!(canonical_host("Billing.Example.com.:8443").as_deref(), Ok("billing.example.com"));
assert_eq!(canonical_host("localhost:5173").as_deref(), Ok("localhost:5173"));
assert_eq!(host_of_origin("https://billing.example.com:443").as_deref(), Some("billing.example.com"));
assert_eq!(host_of_origin("http://billing.example.com"), None);
assert_eq!(origin_of_host("localhost:5173"), "http://localhost:5173");
```

`host::host_of_origin` reads a browser's `Origin` value. It accepts
`https://<host>` with no port or `:443`, and on a loopback host `http://` or
`https://` with any port, which is kept. The scheme must be lower case; a
path, query, fragment or user info, and the value `null`, give `None`.
Origins are compared as canonical hosts, never by suffix or substring.
`host::origin_of_host` goes the other way: `https://<host>`, or
`http://<host>` on loopback.

### Tenant labels

A label is the first DNS label of a tenant's host, or a path segment that
stands for it. `label::LabelRules` holds the product's rules: by default 1
to 63 characters and nothing reserved; `with_length(min, max)` narrows the
length (`INVALID_ARGUMENT` unless `1 <= min <= max <= 63`) and
`with_reserved` adds the product's reserved labels. The crate ships no
reserved list.

`canonical` returns the lowered label or the first `LabelError`, checked in
this order: `Characters` (anything but ASCII letters, digits and `-`),
`Length`, `Hyphen` (at either end), `Punycode` (`--` at positions 3 and 4)
and `Reserved`. The reserved function is called with the lowered label, and
only for a label that passed every other rule.

```rust
use sekvent::tenancy::label::{LabelError, LabelRules, label_host};

let rules = LabelRules::new()
    .with_length(3, 30)?
    .with_reserved(|label| matches!(label, "admin" | "api" | "www"));
assert_eq!(rules.canonical("Orders").as_deref(), Ok("orders"));
assert_eq!(rules.canonical("ADMIN"), Err(LabelError::Reserved));
assert_eq!(rules.canonical("xn--abc"), Err(LabelError::Punycode));
assert_eq!(label_host("orders", "example.com"), "orders.example.com");
```

`label::path_label_host(segment, listener_host)` gives the host a path
label stands for: the canonical listener host with its first label replaced
by the lowered segment (`orders` on `app.example.com` stands for
`orders.example.com`). It is `None` on a loopback listener, on an IP-address
listener (`192.0.2.1`, `[2001:db8::1]`), for a segment that is not a DNS
label, for a listener host of one label and when the result is not a valid
host.

## Role credentials

No tenant role password is stored anywhere. Each tenant role's password is
derived when it is needed from a master secret the product holds, and the
database receives only a SCRAM-SHA-256 verifier of it.

### Master secrets

`RoleSecrets::parse` reads the product's master-secret setting, a `Secret`
of the form `key_id:secret[,key_id:secret…]`, together with the product's
context string (`^[A-Za-z0-9._-]{1,64}$`, for example `billing.role.v1`).

- The setting is split into entries at every `,`, so a secret cannot
  contain a comma.
- Each entry is trimmed as a whole and split at its first `:`, so any later
  colon belongs to the secret.
- Each key id passes `is_key_id` and occurs only once.
- Each secret is at least `secrets::MASTER_SECRET_MIN_BYTES` (32) bytes
  long, does not contain `change-me` in any letter case and is not a single
  repeated character.

A refusal is `INVALID_ARGUMENT`. Its message gives the position of the
entry (`role secrets: entry 2 has an invalid key id`) and never shows a key
id, a secret or a configuration key; the product puts its own key name in
front. `Debug` prints only the number of keys. Clones share the keys.

```rust
use sekvent::config::Secret;
use sekvent::tenancy::{RoleKey, RoleSecrets};

// Read from the product's own configuration key; this value is an example only.
let master = Secret::new("k1:example-only-not-a-real-master-secret");
let secrets = RoleSecrets::parse(&master, "billing.role.v1")?;
let key = RoleKey { key_id: "k1".to_owned(), generation: 1 };
let password: Secret = secrets.derive(tenant, &key)?;
```

### Key ids and generations

A `RoleKey` names the master secret a role's password comes from and its
generation, counted from 1. Each tenant's row stores its own key. `derive`
returns `INTERNAL` when the key id is not configured or the generation is 0.
The derivation is fixed, because a deployed tenant's database opens only
with exactly these bytes:

| Input | Value |
|---|---|
| Function | HKDF-SHA-256 (RFC 5869), extract then expand |
| Input key material | the bytes of the secret configured under the key id |
| Salt | the 16 bytes of the tenant's UUID |
| Info | `<context>:<key_id>:<generation>` in ASCII, with the generation in decimal |
| Output | 32 bytes in base64url without padding: a 43-character ASCII password |

Changing the context, the key id, the generation, the secret or the tenant
changes the password. `derive` returns it as a `Secret`; the connector puts
it into the driver's connect options (see [The connector](#the-connector)).

### The verifier

`ScramVerifier` (facade feature `tenancy-lifecycle`) is the form in which
Postgres stores the password:
`SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`, each part in standard
base64. `SaltedPassword` is one block of PBKDF2-HMAC-SHA-256 with
`secrets::SCRAM_ITERATIONS` (4096) iterations; `ClientKey`, `StoredKey` and
`ServerKey` follow RFC 5802, and the intermediate keys are wiped on drop.

- `ScramVerifier::new` uses a random 16-byte salt. The 4096 iterations block
  for a moment, so run it on the blocking pool.
- `with_salt` is the pure form, used in tests.
- `parse` accepts a verifier made elsewhere after a syntax check only
  (`secrets::is_scram_verifier`): the exact head, the iteration count
  `4096`, three non-empty base64 parts with at most two `=` each, and
  nothing before or after. Any other text is `INTERNAL`. A verifier is
  validated by its syntax only: the parts are not decoded and their lengths
  are not checked.

Roles are created and altered with the verifier only, so the password never
appears in a statement. The verifier's alphabet contains no quote, and it is
the only value the crate puts into a string literal. `Debug` prints
`ScramVerifier(..)`.

```rust
use sekvent::tenancy::ScramVerifier;

let verifier = tokio::task::spawn_blocking(move || ScramVerifier::new(&password))
    .await
    .map_err(AppError::internal)??;
```

### The connector

`Connector` builds the connect options of a tenant pool. The base
`PgConnectOptions` supplies the server: host, port or socket, TLS settings,
application name and any other options. The product parses its own base
URL and sets every field it relies on, because `PgConnectOptions::new()`
fills any gap from the `PG*` environment variables.

`options` clones the base and always replaces user, password and database
with the tenant role, the derived password and the tenant database, so the
base's own credentials never reach a tenant connection, even when they
belong to a superuser. Options are built field by field, never as a
formatted URL. `candidates` gives one candidate per key, in order, as
`sekvent::db::ConnectCandidates`; `KeyedPools` moves on to the next
candidate only after SQLSTATE `28P01`, which is the seam for rotating
passwords. An empty key list is `INTERNAL`. `Debug` prints `Connector(..)`.

The returned `PgConnectOptions` carry the derived password, because the
driver needs it to sign in. Hand them to the driver (a pool, `KeyedPools`,
`PgConnection::connect_with`) and never format or log them: their `Debug`
is the driver's.

```rust
use sekvent::tenancy::{Connector, ObjectNames};

let names = ObjectNames::new("app_t_")?;
let connector = Connector::new(base, secrets); // base: PgConnectOptions
let candidates = connector.candidates(
    tenant,
    &names.role(tenant),
    &names.database(tenant),
    &[key],
)?;
```

## The maintenance connection

Roles and databases are created, checked and dropped by a **maintenance
role**, which must not be a superuser. It has `LOGIN`, `CREATEDB` and
`CREATEROLE`, and it holds every tenant role `WITH INHERIT FALSE, SET TRUE`
(the crate grants that when it creates the role): it can switch to a tenant
role, and it has none of a tenant role's privileges until it does. It holds
no `CONNECT` on any tenant database. Give it no database of its own: let
the maintenance database belong to another role and grant the maintenance
role `CONNECT` on it, because any further database the maintenance role
owns makes a handover refuse (`owns_more`, see
[Adopting a database](#adopting-a-database)). Postgres 16 or later is
required for the membership options and for `CREATEROLE` giving `ADMIN
OPTION` on the roles it creates.

No other role may be a member of the maintenance role. A member reaches
every tenant role through it, and the tenant-role check reports any role
other than the maintenance role itself that reaches a tenant role
(`RoleFinding::ReachedBy`, see [Privilege inspection](#privilege-inspection)),
so `roles::create` and `databases::create` are then refused for every
tenant. On Postgres 16 and later a role with `CREATEROLE` that is not a
superuser becomes a member, with the admin option, of every role it
creates; a maintenance role created by such a role is reachable from its
creator. Create the maintenance role as a superuser, or remove the
creator's membership in it before the first tenant is created.

`Maintenance` (facade feature `tenancy-lifecycle`) wraps the role's connect
options and the product's `ObjectNames`. `role()` is the options' user and
`database()` the options' database (the role's name when none is set). It
allows one session at a time: clones share a single permit, and a second
caller waits for it. `connect` opens a `MaintenanceConn` on the maintenance
database; `connect_to(&database)` opens one on another database with the
same session rules, which the handover uses.

```rust
use sekvent::tenancy::{Maintenance, ObjectNames};
use sqlx::postgres::PgConnectOptions;

// Host, port, TLS, user, password and database: the product sets them all.
let options = PgConnectOptions::new_without_pgpass()
    .host("db.example.com")
    .username("billing_maintenance")
    .password(maintenance_password.expose())
    .database("billing_maintenance");
let maintenance = Maintenance::new(options, ObjectNames::new("app_t_")?);

let mut conn = maintenance.connect().await?;
// … roles::create, databases::create …
conn.close().await?;
```

| Bound | Value | Past it |
|---|---|---|
| waiting for the permit | 30 s | `UNAVAILABLE`, `the maintenance connection is busy` |
| connect, authenticate, session setup | 10 s | `UNAVAILABLE`, `the maintenance database did not answer in time` |
| server `lock_timeout` | 10 s | the statement's database error |
| server `statement_timeout` | 120 s | the statement's database error |
| one operation of `roles`, `databases` or `handover` (client side) | 150 s | `UNAVAILABLE`, `the maintenance operation did not finish in time`; the session is dropped and the connection poisoned |
| close | 10 s | `UNAVAILABLE`; the session is dropped and the permit returns all the same |

Every maintenance session runs with `search_path = pg_catalog, pg_temp`
(during an adoption it is connected to a database that is not the
product's, and no object there may stand in for a catalog function), with
the application name `sekvent-tenancy-maintenance`, and with sqlx's
statement logging off, because role statements carry verifiers. Catalog
reads are static SQL with bound names; utility statements are built only
from the checked `TenantRole`, `DatabaseName` and `ScramVerifier`.

### Poisoning

A session-level `SET ROLE` poisons the connection until its `RESET ROLE`
succeeds. An operation that leaves the session in an unknown state poisons
it for good: one that ran out of time, one whose future was dropped before
it finished, and one whose `BEGIN`, `COMMIT` or `ROLLBACK` failed. A
refusal or an error inside an operation's transaction is rolled back, and
the rollback awaited, before the error returns; the connection stays
usable. A poisoned
connection answers every operation of `roles`, `databases` and `handover`
with `INTERNAL` before sending any statement. It can only be closed:
`close` always returns the permit, and closing a connection whose session a
timeout already dropped returns `Ok`. Check `is_poisoned()` to decide
whether to open a new session.

### A read-only pool on another database

`Maintenance::pool_on(&database)` gives a lazy pool of at most one
connection on another database, for a product's read-only check before an
adoption. It holds the permit from the call until `MaintenancePool::close`,
its sessions start read-only (`default_transaction_read_only = on`), keep
the lock and statement bounds, and use the database's own `search_path`, so
use it only after `handover::prepare` found no object of a third role
there.

```rust
let pool = maintenance.pool_on(&candidate).await?; // candidate: DatabaseName
let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
    .fetch_one(pool.pool())
    .await
    .map_err(sekvent::db::to_app_error)?;
pool.close().await;
```

## Roles and databases

`cluster::roles` and `cluster::databases` (facade feature
`tenancy-lifecycle`) take a `MaintenanceConn` and the checked names. They
accept only roles of the connection's own `ObjectNames`: a `TenantRole` with
another prefix, a refused connection limit or (for `databases::create`) a
database name that is not generated is `INTERNAL` before the connection is
looked at. Refused names, drift and a misconfigured cluster are `INTERNAL`
without a reason (the cause is in the logged source); database errors map
through `sekvent::db::to_app_error`.

**Existing objects are checked, never repaired.** The maintenance role
cannot change every attribute (`SUPERUSER`, `REPLICATION`, `BYPASSRLS`) nor
revoke another grantor's membership, so a partial repair would report
success over a role that is still too wide. An existing role or database
that fails its check ([Privilege inspection](#privilege-inspection)) is
refused and left exactly as it was; drift is an operator's to resolve.

### Roles

- `roles::create(conn, &role, &verifier, limit)` runs in one transaction:
  it reads whether the role exists; an existing role must pass
  `privileges::check_tenant_role`; then `CREATE ROLE` (or `ALTER ROLE` for
  an existing one) with `LOGIN NOINHERIT NOCREATEDB NOCREATEROLE CONNECTION
  LIMIT <n> PASSWORD '<verifier>'`, then
  `GRANT <role> TO CURRENT_USER WITH INHERIT FALSE, SET TRUE`, and the
  tenant-role check of the result before the commit. A repeated call
  converges on the current verifier and limit. The limit is 1 to `i32::MAX`
  (0 would lock the role out).
- `roles::exists(conn, &role)` probes the catalog.
- `roles::drop(conn, &role)` is `DROP ROLE IF EXISTS`; drop the role's
  database first, because a role that owns one cannot be dropped.

### Databases

- `databases::create(conn, &database, &owner)`: `owner` must pass the
  tenant-role check. A missing database is created closed
  (`CREATE DATABASE … OWNER <owner> ALLOW_CONNECTIONS false`), because
  `PUBLIC` holds `CONNECT` by default. An existing one must be owned by
  `owner` and pass `privileges::check_tenant_database` (another owner, a
  template, a grant to a third role or to the maintenance role, `CREATE`
  for `PUBLIC` are refused). Then, always, as the owner in one transaction:
  `CONNECT` revoked from `PUBLIC` and granted to the owner, and connections
  allowed. The database opens only together with its restriction, so a call
  resumed after a failure at any point converges. Afterwards only the owner
  may connect.
- `databases::access(conn, &database)` returns a `DatabaseAccess` (the
  owner and whether `PUBLIC` holds an effective `CONNECT`), `None` for no
  such database.
- `databases::size(conn, &database)` reads `pg_database_size` as the owner
  (`SET LOCAL ROLE`) when a tenant role of the connection's names owns it,
  as the session's role when that role owns it; any other owner and a
  missing database are `INTERNAL`.

### The drop rule

`databases::drop(conn, &database, &owner)` destroys data, so it decides
from one catalog read and **drops a database only as the tenant's role and
only when that role owns it**, never as the maintenance role and never as a
discovered other owner. `database` need not be a generated name, because an
adopted database keeps its own:

| Found | Result |
|---|---|
| no such database | `Ok` |
| owned by `owner`, not a template, not the session's own database, `owner` is not the session's role | dropped as `owner` |
| owned by `owner` but a template, the session's own database, or `owner` is the session's role | `INTERNAL`, nothing dropped |
| owned by another role while `owner` exists | `INTERNAL`, nothing dropped |
| owned by another role and `owner` is gone | `Ok`: the tenant's database went before its role, and the name is someone else's now |

The drop itself is `SET ROLE <owner>`, `DROP DATABASE IF EXISTS … WITH
(FORCE)` and `RESET ROLE`, each alone and outside a transaction. The forced
drop runs as the tenant's role, so it ends that role's own sessions but
cannot end a session of another role (a superuser's or a monitoring
role's, for example): while such a session is connected to the database,
the drop fails, nothing is dropped, and the call can be issued again once
that session is gone. The connection is poisoned from before the `SET ROLE`
until the `RESET ROLE` succeeds. When the `SET ROLE` or the drop fails, the
`RESET ROLE` is still attempted: if it succeeds the connection is usable
again and the drop's error is returned; if it fails as well the drop's
error is returned and the connection stays poisoned. SQLSTATE `42501`
(`insufficient_privilege`) on the `SET ROLE` or the drop is `INTERNAL`
without a reason; Postgres reports a session the drop may not end with
that SQLSTATE too. Any other failure maps as a database error. Every
failed drop is safe to issue again.

```rust
use sekvent::tenancy::cluster::{databases, roles};
use sekvent::tenancy::ScramVerifier;

let role = names.role(tenant);
let database = names.database(tenant);
let password = secrets.derive(tenant, &key)?;
let verifier = tokio::task::spawn_blocking(move || ScramVerifier::new(&password))
    .await
    .map_err(AppError::internal)??;

let mut conn = maintenance.connect().await?;
roles::create(&mut conn, &role, &verifier, 20).await?;
databases::create(&mut conn, &database, &role).await?;
let bytes = databases::size(&mut conn, &database).await?;
conn.close().await?;

// Dropping: the database first, then its role.
let mut conn = maintenance.connect().await?;
databases::drop(&mut conn, &database, &role).await?;
roles::drop(&mut conn, &role).await?;
conn.close().await?;
```

## The startup preflight

The preflight proves at startup that no tenant role can open one of the
service's own databases. Run it before any migration, on every process that
serves tenants: it needs only catalog access, so a process without the
maintenance credential runs it too (`cluster::privileges` and
`cluster::preflight` are in the facade feature `tenancy`).

A **protected** database is one of the service's own: the control database,
the maintenance database, and any other the product names. It passes when:

1. `PUBLIC` holds neither `CONNECT` nor `TEMPORARY` on it. `PUBLIC`'s
   `CREATE` alone opens no session and is not a finding.
2. No tenant role owns it or may connect to it. A **tenant role** is any
   role whose name starts with the object-name prefix
   (`ObjectNames::has_prefix`) and, when the maintenance role is known, any
   role the maintenance role reaches through a membership chain, other than
   the database's owner, its allowed roles and the maintenance role itself.
3. Every other `LOGIN` role that may connect is its owner, an allowed role
   or a superuser. A superuser is the cluster operator's and not a finding;
   a superuser whose name has the prefix is a tenant role and is one.

"May connect" is the effective `CONNECT` as Postgres computes it: an own
grant, `PUBLIC`'s, an inherited membership's, or a superuser's. A database
without an ACL is read as its defaults (the owner's privileges, `PUBLIC`'s
`CONNECT` and `TEMPORARY`).

| `preflight::Finding` | Meaning | What the operator does |
|---|---|---|
| `Missing` | the database does not exist | fix the name the product passes, or create the database |
| `Public(privilege)` | `PUBLIC` holds `CONNECT` or `TEMPORARY` | `REVOKE … ON DATABASE … FROM PUBLIC` |
| `TenantOwner(role)` | a tenant role owns the database | give the database to the service's role |
| `TenantMayConnect(role)` | a tenant role (by prefix or by the maintenance role's reach) may connect | revoke its grant or the membership that gives it one |
| `RoleMayConnect(role)` | another `LOGIN` role may connect and is neither owner, allowed nor a superuser | revoke the grant, or add the role to the allowed list |

Nothing is repaired. A finding is `FAILED_PRECONDITION` without a reason,
its message naming the label, the database and every finding (`preflight of
the control database billing_control: PUBLIC holds TEMPORARY; tenant role
app_t_… may connect`); it never carries a URL, connect options or a
credential. A server older than Postgres 16 is refused with
`FAILED_PRECONDITION` too. Each call is bounded by `preflight::TIMEOUT`
(30 s); past it the answer is `UNAVAILABLE`, its message naming the
database the check was working on (`the preflight of the maintenance
database did not finish within 30 s`). A database error maps through
`sekvent::db::to_app_error`.

The usual call checks the control database on its pool, then the
maintenance database on a connection of its own (it may be another server),
closed afterwards even when the check failed; the check's error wins over a
failed close:

```rust
use sekvent::tenancy::ObjectNames;
use sekvent::tenancy::cluster::preflight;

let names = ObjectNames::new("app_t_")?;
// control_pool: the product's pool on its control database.
// maintenance_options: PgConnectOptions of the maintenance role.
preflight::check_control_and_maintenance(&control_pool, &maintenance_options, &names).await?;
// … then run the control migrations.
```

The control database allows the control pool's session role, the
maintenance database the maintenance role. When both are the same database
(same server, same database), one check runs and allows both roles.

A further database of the service's own is checked on any connection with
catalog access. `preflight::check`, `preflight::facts` and the reads of
`cluster::privileges` take a plain `sqlx::PgConnection`, which the product
opens itself (a pooled connection derefs to one); a `MaintenanceConn` does
not hand out its session. `allowed` lists the roles besides the owner and
superusers that may connect:

```rust
use sekvent::db::to_app_error;
use sekvent::tenancy::cluster::preflight::{self, Protected};

let reports = Protected {
    label: "reports".to_owned(),
    database: "billing_reports".to_owned(),
    allowed: vec!["billing_reporter".to_owned()],
};
// control_pool: the product's pool on its control database (same server).
let mut conn = control_pool.acquire().await.map_err(to_app_error)?;
preflight::check(&mut conn, &reports, Some("billing_maintenance"), &names).await?;
drop(conn);

// Or the pool's own database, allowing the pool's session role:
preflight::check_session_database(&reports_pool, "reports", Some("billing_maintenance"), &names).await?;
```

A check takes a fixed number of statements per database however many roles
the cluster holds: one query returns every role that may connect with the
facts the rules need, and the rules are pure. `preflight::facts` returns
those facts (`ProtectedFacts`); `preflight::protected_facts` builds them
from an owner, an ACL and the connecting roles a product read itself, and
`preflight::findings` applies the rules to them, so a product can report
findings its own way:

```rust
let mut conn = control_pool.acquire().await.map_err(to_app_error)?;
if let Some(facts) = preflight::facts(&mut conn, "billing_control", &[], Some("billing_maintenance"), &names).await? {
    for finding in preflight::findings(&facts, &[], &names) {
        tracing::error!(?finding, "the control database is reachable by a role it should not be");
    }
}
```

### Privilege inspection

The preflight and the cluster operations read privileges through
`cluster::privileges`, the one place that does. Every read is one static
statement with bound names, changes nothing and returns `None` for a
missing role or database:

| Function | Returns |
|---|---|
| `role_facts(conn, role)` | `RoleFacts`: the attributes, every membership edge naming the role (with its `ADMIN`, `INHERIT` and `SET` options), every role that reaches it and every role it reaches through a chain, sorted |
| `database_facts(conn, database)` | `DatabaseFacts`: owner, template flag, the ACL one privilege per entry (a NULL ACL as its defaults; an unknown privilege is `INTERNAL`) |
| `role_may_connect(conn, role, database)` | the role's effective `CONNECT` |
| `roles_that_may_connect(conn, database)` | every `LOGIN` role with an effective `CONNECT`, sorted |
| `public_may_connect(conn, database)` | whether `PUBLIC` holds `CONNECT` (`TEMPORARY` alone is not) |

Any membership edge is a path, whatever its options: `ADMIN` lets the
member grant itself the role, `SET` lets it switch, `INHERIT` gives it the
privileges. Two pure rules say what is wrong with an existing tenant
object:

- `tenant_role_findings(&facts, maintenance)` (`RoleFinding`): an attribute
  among `SUPERUSER`, `INHERIT`, `CREATEROLE`, `CREATEDB`, `REPLICATION`,
  `BYPASSRLS`; a role it reaches; a role other than the maintenance role
  that reaches it, members of the maintenance role included; an `INHERIT`
  on the maintenance role's membership.
- `tenant_database_findings(&facts, owner)` (`DatabaseFinding`): another
  owner; a template; any ACL entry beyond the owner's and `PUBLIC`'s
  `CONNECT` and `TEMPORARY` without grant option, a grant to the
  maintenance role included.

`check_tenant_role` and `check_tenant_database` run both on a connection of
the maintenance role and turn any finding, or a missing object, into
`INTERNAL` without a reason, the findings in the error's source. The roles
and databases operations call them on their own session; a product that
checks by itself opens a plain connection with the maintenance role's
options:

```rust
use sekvent::db::to_app_error;
use sekvent::tenancy::cluster::privileges;
use sqlx::{Connection, PgConnection};

// maintenance_options: PgConnectOptions of the maintenance role.
let mut conn = PgConnection::connect_with(&maintenance_options)
    .await
    .map_err(to_app_error)?;
let role = names.role(tenant);
privileges::check_tenant_role(&mut conn, &role).await?;
privileges::check_tenant_database(&mut conn, &names.database(tenant), &role).await?;
conn.close().await.map_err(to_app_error)?;
```

## Adopting a database

`cluster::handover` (facade feature `tenancy-lifecycle`) is the cluster half
of adopting an existing database as a tenant's: a database the maintenance
role owns becomes the tenant role's, with every object in it, and
afterwards opens only to that role. It knows no tenant row; the tenant
operation built on it is [Adopt an existing database](#adopt-an-existing-database).

| Function | Does |
|---|---|
| `handover::prepare(&maintenance, &database, &role)` | the checks, and on the reassign path the isolation of the database; returns the `Adoption` still to do. `role` need not exist yet |
| `handover::adopt(&maintenance, &database, &role)` | `prepare`, the transfer and the CONNECT restriction. `role` must exist and be held by the maintenance role, as `roles::create` leaves it |

`Adoption::Reassign` means the maintenance role still owns the database;
`Adoption::Restrict` means the tenant role owns it already and only the
CONNECT restriction is left. Both functions take a `Maintenance` and open
their own sessions, so call them while no `MaintenanceConn` or
`MaintenancePool` is open: a held permit makes them wait and answer
`UNAVAILABLE` after 30 s.

### Steps

Each step runs on a maintenance session of its own, one at a time, closed
after:

| Step | Session on | What |
|---|---|---|
| 1 | the maintenance database | the preconditions below up to `owns_more` |
| 2, reassign only | the database | `foreign_object`; then `CONNECT` revoked from `PUBLIC`, on its own; then `session` |
| 3, reassign only, `adopt` | the database | one transaction: every check of steps 1 and 2 again (the revoke too); the maintenance role's membership in the tenant role set to `INHERIT TRUE, SET TRUE`; the database's owner changed; `REASSIGN OWNED BY CURRENT_USER TO` the tenant role; the membership back to `INHERIT FALSE, SET TRUE`. An error rolls all of it back |
| 4, `adopt` | the maintenance database | as the new owner, in one transaction: `CONNECT` revoked from `PUBLIC` and granted to the owner, connections allowed |

The membership inherits only inside step 3's transaction, because
`REASSIGN OWNED` needs the privileges of the receiving role. One deadline
of 150 s covers the SQL of a whole call across its sessions (`UNAVAILABLE`
past it, and that session is dropped); waiting for the maintenance permit,
connecting and closing keep their own bounds.

### Preconditions

A refusal is `FAILED_PRECONDITION` with the reason `ADOPTION_REFUSED`, the
message `the database cannot be adopted` and the metadata key `check`; it
carries no domain. In the order they run:

| `check` | Refused when | What the operator does |
|---|---|---|
| `missing` | there is no such database | check the name |
| `template` | the database is a template | `ALTER DATABASE … IS_TEMPLATE false`, or pick another database |
| `owner` | neither the maintenance role nor the tenant role owns it | `ALTER DATABASE … OWNER TO` the maintenance role |
| `grantee` | its ACL names a role other than its owner (a grant to `PUBLIC` is fine) | revoke those grants |
| `owns_more` | reassign path: the maintenance role owns another database or a tablespace, which `REASSIGN OWNED` would move too | give those to another owner first |
| `foreign_object` | reassign path: an object in the database is owned by a role other than the maintenance role and the tenant role | change those owners or drop the objects |
| `session` | reassign path: a session of a role other than the maintenance role is connected (background workers do not count) | end those sessions and call again |

The object check reads both the recorded ownership dependencies of the
database (any third owner refuses) and the owner column of every catalog
that has one; there it skips what every database has by design: system
objects, temporary schemas and their contents, members of an extension and
internally dependent objects. An extension's own row counts.

Two arguments are refused as `INTERNAL` without a reason before any session
opens: a tenant role of another object-name prefix, and the maintenance
database itself.

### What a refusal leaves

The refusals of step 1 and `foreign_object` change nothing. `session`, and
a refusal of step 3's repeated checks, leave `PUBLIC`'s `CONNECT` revoked:
step 2 revokes it before the session check, so that no new session of a
role without a grant of its own can start while the operator clears the
database. Nothing else changes.

### Convergence and the end state

A call made again after an interruption converges. A call that failed
after step 3 committed finds the database owned by the tenant role and
takes the `Restrict` path; so does step 3 when another call finished the
transfer after this call's step 1, provided this call's session on the
database is already open. Two calls that overlap can still fail once: when
one call has just transferred the ownership, the maintenance role can no
longer connect to the database, so the other call's next step on it can
fail with a permission error. The call after that finds the database
owned by the tenant role, takes the `Restrict` path and converges.

Afterwards the tenant role owns the database and every object the
maintenance role owned in it, only the tenant role may connect, and the
maintenance role holds the tenant role `WITH INHERIT FALSE, SET TRUE`: it
can switch to it but has none of its privileges and can no longer connect
to the database.

Default privileges (`ALTER DEFAULT PRIVILEGES`) that the maintenance role
defined in the database stay the maintenance role's after the handover:
`REASSIGN OWNED` does not move them, and the handover does not refuse
them. Do not define default privileges as the maintenance role in a
database that will be handed over.

```rust
use sekvent::tenancy::cluster::{handover, roles};
use sekvent::tenancy::{DatabaseName, Maintenance, ObjectNames};

let maintenance = Maintenance::new(maintenance_options, ObjectNames::new("app_t_")?);
let database = DatabaseName::parse("inventory")?;
let role = maintenance.names().role(tenant);

// Before anything is recorded: may this database be adopted at all?
// (Revokes PUBLIC's CONNECT on the reassign path.)
let path = handover::prepare(&maintenance, &database, &role).await?;

// The role first (verifier as in "Roles and databases"), then the handover;
// both converge when called again.
let mut conn = maintenance.connect().await?;
roles::create(&mut conn, &role, &verifier, 20).await?;
conn.close().await?;

if let Err(err) = handover::adopt(&maintenance, &database, &role).await {
    if err.reason() == Some(sekvent::tenancy::reasons::ADOPTION_REFUSED) {
        // "session", "foreign_object", …: never a role, an object or a connection detail.
        let check = err.metadata().get("check").cloned();
        tracing::warn!(?check, "the database cannot be adopted yet");
    }
    return Err(err);
}
```

## The control schema

The product keeps one row per tenant in a control database it owns. The
crate creates no schema there: `control::REFERENCE_SCHEMA` is the DDL text
the product copies into a migration of its own. It holds three tables:

| Table | Holds | Keys the crate relies on |
|---|---|---|
| `tenants` | one row per tenant: `id`, `subdomain`, `display_name`, `state`, `provision_step` (default `''`), `db_cluster` (default `main`), `db_name`, `db_role`, `db_role_key_id`, `db_role_generation` (default 1), `schema_version`, `created_at`, `updated_at` | `tenants_pkey`; `tenants_db_role_key`, unique on `(db_cluster, db_role)`; `tenants_db_name_key`, a unique index on `(db_cluster, db_name)` over the rows that are not `purged` |
| `tenant_hosts` | every host of a tenant: `host`, `tenant_id`, `is_primary`; deleted with its tenant | `tenant_hosts_pkey` (one tenant per host); `tenant_hosts_primary_key`, a unique index on `tenant_id` over the primary hosts (one primary per tenant) |
| `admin_audit` | the admin operations; checked only with feature `admin` | `admin_audit_pkey` |

A product without feature `admin` may leave `admin_audit` out of its
migration.

`control::verify_schema(&pool)` runs at startup, after the
[preflight](#the-startup-preflight) and the control migrations. It selects
every column the crate uses from `tenants` and `tenant_hosts` (and, with
feature `admin`, from `admin_audit`) with a false predicate, then reads from
the catalog that each key is a valid unique index of its table (a primary
key and a unique constraint have one), in the order `tenants_pkey`,
`tenants_db_role_key`, `tenant_hosts_pkey`, `tenants_db_name_key`,
`tenant_hosts_primary_key` (then `admin_audit_pkey`). The two partial keys of
the reference schema, `tenants_db_name_key` and `tenant_hosts_primary_key`,
must be partial indexes (with a `WHERE`); every other key must cover every
row. A gap is `FAILED_PRECONDITION` / `CONTROL_SCHEMA_MISSING` without a
domain, with the metadata key `table` (a missing table or column) or
`constraint` naming the first key that is missing, not unique, invalid (as a
failed `CREATE UNIQUE INDEX CONCURRENTLY` leaves it), partial where it must be
total or total where it must be partial. Any other failure, a missing grant
for example, is the database error as `sekvent::db::to_app_error` maps it.

Extra columns and extra indexes are the product's and pass. Every statement
of the crate names its columns, so an extra column on `tenants` must be
nullable or have a default. The product reads and writes its own columns
with statements of its own.

### What a row holds

- `TenantRow`: the whole row with its hosts: the `TenantInfo`, the
  `ProvisionStep`, the stored cluster label (`db_cluster`), database name
  (`db_name`) and role name (`db_role`) as stored, the stored
  `schema_version` and the two timestamps (`SystemTime`).
- `TenantInfo`: what serving needs: `id`, `subdomain`, `display_name`, the
  `TenantState`, `primary_host`, `hosts` (canonical, byte-sorted, without
  duplicates) and the `RoleKey`. `origin()` is the primary host's origin,
  `None` without one. `Debug` prints the id, the subdomain and the state
  only.
- `TenantState`: `provisioning`, `active`, `suspended`, `purging`, `purged`,
  stored in lower case.
- `ProvisionStep`: how far provisioning got, in the order the steps are
  recorded: `Inserted` (stored as the empty text), `Role`, `Database`,
  `Migrated`, `Seeded`, `Active`.

A stored value the crate cannot use (a state or step outside those texts, a
generation that is not positive, hosts without a primary, a tenant without
hosts that is not `purging` or `purged`) is `INTERNAL`; the cause names the
column, never the value. The stored names are copied as they are; the
tenant cache opens a tenant only when `db_cluster` is `control::MAIN_CLUSTER`
(`main`), `db_role` equals the role `ObjectNames::role` derives and
`db_name` passes `DatabaseName::parse` ([What a cold open does](#what-a-cold-open-does)).

### The row functions

`control::rows` holds static SQL with bound values. Every function takes
the caller's `&mut PgConnection` and never begins, commits or rolls back:

| Function | Does |
|---|---|
| `insert` | in the caller's transaction (two statements): checks the `NewTenant`'s hosts before any statement (at least one, each canonical, the primary among them; sorted and de-duplicated) and its role key's generation (1 to 2147483647), inserts the row in `provisioning` with its hosts and returns `(row, true)`. A refused host is `INVALID_ARGUMENT` naming its position from 0, never its text (`host 2 is not a canonical host`, field violation `hosts[2]`). A repeated id changes nothing and returns `(the existing row, false)`. A host, role name or database name another tenant holds is `FAILED_PRECONDITION` / `TENANT_STATE`; a database name is free again once its tenant is `purged`, a role name never is |
| `get`, `by_host` | one row with its hosts, in one statement |
| `lock` | the row under `FOR UPDATE`, then its hosts by a second statement, so a caller that waited for the lock sees the hosts the holder committed |
| `by_loopback_label` | the one tenant that is not `purged`, carries the label as its subdomain and has a loopback host; `None` when there is none or more than one carries the label |
| `list_active` | every `active` tenant with its hosts, in id order; one row that does not decode fails the whole read |
| `set_state` | the state and `updated_at`; whether a row changed |
| `set_hosts` | in the caller's transaction (three statements): the whole host set and the subdomain, with `insert`'s host checks and conflicts; `NOT_FOUND` without a reason for a missing row |
| `store_schema_version` | raises `schema_version`, never lowers it, never touches `updated_at`, and takes the row `FOR UPDATE SKIP LOCKED`: a locked row, and a missing one, answer `VersionStored::Busy` at once |

### The registry

`Registry::new(control_pool, clock)` wraps the control pool and does no I/O.
The pool needs at least `Registry::MIN_CONNECTIONS` (3) connections, so
lookups and lifecycle work each keep room; a smaller pool is
`INVALID_ARGUMENT`. The `Clock` (`sekvent::context::SystemClock` in
production) dates the lookup cache's entries.

| Call | Reads |
|---|---|
| `lookup(&selector)` | a request's lookup, through the lookup cache and its admission ([Lookups and their admission](#lookups-and-their-admission)); the row in any state. `Tenants::select` and `Tenants::get`'s state check read through it |
| `tenant(id)` | an internal read by id through the same cache, without a permit |
| `tenant_now(id)` | the row as committed now: no cache, no permit |
| `active()` | every active tenant with its hosts (`rows::list_active`): one active row that cannot be decoded fails the whole read, and with it every caller of the active list: the walks, the sweep, `refresh_hosts` and the invalidation listener's loads |
| `store_schema_version(id, version)` | the row function of that name |
| `forget(id)`, `clear_lookups()` | drop cached lookups, no I/O |
| `admit()` | a lifecycle permit for the product's own lifecycle work: waits up to 10 s, then `UNAVAILABLE` without a reason |
| `pool()` | the control pool |

Each read holds one control connection for one statement and gives it back
before its answer is stored. Clones of a `Registry` share everything: one
lookup cache with its reads in flight, one set of lookup permits and one set
of lifecycle permits. `Debug` prints `Registry(..)`.

```rust
use std::sync::Arc;

use sekvent::context::SystemClock;
use sekvent::tenancy::{Registry, Selector, control};

// control_pool: the product's pool on its control database (max_connections >= 3).
control::verify_schema(&control_pool).await?;
let registry = Registry::new(control_pool, Arc::new(SystemClock))?;
let row = registry.lookup(&Selector::Host("orders.example.com".to_owned())).await?;
```

## Lookups and their admission

A request that names a tenant asks the control database which tenant that
is before the caller is authenticated. `Registry::lookup` answers through a
lookup cache and a lookup admission, so such requests take a bounded share
of the control pool however many arrive.

**Key spaces.** A lookup is keyed by tenant id (`Selector::Id`) or by exact
canonical host (`Selector::Host`, and a path label, which
`label::path_label_host` turns into the host it stands for: the listener
host with its first label replaced by the lowered segment). A host that is
not exactly its canonical spelling, a path segment that is no DNS label, and
a path label on a listener from which no host can be derived and that is
not loopback (an IP address, a host of one label) answer "no tenant"
without a read. Each key space has two separate
partitions: 4,096 found entries and 1,024 not-found entries. A found entry
replaces a not-found entry of the same key and the reverse. A host lookup
that finds a row also stores it in the id space; an id lookup stores
nothing in the host space.

**Replacement.** An entry is replaced only by the answer of a read that
started later than the read that made it; on a tie the stored entry stays.
So a slower, older read never overwrites a newer observation: a host read
whose answer arrives after an id read of the same tenant that started later
keeps its host entry but leaves the id entry alone.

**Lifetime.** An entry is served for 30 s counted from the start of the
read that produced it, on the registry's `Clock`, and only while
`start <= now < start + 30 s`, so a clock set back never extends it. An
entry found not fresh (expired, or dated after now because the clock was set
back) is removed when it is found, so it is never served later, whatever the
clock does next. An answer already past its lifetime when its read ends goes
to its caller and is not stored. Errors are never cached, and an entry past
its lifetime is never served, also not when the read that would replace it
fails. The lifetime bounds how long a cached
lookup answer is reused, nothing more: it does not bound how fast a change
takes effect. A read already running still answers the caller that
leads it, and a lease a caller already holds is not taken back. Changes
reach other processes through
[the invalidation channel](#follow-changes-from-other-processes).

**A full partition.** When a new key does not fit, the partition first
drops its expired entries; when it is still full, that partition alone is
cleared. Not-found entries never push found entries out, and a full
partition never clears another one. The key spaces are not independent
otherwise: a host read that finds a row also fills the id space, and
`forget` drops entries from both.

**Forgetting.** `Registry::forget(tenant)` drops the tenant's id entry,
every found host entry of the tenant and every not-found host entry.
`Registry::clear_lookups()` drops everything. The cache has one epoch,
which both raise, so either one stops every read already running, of any
tenant and any key, from storing its answer; the caller that started such a
read still receives it.

**The lookup admission.** A miss takes one of `max(1, n / 2)` lookup
permits, `n` being the control pool's `max_connections`, without waiting.
With none free the lookup answers `UNAVAILABLE` / `TENANT_BUSY` (retry hint
1 s) at once, before any control connection is asked for. `Registry::lookup`
returns it without a domain; `Tenants::select` and `Tenants::get` add the
serving domain.

**Joining.** At most one read of a key runs at a time. A caller that misses
while a read of the same key is running joins it: it waits until that read
ends, however it ends, and looks at the cache again, without taking a
permit. A read that fails returns its error to the caller that started it
only; its joiners look again and may lead a read of their own. A caller
that has joined twice and still finds no answer gets `TENANT_BUSY`. A read
that ends gives its permit back first, then leaves the reads in flight, and
wakes its joiners last: a caller of the same key that arrives in between
joins the finished read (and looks again at once) instead of getting
`TENANT_BUSY`.

**A caller that goes away.** The read runs on a task of its own. Dropping
the caller's future cancels nothing: the read keeps its permit, finishes,
fills the cache and only then gives the permit back. A read task that
panics answers its caller `INTERNAL` and still gives its permit and its key
back.

**Internal reads.** `Registry::tenant(id)` (the first row read inside a
tenant cache's creation, the product's lifecycle code) uses the same cache and stores its answers
under the same rules, but takes no permit, never joins a request's read and
is never joined by one. Its read runs on the caller's own task, so a caller
that goes away stores nothing. `Registry::tenant_now(id)` bypasses the
cache.

**The loopback path label.** On a loopback listener (`localhost`,
`*.localhost`, `127.0.0.0/8`, `[::1]`, any port) a path label cannot be
turned into a host. There the label selects a tenant only when exactly one
tenant that is not purged carries it as its subdomain and that tenant has
at least one loopback host (any port); otherwise nothing is selected. This
read is never cached and never joined, and each one takes a lookup permit
like any miss.

**Next to the App's admission.** The App's serve admission
([components](components.md#the-serve-admission)) sits in front and bounds
how many unauthenticated calls are pending at all. The lookup admission
bounds how much of the control pool those calls can take, so lifecycle
work, the invalidation listener and the product's own statements keep their
share ([Sizing](#sizing)).

## Select the tenant

Which tenant a request names is decided from the request head before the
caller is authenticated. The product builds `SelectorRules` from its own
configuration:

```rust
use http::HeaderName;
use sekvent::tenancy::SelectorRules;
use sekvent::tenancy::label::LabelRules;

let rules = SelectorRules::new()
    .id_header(HeaderName::from_static("x-tenant-id"), ["api.example.com".to_owned()])?
    .path_label("/t/", ["go.example.com".to_owned()])?
    .well_known(["app-links".to_owned()])?
    .labels(LabelRules::new().with_reserved(|label| label == "admin"))
    .tenant_hosts(true)
    .bypass(&["/billing.health.v1.Health/"]);
```

| Builder | Rule |
|---|---|
| `id_header(name, hosts)` | requests to these hosts name the tenant by id in the header `name` |
| `path_label(prefix, hosts)` | requests to these hosts under `prefix` name it by the path segment after the prefix. The prefix starts and ends with `/`, is longer than `/` and is visible ASCII without `%`, `?` or `#`. Since the prefix is reserved on every host, `/` alone, which would reserve every path, is refused |
| `well_known(names)` | `<prefix>.well-known/<name>/<label>` selects like `<prefix><label>`; each name is 1 to 64 of `[A-Za-z0-9._-]`; a second call replaces the list |
| `labels(rules)` | the product's `LabelRules` (default `LabelRules::new()`); a path label they refuse selects nothing and causes no read |
| `tenant_hosts(true)` | any other host is looked up as a tenant's own host |
| `bypass(prefixes)` | RPC path prefixes that belong to no tenant; a second call replaces the list |

Hosts are stored in canonical form (`host::canonical_host`). A host the
builder refuses is `INVALID_ARGUMENT` naming its position
(`id-header host #2 is not a valid host`), never its text. `id_header` and
`path_label` may each be called once, and a host may be in both lists only when it is
loopback. A bypass prefix is a plain string prefix of the request path as
received, written with its leading `/` and never percent-decoded: `"/"`
would bypass every path, a prefix without a trailing `/` also matches longer
service names (`/billing.v1.Admin` matches `/billing.v1.AdminOther/List`),
and an empty prefix matches nothing. End every bypass prefix with `/`.
`Debug` prints the counts of hosts, names and prefixes only.

**The request host** is the canonical form of the URI authority and of the
one `Host` header; when both are present they must agree. An authority with
user info, a repeated `Host`, a `Host` that is not text or not a host, two
sources that disagree, or neither source present: no usable host.
Forwarded-host headers are never read.

**The selection order.** The path-label prefix is a reserved namespace on
every host: a request under it never falls through to another rule.

| Order | Condition | Result |
|---|---|---|
| 1 | the path starts with a bypass prefix | no tenant and an empty scope, before the host is looked at |
| 2 | no usable request host | `TENANT_NOT_HERE` |
| 3 | the path is under the prefix | on a path-label host: the label (the first segment after the prefix, or after `.well-known/<name>/`), lowered by the label rules and looked up as the host it stands for (the request host with its first label replaced by the label); what follows the label is ignored. On any other host, or with an empty label, one holding `%`, one the label rules refuse or an unlisted well-known name: `TENANT_NOT_HERE` |
| 4 | the host is an id-header host | the id in the header, which must be exactly one value in `TenantId`'s text; absent: `TENANT_SELECTOR_MISSING`; repeated or malformed: `TENANT_MISMATCH` |
| 5 | the host is a path-label host | `TENANT_NOT_HERE`: a path-label host serves nothing outside the prefix |
| 6 | `tenant_hosts` is on | the host is looked up as a tenant's own host |
| 7 | otherwise | `TENANT_NOT_HERE` |

On a loopback listener a path label names the one tenant that carries it
and has a registered loopback host
([the loopback path label](#lookups-and-their-admission)).

The tenant found is checked for its state first (`TENANT_PROVISIONING`,
`TENANT_SUSPENDED`, `TENANT_NOT_HERE`, see [States](#states)), and only then
for the claims of the request, so a refusal tells a caller nothing about a
tenant it could not name:

- **The optional id header** of a path-label or tenant-host request: when
  present it must be exactly one valid id equal to the tenant found,
  otherwise `TENANT_MISMATCH`.
- **The origin** of an id-header request: a request without `Origin`
  passes; otherwise it must carry exactly one `Origin` whose canonical host
  is one of the selected tenant's hosts, otherwise `ORIGIN_NOT_ALLOWED`.
  Only an id-header selection is followed by this check: a path-label or
  tenant-host selection has no origin check. On a loopback host listed for
  both selectors, a request under the path-label prefix is a path-label
  selection without an origin check, while the
  [CORS predicate](#cors) answers for that host as an id-header host.

`Tenants::select(&parts)` runs all of this and returns the tenant's id
(`None` for a bypass path) without opening the tenant; every refusal carries
the serving domain.

### The scope resolver

`TenantResolver` is the App's `ScopeResolver`
([components](components.md#how-to-resolve-a-request-scope)):

```rust
use sekvent::tenancy::TenantResolver;

builder.scope_resolver(TenantResolver::new(
    tenants.clone(),
    &["billing.invoices.v1.Invoices/Watch"],
))?;
```

- The scope's tenant is the tenant id's text, so `cx.tenant()` is the id,
  and an end user of another tenant is refused by the framework's tenant
  rule.
- A handler reads its tenant's scope with
  `cx.scoped::<TenantLease<S>>()`.
- **A streaming method gets the tenant id only.** The listed paths
  (`<package>.<Service>/<Rpc>`; an entry with one leading `/` names the same
  method) are each compared whole with the routed path, never as a prefix
  and never percent-decoded. They get a `StreamTenant(id)` in the scope
  instead of a lease, and resolving them opens nothing. A served
  stream keeps its scope until it ends, so a lease there would keep the
  tenant's pool in use, and a suspended tenant's stream alive, for that
  long. The streaming handler opens the tenant per read with
  `tenants.get(id)` and drops the lease after it:

  ```rust
  use sekvent::tenancy::StreamTenant;

  let StreamTenant(tenant) = *cx
      .scoped::<StreamTenant>()
      .ok_or_else(|| AppError::internal("no tenant scope"))?;
  // For each read of the stream:
  let lease = tenants.get(tenant).await?;
  let unpaid: i64 = sqlx::query_scalar("SELECT count(*) FROM invoices WHERE paid_at IS NULL")
      .fetch_one(&lease.pool)
      .await
      .map_err(sekvent::db::to_app_error)?;
  drop(lease);
  ```

- **A bypass path gets an empty scope in every form**: through the rules'
  `bypass` on the keyed form, through `Tenants::fixed_with_bypass` on the
  fixed form.
- **The fixed form** names `TenantId::LOCAL` as the scope's tenant, so its
  end users carry the local tenant's id text
  (`00000000-0000-0000-0000-000000000000`).
- Errors reach the caller unchanged: the resolver passes on what `Tenants`
  answers, and only `Tenants` adds the serving domain. The resolver logs
  nothing and opens no span.

## CORS

`HostSnapshot` holds every host of every active tenant in memory and is
replaced as a whole. It starts empty, compares hosts as given, and its
`Debug` prints only its size; clones share the map. The keyed tenant cache
owns one (`Tenants::hosts()`), which `Tenants::refresh_hosts()` reloads and
the [invalidation listener](#follow-changes-from-other-processes) keeps
current: it loads it when it starts, after every change and at every
resync. `hosts::refresh_hosts(&snapshot, &registry)` reloads any snapshot:
it reads the active tenants with one statement, gives the connection back,
then swaps the map and returns what it read; on a failure the old map
stays. Reloads of one snapshot are serialised across its clones.

`hosts::predicate(snapshot, rules)` is the function for
`Cors::origin_predicate` ([server](server.md#configure-cors)):

```rust
use sekvent::runtime::Cors;
use sekvent::tenancy::hosts;

let snapshot = tenants.hosts().expect("the keyed form has a host snapshot").clone();
let cors = Cors::origin_predicate(hosts::predicate(snapshot, rules.clone()));
```

- **It answers for id-header hosts only** and never reads the path: it
  allows an origin when the request host is an id-header host of the rules,
  the head carries exactly one `Origin`, and that origin's canonical host is
  in the snapshot. A path-label host or a tenant's own host as request host
  gets no cross-origin headers from it; a product that wants more composes
  its own function with `Cors::or_origin_predicate`.
- **It may allow the origin of any active tenant**, not only the one the
  request names: CORS only shapes response headers. The resolver then
  checks the origin against the selected tenant's own hosts
  (`ORIGIN_NOT_ALLOWED`) for an id-header selection only. On a loopback
  host listed for both selectors the predicate also answers for a request
  under the path-label prefix, whose selection has no origin check.
- It is synchronous, reads memory only, and owns the snapshot and the rules
  it was given.
- Methods, allowed and exposed headers and the max age are the product's
  `Cors` settings.

## The tenant cache

`Tenants<S>` is what a product holds: one scope per tenant, built once
however many requests arrive for a cold tenant, on a pool that connects as
the tenant's own database role and is migrated before anyone sees it.
Clones share everything; `Debug` prints the stats only.

### The scope factory

The product says how a scope is built with a `ScopeFactory`. The build is
async, may fail, and may take seconds (a scope that loads a tenant's working
set into memory, say):

```rust
use sekvent::tenancy::{ScopeFactory, ScopeFuture, TenantInfo};
use sqlx::PgPool;

struct BillingScope {
    pool: PgPool,
    prices: PriceTable,
}

struct BillingFactory;

impl ScopeFactory for BillingFactory {
    type Scope = BillingScope;

    fn build<'a>(&'a self, _info: &'a TenantInfo, pool: PgPool) -> ScopeFuture<'a, BillingScope> {
        Box::pin(async move {
            let prices = PriceTable::load(&pool).await?;
            Ok(BillingScope { pool, prices })
        })
    }
}
```

The scope type needs no trait and need not be `Clone`: the cache keeps the
`TenantInfo` the scope was built from next to it, and a `TenantLease<S>` is
`Clone` and derefs to `S` whatever `S` is.

### The two forms

- `Tenants::builder(registry, connector, names, rules, &TENANT_MIGRATOR,
  factory)` gives a `TenantsBuilder` for the keyed form. Its settings are
  the keyed cache's ([Sizing](#sizing)) plus `domains(ErrorDomains)`;
  `build()` refuses the values the keyed cache refuses (a zero capacity,
  zero connections, a zero acquire or create timeout, a zero reap
  interval, a zero `Some` idle eviction, connection idle timeout or
  lifetime) with `INVALID_ARGUMENT` naming the setting. The cache makes its own `HostSnapshot`.
- `Tenants::fixed(scope)` serves one scope as `TenantId::LOCAL` through the
  same calls, for a single-tenant deployment: no registry, no snapshot, no
  cache. It serves the local tenant id alone: `get` of any other id is
  `TENANT_NOT_HERE`. `Tenants::fixed_with_bypass(scope,
  &["/billing.health.v1.Health/"])` also names RPC path prefixes that belong
  to no tenant: a request under one of them gets an empty scope, compared
  exactly as `SelectorRules::bypass` compares. The fixed form answers under
  the default domains, its `stats()` reports capacity 1 and one warm scope,
  and `invalidate`, `evict_idle_older_than` and `refresh_hosts` do nothing.

### What a cold open does

`get(id)` first checks the tenant's state with a request's lookup by id
(`Registry::lookup`): through the lookup cache, joining a read of the same
id already in flight, otherwise under a lookup permit taken without waiting.
With no free permit, or after a second join without an answer, it answers
`TENANT_BUSY`. So many concurrent `get`s of one id whose lookup is not
cached cause one control read. A tenant that is not `active` is refused with
its state's reason. When the cache holds no scope `get` starts one creation,
on a task of its own, whose reads are internal reads without a permit; in
order it:

1. reads the row again through the lookup cache (`Registry::tenant`) and
   refuses a row that is not `active`;
2. checks the stored row: `db_cluster` must be `main`, `db_role` must be
   exactly the role `ObjectNames::role(id)` derives, and `db_name` must pass
   `DatabaseName::parse` (an adopted name is allowed). A row that fails
   connects to nothing;
3. connects as the derived role, with the password derived from the master
   secret under the row's role key ([The connector](#the-connector)); the
   base connect options' own user, password and database are never used;
4. runs the tenant migrator on the new pool, so the migration runs as the
   tenant's role; a failed migration, or a database that holds a migration
   the binary does not know, fails the open;
5. reads the row as committed now (`Registry::tenant_now`) and checks its
   state and its stored names again. The row must still be `active` and
   must name the same database, the same role and the same role key (id and
   generation) the pool was opened with; otherwise the open is
   `TENANT_UNAVAILABLE` (log stage `row`), the factory is not called,
   nothing is cached, and the next `get` opens from the row as it is then;
6. calls the factory with that row's info and the pool.

**Build the tenant migrator without `ignore_missing`.** Step 4 refuses a
database ahead of the binary only because sqlx's `Migrator` refuses a
migration it does not know; with `ignore_missing` set it passes over such a
migration, and the scope is built on a schema the binary does not know.

An open writes nothing to the control database; only the
[migration sweep](#walks-and-the-sweep) records schema versions.

### The lease

`get` and `resolve` return a `TenantLease<S>`. Hold it for the call and drop
it: while a lease (or a clone of it) exists the cache never evicts the
scope, and a scope retired meanwhile stays usable until its last lease
drops. `lease.id()` and `lease.info()` give the tenant and the info the
scope was built from; `Debug` prints the id only.

### The other calls

| Call | Does |
|---|---|
| `select(&parts)` | the tenant a request names, after the lookup, the state check and the id-header and origin checks ([Select the tenant](#select-the-tenant)); `None` for a bypass path; never opens a tenant |
| `resolve(&parts)` | `select`, then `get`; `None` for a bypass path |
| `peek(id)` | the warm scope or `None`. Never opens, never reads the row and does not count as a use, so it can return the scope of a tenant whose row changed a moment ago |
| `invalidate(id)` | forgets the tenant's lookups and retires its warm scope or its creation in flight; whether there was one. Holders keep their lease; a retired creation runs to its end, its scope is dropped unseen and its waiters look again |
| `evict_idle_older_than(idle)` | evicts every scope nothing holds that was last used at least `idle` ago; how many. `Duration::ZERO` evicts every scope not held |
| `stats()` | `TenantsStats`: the gauges `capacity`, `warm`, `in_use`, `creating`, `retiring` and the counters `creations`, `create_failures`, `evictions`, `capacity_refusals` |
| `refresh_hosts()`, `hosts()`, `registry()`, `domains()` | the host snapshot's reload, the snapshot, the registry (`None` on the fixed form) and the error domains |
| `close()` | closes the cache; a closed cache stays closed and answers `TENANT_UNAVAILABLE` |

### States

| The row's state | `get`, `select` and `resolve` |
|---|---|
| `active` | served |
| `provisioning` | `UNAVAILABLE` / `TENANT_PROVISIONING` |
| `suspended` | `FAILED_PRECONDITION` / `TENANT_SUSPENDED` |
| `purging`, `purged`, no row | `NOT_FOUND` / `TENANT_NOT_HERE` |

The state is checked before the open and twice inside it, so **a creation
never caches a tenant that is not `active`**. A warm scope of a tenant that
later left `active` is refused at the next `get` by its state check, once
the lookup it reads shows the new state (at once after the tenant's lookups
are forgotten, which the invalidation listener does on every process,
otherwise when the cached answer's 30 s are up). The entry itself is retired
by the listener's invalidation, by `invalidate` or by idle eviction; until
then `peek`, which reads no row, can still return it. A lease already held
stays usable until it drops.

### How an open can end

| The open ends with | The caller gets |
|---|---|
| a refusal of the factory with a reason | that error unchanged: code, message, reason, domain, metadata and field violations, and its retry hint to the millisecond, whatever the reason text is (one equal to a framework reason included); no serving domain is added |
| the row not `active` (any of its reads) | the state's reason, under the serving domain |
| a full cache (every slot warm and held, or creating), or no free lookup permit for `get`'s state check | `UNAVAILABLE` / `TENANT_BUSY`, retry hint 1 s |
| a panic anywhere in the creation, the factory's included | `INTERNAL`, no reason |
| anything else: a failed control read, the stored row refused, a row that changed under the open (another database, role or role key than the pool's), the connect or the credentials refused, a failed migration, the create deadline, a closed cache, a factory error without a reason | `UNAVAILABLE` / `TENANT_UNAVAILABLE`, its fixed message, no metadata |

The keyed cache logs every failed open once, at `warn` (`creating a cached
value failed`), with the error's code, reason and source. A state refusal
has no source. Any other source is a fixed stage text, `opening the tenant
failed: <stage>` with the stages `control_read`, `row`, `candidates`,
`migrate` and `build`, followed by ` (sqlstate <code>)` when a
`control_read` or `migrate` failure carried a SQLSTATE (`opening the tenant
failed: migrate (sqlstate 42501)`); or, for a refused connect, the keyed
cache's own `connect failed: <class>, <n> of <m> candidates tried`, with
` (sqlstate <code>)` after the class when there is one. It never carries connection details: no role, database
name, host, connect option, password or driver text. The factory's own
source is dropped and not logged; a factory that wants its cause in the log
writes its own event, with no connection detail in it.

### Cold-open rules

- One creation per tenant however many callers wait; every waiter gets its
  outcome.
- The creation runs under one deadline (`create_timeout`, 30 s by default)
  that covers the connect, the migration and the build; a pool the creation
  connected is closed when it ends without a scope. A product with a slow
  build raises the deadline.
- Cancelling a waiter cancels nothing and frees no slot: warm and creating
  tenants share the capacity, so callers that disconnect cannot turn cold
  opens into unbounded work.
- A failure is never cached: the next `get` tries again. A product whose
  build refuses on purpose bounds that cost itself.
- A scope is dropped before its pool is closed, so a scope that owns tasks
  ends them in its `Drop`.

### Walks and the sweep

- `for_each_active(|lease| async move { … })` visits the active tenants in
  id order, one after another, with a lease each, dropped when the visit
  ends. A tenant whose open the cache or the lookup refuses on its own as
  busy (the cache is full, or no lookup permit is free) is counted in `busy`
  and skipped; any other failed open, a refusal of the product's factory
  included whatever its reason, and an error the visit returns, is recorded
  in `failed` and logged at `warn` with the tenant id, the code and the
  reason only. Only a failed list read fails the walk. The answer is a
  `WalkReport { visited, busy, failed }`.
- `migration_sweep()` opens every active tenant (which migrates it), at most
  two at a time and never more than the cache's capacity (one at a time on
  a cache of one), drops each lease at once, then stores the migrator's
  highest up migration version in the tenant's row with `FOR UPDATE SKIP
  LOCKED`, so it never waits for a transaction holding the row. A locked (or
  missing) row's write is retried once after the others; a row still locked
  then is counted in `busy`. An open the cache or the lookup refuses on its
  own as busy is counted in `busy` without a retry; any other failed open, a
  product refusal whatever its reason, and a failed write are recorded in
  `failed` and logged like a walk's. A migrator without an up migration,
  and the fixed form, store nothing. Run it after a deployment that brings
  tenant migrations.

### The runtime unit

With feature `runtime`, `tenants.clone().into_unit()` is the cache's unit:
it reports ready at once, evicts the scopes idle for `evict_idle_after`
every `reap_interval`, and closes the cache when its stage drains. Register
it in `Stage::Infrastructure` with `UnitPolicy::Critical`; that stage drains
last, after the calls that hold leases. The fixed form's unit reports ready
and ends when shutdown begins. A process without the runtime calls
`close()` itself.

### Seams

- **A tenant one process at a time may own**: the factory takes the
  product's own lease (for example a `sekvent::db::LeaseStore` lease in its
  control database, see [db](db.md#hold-a-lease-exclusive-work-across-instances)) and refuses with a reason of its own when another
  process holds it; the refusal reaches the caller unchanged and is not
  cached. Routing the request to the owning process is the product's.
- **Shedding by a measure of the product's own** (memory, say): read
  `stats()` and call `evict_idle_older_than` until the measure is back under
  its limit.

## Follow changes from other processes

Several processes serve the same tenants and share only the control
database. When one of them changes a tenant (suspends it, moves its hosts),
the others must stop using what they hold of it. `invalidation` does this
with one Postgres `NOTIFY` channel on the control database: the process
that made the change sends a notification after its commit, and every
process listens.

### The channel and its messages

The product names the channel: `Channel::new("app_changes")`. The name has
the grammar of a `DatabaseName` (`^[a-z_][a-z0-9_]{0,62}$`, not `postgres`,
`template0` or `template1`); anything else is `INVALID_ARGUMENT`.

There are two payloads. `Message::parse` accepts exactly these (no leading
or trailing text, no extra field; anything else is `None`), and
`Message::encode` writes them:

| Message | Payload | Meaning |
|---|---|---|
| change | `t:<tenant>:<kind>` | the tenant's control row or hosts changed |
| hint | `e:<tenant>:<topic>:<seq>` | the product's data of the tenant moved |

- `<tenant>` is the tenant id's one text.
- `<kind>` is a `ChangeKind`, `^[a-z][a-z0-9_]{0,15}$`: `ChangeKind::STATE`
  (`state`) and `ChangeKind::HOSTS` (`hosts`) are the crate's, and
  `ChangeKind::new("plan")` makes one of the product's own.
- `<topic>` is a `Topic`, `^[A-Za-z0-9_-]{1,64}$`: a stream of the
  product's (`orders`).
- `<seq>` is decimal digits without a sign and without a leading zero
  (except `0` itself), at most `i64::MAX`.

### Sending

```rust
use sekvent::tenancy::invalidation::{ChangeKind, Channel, Sender, Topic};

let sender = Sender::new(control_pool.clone(), Channel::new("app_changes")?);
// After the commit that suspended `tenant`:
sender.change(tenant, &ChangeKind::STATE);
// After an order of `tenant` was committed:
sender.hint(tenant, &Topic::new("orders")?, order_seq);
```

`change` and `hint` never wait for the database: they put the message into
a queue and start a background task that sends it. Clones of a `Sender`
share the queue.

- At most one statement is in flight across all clones. The task takes a
  control connection first and then the whole queue, so everything queued
  while it waited goes out together in one statement (`pg_notify` with
  bound values). It then looks at the queue again and ends when it is
  empty.
- A change is kept once per tenant and kind, in the order first queued. For
  hints only the highest `seq` per tenant and topic is kept; a negative
  `seq` is ignored. Changes go out before hints.
- A failed acquire or statement drops that batch with one warning (the
  error's class and the number of messages dropped, never a payload);
  whatever is queued afterwards is still sent. A dropped batch is not
  re-sent: the listener's next successful resync repairs what it said about
  tenant rows, not its hints or product-defined change kinds
  ([What this guarantees](#listening)).
- A send task that never ran (its runtime shut down before it was polled)
  or that panicked does not stop later sends: the next call starts another.
- While a send is stalled the queue keeps one entry per distinct tenant and
  kind, and per tenant and topic; there is no limit on the number of such
  keys.
- A call made outside a tokio runtime only queues; the message goes out
  with the next round of a task already running, or with the next call made
  inside a runtime.

### Listening

```rust
use std::time::Duration;

use sekvent::tenancy::invalidation::{Channel, Listener};

let listener = Listener::new(tenants.clone(), Channel::new("app_changes")?)
    .sink(streams.clone())
    .resync_every(Duration::from_secs(60))?;
// Without the runtime: `shutdown` is a tokio_util CancellationToken.
listener.run(|| tracing::info!("listening for tenant changes"), shutdown).await?;
```

`invalidation::listen(tenants, channel, sink, ready, shutdown)` is the same
in one call. The listener needs the keyed form of `Tenants`; a fixed
`Tenants` has nothing to listen for and is refused with
`FAILED_PRECONDITION` before any I/O.

On start the listener runs `LISTEN` on a connection of the control pool and
loads the active tenants once, which fills the host snapshot. That first
load is the baseline the resyncs compare with; it tells the sink nothing
and invalidates nothing. Only then is `ready` called, once: a notification
sent after `ready` is heard. A failure before that (the `LISTEN`, the first
load) is `run`'s result, and `ready` is not called.

**Start the listener before the cache serves requests.** Since the first
load retires nothing, a scope the cache already holds when the listener
starts is never compared with anything: a change to its row before the
baseline was read goes unnoticed until another change of that tenant.

Then, until `shutdown` fires (`run` returns `Ok(())`, also while a load is
awaited):

| Event | What the crate does | What the `Sink` is told |
|---|---|---|
| a change | drops the tenant's cached lookups, reloads the active list (which refreshes the host snapshot and that tenant's entry in the baseline), then retires the tenant's scope. A failed reload still retires the scope, leaves the baseline unchanged and is not counted as a failed resync | `change(tenant, &kind)`, also when the reload failed |
| a hint | nothing | `hint(tenant, &topic, seq)` |
| every resync interval (default 60 s) | clears every cached lookup, reloads the snapshot, compares the active tenants with the baseline and retires each one that changed, joined or left; the load becomes the baseline | `resync(&changed)` after a successful load, the ids sorted, also when empty |
| a read of the channel failed without ending the listener (the listening connection was lost, or any other read error) | drops its session, takes a new connection from the pool and listens on it, then a resync as above | `resync(&changed)` when that resync succeeds, then `reconnected()`, also when it failed |

- A payload that is not a message is ignored (a `debug` event with its
  length, never its text).
- Retiring a scope takes it out of the cache at once; its pool closes in
  the background (on the cache's own task, waiting for connections still
  checked out) and never blocks the listener.
- **Resync timing.** The first periodic resync is one interval after
  `ready`; each next one is one interval after the previous resync ended,
  a reconnect's resync included. A tick that came due while something else
  was handled runs once, at once, and is never replayed, and a resync
  slower than the interval still leaves room for notifications.
  `resync_every` refuses zero; an interval longer than 100 years
  (`Duration::MAX`, say) never ticks, and the listener then resyncs only
  after a failed read of the channel.
- A resync's load has no deadline of its own: it takes as long as the
  control read takes. A load that has not returned counts neither as a
  success nor as a failure.
- When a resync's load fails, the baseline stays and a warning is logged;
  the sink is not told. **Two failed resyncs in a row empty the host
  snapshot**, so origins are refused rather than allowed from stale data; a
  successful resync resets the count. A failed reload after a change does
  not count.
- After any failed read that does not end the listener, its session is
  dropped and the channel is listened on again on a new connection from
  the pool, then comes the resync and `reconnected()` of the table above.
  A lost connection is listened on again at once. Any other failed read,
  and a failed attempt to listen again, is logged as a warning with the
  error's code (`reading the invalidation channel failed; reading again in
  1 s`) followed by a 1 s wait; the resync interval and `shutdown` still
  apply during that wait. When the control pool is closed, the listener
  ends with `UNAVAILABLE`.
- The sink's methods run on the listener's task, one at a time, in the
  order the events are handled. They must return quickly and must not panic: wake the
  product's streams and return.

**What this guarantees.** One notification drops a tenant's scope and
lookups on every listening process in the normal case. A successful resync
repairs a lost notification about what a tenant's row holds (its state, its
hosts and primary host, its role key: the fields of `TenantInfo`), whether
the notification was lost on the way or its batch was dropped by the
sender. It does not repair a lost notification of a product-defined change
kind or a lost hint: after `resync` and `reconnected` the product's
consumers catch up from their own durable data. There is no bound in
seconds: a resync succeeds only when the control database answers. The 30 s
lifetime of a cached lookup bounds only how long a cached answer is reused;
it is not a revocation guarantee: a read already in flight still answers its
caller, and a lease already held survives the invalidation until it is
dropped.

### The sink

`Sink` has four methods (`change`, `hint`, `resync`, `reconnected`), each
with a default that does nothing; `()` is a sink that ignores everything.
`Arc<T>` is a sink when `T` is (`Arc<dyn Sink>` too), every method
delegating to `T`'s, so one sink can be shared: give the listener
`.sink(shared.clone())` and keep `shared` for the parts of the product that
announce changes on this process.

```rust
use std::sync::Arc;

use sekvent::tenancy::TenantId;
use sekvent::tenancy::invalidation::{ChangeKind, Sink, Topic};

/// Wakes the order streams of a tenant.
struct OrderStreams { /* the product's waiters */ }

impl Sink for OrderStreams {
    fn change(&self, tenant: TenantId, _kind: &ChangeKind) {
        self.wake_tenant(tenant);
    }
    fn hint(&self, tenant: TenantId, topic: &Topic, seq: i64) {
        if topic.as_str() == "orders" {
            self.wake_orders(tenant, seq);
        }
    }
    fn resync(&self, changed: &[TenantId]) {
        for &tenant in changed {
            self.wake_tenant(tenant);
        }
    }
    fn reconnected(&self) {
        self.wake_all();
    }
}

let streams = Arc::new(OrderStreams::new());
```

### The listener's runtime unit

With feature `runtime`, `Listener::into_unit()` is a unit that reports
ready when the listener calls `ready` (after the first `LISTEN` and the
first load) and returns `Ok(())` when its stage drains. Its stage and policy
are the product's choice; this page recommends `Stage::Infrastructure`,
next to the tenant cache's unit, so the stages that take traffic start only
once the snapshot is filled and the baseline is read: no request opens a
scope before the listener can compare it
([its first load retires nothing](#listening)). It also recommends
`UnitPolicy::Critical`, so a listener that
ends (a closed control pool, a failure before `ready`) stops the process
instead of leaving it to serve with a snapshot and lookups nobody keeps
current:

```rust
use sekvent::runtime::{Stage, UnitPolicy};

let runtime = runtime
    .unit("tenant-cache", Stage::Infrastructure, UnitPolicy::Critical, tenants.clone().into_unit())
    .unit("invalidation", Stage::Infrastructure, UnitPolicy::Critical, listener.into_unit());
```

## Provision a tenant

Arrives with the lifecycle slice (1c).

## Suspend, resume, change hosts, purge

Arrives with the lifecycle slice (1c).

## Adopt an existing database

Arrives with the lifecycle slice (1c).

## The admin surface

Arrives with the lifecycle slice (1c).

## Call a product's admin API

Arrives with the lifecycle slice (1c).

## Configuration keys

The crate reads none. The product reads its own keys and passes the values
in:

| Setting | Given to | Default |
|---|---|---|
| the control database URL (a `Secret`) and the control pool size (at least 3) | the product's `PgPool`, then `Registry::new` | none |
| the master secrets (a `Secret`) and their context string | `RoleSecrets::parse` | none |
| the object-name prefix | `ObjectNames::new` | none |
| the tenant server: host, port, TLS, application name | the base `PgConnectOptions` of `Connector::new` (their user, password and database are never used for a tenant) | none |
| the maintenance role's connect options | `Maintenance::new`, the preflight | none |
| the id header's name and its hosts | `SelectorRules::id_header` | none |
| the path-label prefix and its hosts | `SelectorRules::path_label` | none |
| the well-known names | `SelectorRules::well_known` | none |
| label length and reserved labels | `SelectorRules::labels` (`LabelRules`) | 1 to 63, nothing reserved |
| whether tenants' own hosts select | `SelectorRules::tenant_hosts` | off |
| bypass path prefixes | `SelectorRules::bypass`, or `Tenants::fixed_with_bypass` | none |
| streaming RPC paths | `TenantResolver::new` | none |
| the CORS policy (methods, headers, max age) | the product's `Cors`, with `hosts::predicate` | the `Cors` defaults |
| the tenant schema: a static sqlx `Migrator`, never built with `ignore_missing` (with it, a database ahead of the binary is not refused) | `Tenants::builder` | none |
| the tenant cache: capacity, connections per tenant, acquire and create timeouts, idle eviction, reap interval, connection idle timeout and lifetime | `TenantsBuilder` | see [Sizing](#sizing) |
| the error domains | `TenantsBuilder::domains` (`ErrorDomains`) | `sekvent.tenancy.v1`, `sekvent.tenancy.admin.v1` |
| the channel name | `Channel::new` | none |
| the resync interval | `Listener::resync_every` (zero refused; above 100 years never ticks) | 60 s |

## Errors and reasons

`ErrorDomains` holds the two domains this crate's reasons are sent under,
both `&'static str`: `serving` (default `ErrorDomains::DEFAULT_SERVING`,
`sekvent.tenancy.v1`) and `admin` (default `ErrorDomains::DEFAULT_ADMIN`,
`sekvent.tenancy.admin.v1`); `with_serving` and `with_admin` set the
product's own. `reasons` holds every reason as a constant whose value is its
own name; reasons and their codes are wire contract, messages are not.

Serving refusals carry one of eight reasons under the serving domain;
everything else carries no domain:

| Reason | Code | When |
|---|---|---|
| `TENANT_NOT_HERE` | `NOT_FOUND` | no usable request host; a path under the path-label prefix on a host that is not a path-label host, or without a usable label; a path-label host outside the prefix; a host that selects nothing; an unknown, `purging` or `purged` tenant; any id but `TenantId::LOCAL` on the fixed form |
| `TENANT_PROVISIONING` | `UNAVAILABLE` | the tenant is `provisioning` |
| `TENANT_SUSPENDED` | `FAILED_PRECONDITION` | the tenant is `suspended` |
| `TENANT_UNAVAILABLE` | `UNAVAILABLE` | a failed control read; an open that failed for any reason but the others in this table ([How an open can end](#how-an-open-can-end)); a closed cache |
| `TENANT_BUSY` | `UNAVAILABLE` | a lookup miss without a free lookup permit, a caller that joined two reads of its key without an answer, or a full tenant cache; retry hint 1 s |
| `TENANT_SELECTOR_MISSING` | `INVALID_ARGUMENT` | an id-header host without the id header |
| `TENANT_MISMATCH` | `INVALID_ARGUMENT` | a repeated or malformed id header; an optional id header that names another tenant than the one found |
| `ORIGIN_NOT_ALLOWED` | `PERMISSION_DENIED` | an id-header request whose `Origin` is repeated, malformed or not a host of the selected tenant |

`Registry::lookup` returns `TENANT_BUSY` without a domain; only `Tenants`
adds the serving domain, to every serving reason that has none, and the
`TenantResolver` passes errors on unchanged. A refusal of the product's
scope factory that carries a reason reaches the caller unchanged, with the
product's domain or none; a factory error without a reason is
`TENANT_UNAVAILABLE`, a panic anywhere in a creation `INTERNAL` without a
reason.

| Situation | Code | Reason |
|---|---|---|
| A refused object-name prefix, label length bounds, master-secret setting or context string | `INVALID_ARGUMENT` | |
| A control pool under 3 connections; a `SelectorRules` builder refusing a host (named by position), a second host list, a non-loopback host in both lists, a prefix or a well-known name; a cache setting `TenantsBuilder::build` refuses; a channel name, change kind or topic outside its grammar; a zero resync interval; `rows::insert` or `rows::set_hosts` with a host that is not canonical, no host or a primary outside the hosts, `rows::insert` with a role-key generation outside 1 to 2147483647 | `INVALID_ARGUMENT` | |
| `rows::insert` or `rows::set_hosts`: a host, role name or database name another tenant holds | `FAILED_PRECONDITION` | `TENANT_STATE` |
| `rows::set_hosts` for an id without a row | `NOT_FOUND` | |
| `control::verify_schema`: a missing table or column; a key that is missing, not unique, invalid, or partial where it must be total or the reverse | `FAILED_PRECONDITION` | `CONTROL_SCHEMA_MISSING`, metadata `table` or `constraint` |
| A stored name or verifier outside its grammar, a role of another prefix, drift on an existing role or database, a refused drop (including SQLSTATE `42501` on its `SET ROLE` or `DROP`), a poisoned connection, a key id that is not configured or generation 0, an empty key list; a control row the crate cannot decode (the cause names the column) | `INTERNAL` | |
| A preflight finding, a server older than Postgres 16; `Listener::run` on a fixed `Tenants` | `FAILED_PRECONDITION` | |
| A refused adoption | `FAILED_PRECONDITION` | `ADOPTION_REFUSED`, metadata `check`: `missing`, `template`, `owner`, `grantee`, `owns_more`, `foreign_object` or `session` |
| The permit, connect, operation, close or preflight bound passed; `Registry::admit` found no lifecycle permit within 10 s; the listener's control pool was closed | `UNAVAILABLE` | |
| Any other database error, the listener's first `LISTEN` or first load included | as `sekvent::db::to_app_error` maps it | |

Hosts and labels answer with their own error enums (`HostError`,
`LabelError`). Cluster operations, `control::verify_schema` and the row
functions attach no domain. The admin reasons (`ADMIN_TOKEN_INVALID`,
`ADMIN_TOKEN_REPLAYED`, `TENANT_NOT_FOUND`, `TENANT_LIMIT_REACHED`) are
defined in `reasons` and not returned by anything built yet.

## Sizing

**The control pool** has at least 3 connections (`Registry::MIN_CONNECTIONS`;
`Registry::new` refuses a smaller one). With `n` connections:

| Holder | Share |
|---|---|
| Request lookups (cache misses of `Tenants::select` and of `Tenants::get`'s state check) | `max(1, n / 2)` lookup permits, taken without waiting; one connection each, for one statement |
| Lifecycle work (`Registry::admit`) | `max(1, (n - 1) / 2)` lifecycle permits; a call waits up to 10 s for one |
| The invalidation listener | one connection for as long as it runs |
| Internal reads by id without a permit (the two reads inside each creation, `Registry::tenant` and `tenant_now`; the product's own `Registry::tenant` calls), the sender, snapshot reloads, the walks' list reads, the migration sweep's writes, the product's own statements | one connection each, for one statement |

**The tenant cache** holds at most `capacity` tenants, warm and being opened
together, each with a pool of at most `max_connections`. Its defaults are
those of `sekvent::db`'s keyed pools
([db](db.md#keep-one-pool-per-key-keyedpools)):

| Setting | Default |
|---|---|
| `capacity` | 64 tenants |
| `max_connections` | 5 per tenant |
| `acquire_timeout` | 5 s |
| `create_timeout` | 30 s: the one deadline of an open (connect, migration, build); raise it for a slow build and keep `acquire_timeout` below it |
| `evict_idle_after` | 10 min |
| `reap_interval` | 30 s |
| `connection_idle_timeout` | 10 min |
| `connection_max_lifetime` | 30 min |

The database server must allow `capacity × max_connections` connections,
plus those of scopes still retiring (evicted or invalidated while a lease
held them; `stats().retiring` counts them and `capacity` does not bound
them), plus the control pool, the maintenance sessions and headroom, below
its own `max_connections`. A tenant role's connection limit (the
`connection_limit` the product passes to `roles::create`) is enforced by
the server across every process that serves the tenant; the crate does not
check it against anything. As arithmetic: `p` processes, each with a tenant
pool of at most `max_connections`, can hold up to `p × max_connections`
connections of that role at once, so a lower limit makes connects fail
once it is reached.

**The sender's queue** has no fixed limit: while a send is stalled (the
control pool exhausted, a statement hanging) it keeps one entry per
distinct tenant and change kind and per tenant and topic, however many
there are.

## Security rules

- No credential is stored: tenant role passwords are derived from the
  product's master secrets when needed, and the database receives only a
  SCRAM verifier.
- The crate logs and formats nothing secret: no statement text, verifier,
  password, master secret or connect options. Master secrets, passwords
  and verifiers live in `Secret`; `ScramVerifier`, `Connector`,
  `Maintenance`, `MaintenanceConn` and `MaintenancePool` print only their
  type name in `Debug`, `RoleSecrets` only its number of keys; refusals
  name positions, labels and checks, never a key id, a secret, a URL or
  connect options. The connect options the connector returns carry the
  derived password; the product passes them to the driver and never
  formats or logs them.
- Maintenance sessions turn the driver's statement logging off. The
  database server's own logging is the operator's configuration: role
  statements carry the verifier in their text, so keep the server from
  recording them (`log_statement`, `log_min_duration_statement` and
  `log_min_error_statement`, which by default records the text of a
  statement that fails).
- Utility statements are built only from checked names (`TenantRole`,
  `DatabaseName`) and a checked `ScramVerifier`; every other value is a
  bound parameter of a static catalog query.
- Existing objects are checked, never repaired: a role or database with
  drift is refused and left as it was.
- A database is dropped only as the tenant's role, and only when that role
  owns it.
- The maintenance role is a non-superuser with `CREATEDB` and `CREATEROLE`
  that owns no tenant database and has no members; it holds tenant roles
  without inheriting their privileges and has no `CONNECT` on their
  databases.
- Tenant connections never carry the base options' credentials, and the
  preflight passes only while no tenant role can reach the service's own
  databases.
- A tenant pool connects as the tenant's derived role with a password
  derived from the master secret, never as a role the control row merely
  names: the stored role must equal the derived one and the stored cluster
  must be `main`. The tenant's migrations run as the tenant's role.
- A creation never caches a tenant that is not `active` and hands its pool
  to the factory only when the committed row still names the database, role
  and role key it was opened with; `get` checks the state before it hands
  out a warm scope, and the state is checked before an id header or an
  origin is compared.
- The tenant migrator is never built with `ignore_missing`, so a database
  that holds a migration the binary does not know is refused.
- The request host is the URI authority and the one `Host` header, which
  must agree; forwarded-host headers are never read. Hosts and origins are
  compared whole, in canonical form, never by suffix or substring.
- The path-label prefix is a reserved namespace on every host, and a bypass
  path gets an empty scope in every form, so no tenant resource is opened
  for it.
- CORS shapes response headers only and never replaces the resolver's
  origin check, which follows id-header selections only; on a loopback host
  listed for both selectors the predicate answers for path-label requests
  too, which have no origin check. When the control database stops answering, two failed
  resyncs in a row empty the host snapshot, so origins are refused rather
  than allowed from stale data.
- No error or log event carries a host, an origin or a header value a
  caller sent, a notification payload, or a connection detail. A failed
  open is logged with a fixed stage text and at most a SQLSTATE; the
  factory's own error source is dropped. `Registry`, `Tenants`,
  `TenantsBuilder`, `TenantResolver`, `Listener` and `Sender` print only
  their type name or counts in `Debug`, `TenantInfo` its id, subdomain and
  state, `TenantLease` its id.
- The cached lookup lifetime (30 s) bounds how long a cached answer is
  reused, not how long a revoked tenant stays reachable: a held lease
  survives an invalidation, and a read in flight still answers its caller.
  Changes take effect through the invalidation channel; a lost
  notification about a tenant's row is repaired by the next successful
  resync, a lost hint or product-defined change kind is not.

## Testing

Pure parts (names, hosts, labels, the derivation, the verifier, the
preflight and privilege rules, the selection order, the lookup cache's
rules on a manual clock, the message codec, the listener's loop on a paused
clock) are unit-tested with fixed vectors. Tests against a server are
`#[ignore]`d and run with `SEKVENT_DOCKER_TESTS=1` and `--include-ignored`;
they use `sekvent_testing::PostgresHarness` (feature `postgres`,
[testing](testing.md)), one Postgres 17 container per test binary. The
crate's fixture creates, as the harness superuser, a control database and a
maintenance role that is not a superuser (`LOGIN CREATEDB CREATEROLE`,
owning no database, with `CONNECT` on a maintenance database of another
owner), and revokes `PUBLIC`'s privileges on both databases; a product's
own tests set up the same shape. Each scenario runs under a 30 s guard, and
sessions are observed with `await_until!`, never with sleeps.

For the serving half a product's tests:

- apply `control::REFERENCE_SCHEMA` (or the product's own control
  migration) to the control database as its owner, then build the
  `Registry` on a pool of at least 3 connections and a
  `sekvent::context::ManualClock` when a test moves time past a lookup's
  lifetime;
- create a tenant with `roles::create` and `databases::create`, record its
  row with `control::rows::insert` in a transaction and set it `active`
  with `rows::set_state`;
- use a test product: a small tenant migrator (`sqlx::migrate!`), a scope
  type that derives nothing and a `ScopeFactory` whose build a test can
  hold at a gate, fail and count, with `tokio::sync::watch` signals so a
  test waits for a build to arrive instead of sleeping;
- serve `Tenants::fixed(scope)` where a test needs no database at all.

## See also

- [db](db.md): `KeyedPools` and `ConnectCandidates`, which the tenant
  cache and the connector build on.
- [components](components.md): the request scope and the end-user
  authenticator.
- [server](server.md#configure-cors): `Cors` and `Cors::origin_predicate`.
- [runtime](runtime.md): units, stages and policies for the tenant cache and
  the listener.
- [design/tenancy.md](../design/tenancy.md): the decisions and the slices.
