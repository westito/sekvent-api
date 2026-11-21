# Tenants on a database each (`sekvent::tenancy`)

`sekvent::tenancy` is for a product that serves many tenants from one
process and gives each tenant a Postgres database and a database role of its
own. It names those roles and databases so they can be put into DDL safely,
derives each role's password from a master secret instead of storing it,
creates, checks and drops roles and databases through a maintenance role
that is not a superuser, hands an existing database over to a tenant role,
and checks at startup that no tenant role can reach a database that is not
its own, the service's own databases included.

The request path (picking the tenant from the request head, opening and
caching its pool, following changes made on other processes) arrives with
the serving slice (1b); provisioning, suspend, resume, purge, the admin
surface and the manager kit arrive with the lifecycle slice (1c).

The crate reads no configuration keys and runs on Postgres 16 or later.

## Enable it

| | |
|---|---|
| Facade feature | `tenancy`; `tenancy-lifecycle` for the maintenance connection, roles, databases and the handover; `tenancy-admin` and `tenancy-manager` (slice 1c) |
| Module | `use sekvent::tenancy::…;` |
| Internal crate | `sekvent-tenancy` (features `serving` (default), `lifecycle`, `admin`, `manager`, `runtime`) |
| Test container | `sekvent-testing` feature `postgres` ([testing](testing.md)) |

| Facade feature | Crate feature | Adds |
|---|---|---|
| `tenancy` | `serving` | Tenant ids, object names, hosts and labels, role secrets, the connector, privilege inspection and the startup preflight |
| `tenancy-lifecycle` | `lifecycle` (with `serving`) | The maintenance connection, roles, databases, the SCRAM verifier, the ownership handover |
| `tenancy-admin` | `admin` (with `serving`) | The admin token verifier, authenticator and guard (slice 1c) |
| `tenancy-manager` | `manager` | The token minter and the admin client, without a database (slice 1c) |
| `runtime` (default) | `runtime` (with `serving`) | The tenant cache and the invalidation listener as runtime units (slice 1b) |

`tenancy-manager` keeps sqlx and `sekvent-db` out of the build only when the
facade is used with `default-features = false`: the facade's default
`runtime` feature forwards to the crate's `runtime` feature, which turns on
`serving`.

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["tenancy-lifecycle"] }

[dev-dependencies]
sekvent-testing = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["postgres"] }
```

## What you bring

Arrives with the serving slice (1b).

## Quick example

Arrives with the serving slice (1b).

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

Arrives with the serving slice (1b).

## Lookups and their admission

Arrives with the serving slice (1b).

## Select the tenant

Arrives with the serving slice (1b).

## CORS

Arrives with the serving slice (1b).

## The tenant cache

Arrives with the serving slice (1b).

## Follow changes from other processes

Arrives with the serving slice (1b).

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

The crate reads none. The table of what the product reads itself arrives
with the serving slice (1b).

## Errors and reasons

`ErrorDomains` holds the two domains this crate's reasons are sent under,
both `&'static str`: `serving` (default `ErrorDomains::DEFAULT_SERVING`,
`sekvent.tenancy.v1`) and `admin` (default `ErrorDomains::DEFAULT_ADMIN`,
`sekvent.tenancy.admin.v1`); `with_serving` and `with_admin` set the
product's own. `reasons` holds every reason as a constant whose value is its
own name; reasons and their codes are wire contract, messages are not.

What the parts described above answer:

| Situation | Code | Reason |
|---|---|---|
| A refused object-name prefix, label length bounds, master-secret setting or context string | `INVALID_ARGUMENT` | |
| A stored name or verifier outside its grammar, a role of another prefix, drift on an existing role or database, a refused drop (including SQLSTATE `42501` on its `SET ROLE` or `DROP`), a poisoned connection, a key id that is not configured or generation 0, an empty key list | `INTERNAL` | |
| A preflight finding, a server older than Postgres 16 | `FAILED_PRECONDITION` | |
| A refused adoption | `FAILED_PRECONDITION` | `ADOPTION_REFUSED`, metadata `check`: `missing`, `template`, `owner`, `grantee`, `owns_more`, `foreign_object` or `session` |
| The permit, connect, operation, close or preflight bound passed | `UNAVAILABLE` | |
| Any other database error | as `sekvent::db::to_app_error` maps it | |

Hosts and labels answer with their own error enums (`HostError`,
`LabelError`). Cluster operations attach no domain. The serving and admin
reasons (`TENANT_NOT_HERE`, `ADMIN_TOKEN_INVALID` and the rest of
`reasons`) are returned from the serving slice (1b) on.

## Sizing

Arrives with the serving slice (1b).

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
## Testing

Pure parts (names, hosts, labels, the derivation, the verifier, the
preflight and privilege rules) are unit-tested with fixed vectors. Tests
against a server are `#[ignore]`d and run with `SEKVENT_DOCKER_TESTS=1` and
`--include-ignored`; they use `sekvent_testing::PostgresHarness` (feature
`postgres`, [testing](testing.md)), one Postgres 17 container per test
binary. The crate's fixture creates, as the harness superuser, a control
database and a maintenance role that is not a superuser (`LOGIN CREATEDB
CREATEROLE`, owning no database, with `CONNECT` on a maintenance database
of another owner), and revokes `PUBLIC`'s privileges on both databases; a
product's own tests set up the same shape. Each scenario runs under a 30 s
guard, and sessions are observed with `await_until!`, never with sleeps.

## See also

- [db](db.md): `KeyedPools` and `ConnectCandidates`, which the tenant
  cache and the connector build on.
- [components](components.md): the request scope and the end-user
  authenticator.
- [design/tenancy.md](../design/tenancy.md): the decisions and the slices.
