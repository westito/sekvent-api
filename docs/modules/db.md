# Databases (`sekvent::db`)

`sekvent::db` gives a service its database plumbing without hiding SQL:
named connection pools read from configuration, a startup check that two
pools never point at the same database, migrations on boot, one mapping from
driver errors to `AppError` codes, allow-listed list filters for sea-orm,
readiness probes per pool, and database leases with fencing tokens for work
that must run on exactly one instance. It works with sqlx (Postgres and
MySQL) and, on top of the same pools, sea-orm. Database URLs are `Secret`s
and never appear in logs or errors; a failed connect is reported as the pool
name plus a failure class.

## Enable it

| | |
|---|---|
| Facade feature | `db` (pulls in nothing heavy on its own) |
| Module | `use sekvent::db::…;` |
| Internal crate | `sekvent-db` |

Pick a backend and the extras you need:

| Facade feature | `sekvent-db` feature | Adds |
|---|---|---|
| `db` | — | `PoolSpec`, `assert_distinct_targets`, `parse_target`, `Target`, `DbError`, `ConnectFailure`, `public_message` |
| `db-sqlx-postgres` | `sqlx-postgres` | `PoolRegistry`, `Pool::Postgres`, `classify`, `classify_connect`, `to_app_error`, `IntoAppError`, `reasons` |
| `db-sqlx-mysql` | `sqlx-mysql` | the same for MySQL and MariaDB (`Pool::MySql`) |
| `db-sea-orm-postgres` | `sea-orm-postgres` | `Pool::sea_orm`, `classify_db_err`, `db_err_to_app_error`, `sekvent::db::list` (implies `db-sqlx-postgres`) |
| `db-sea-orm-mysql` | `sea-orm-mysql` | the same over MySQL pools (implies `db-sqlx-mysql`) |
| `db-migrate` | `migrate` | `migrate_on_boot`, `PoolRegistry::migrate_all` |
| `db-sea-orm-migrate` | `sea-orm-migrate` | `run_sea_orm_migrations` (combine with a `db-sea-orm-*` backend) |
| `db-lease` | `lease` | `LeaseStore`, `Lease`, `HeldLease`, `FencingToken`, `LeaseInfo` (needs a sqlx backend) |
| `db` + `runtime` | `runtime` | `PoolProbe`, `PoolRegistry::probes` (needs a sqlx backend) |
| `db-lease` + `runtime` | `lease` + `runtime` | `LeaseGuard` for singleton jobs |

`runtime` is a default facade feature, so probes and `LeaseGuard` are there
as soon as a sqlx backend (and, for the guard, `db-lease`) is on.

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = [
    "db-sqlx-postgres",
    "db-migrate",
    "db-lease",
] }
```

`sekvent-db` also has a `serde` feature (`Serialize`/`Deserialize` for
`ListParams` and `ColumnFilter`, camelCase) that the facade does not
forward. To use it, add the internal crate next to the facade from the same
source so Cargo unifies the features:

```toml
sekvent-db = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["serde"] }
```

## Quick example

```rust
use sekvent::config::EnvSource;
use sekvent::db::{IntoAppError, PoolRegistry, PoolSpec, assert_distinct_targets};
use sekvent::error::AppError;

// Startup: read, check and connect every pool (anyhow for brevity).
async fn pools() -> anyhow::Result<PoolRegistry> {
    let orders = PoolSpec::from_config(&EnvSource, "ORDERS_DB_")?;    // pool "orders_db"
    let audit = PoolSpec::from_config(&EnvSource, "AUDIT_DB_")?;      // pool "audit_db"
    assert_distinct_targets(&[("orders_db", &orders.url), ("audit_db", &audit.url)])?;
    let registry = PoolRegistry::build(vec![orders, audit]).await?;  // also runs the distinct check
    registry.migrate_all().await?;                                    // ORDERS_DB_MIGRATIONS etc., feature db-migrate
    Ok(registry)
}

// A handler: errors become generic AppErrors with the right code.
async fn count_orders(pg: &sqlx::PgPool) -> Result<i64, AppError> {
    sqlx::query_scalar("SELECT count(*) FROM orders")
        .fetch_one(pg)
        .await
        .into_app_error()
}
```

`registry.get("orders_db")?.postgres()` returns the `sqlx::PgPool`
(`Option<&PgPool>`; `None` when the pool is MySQL). `?` on a `DbError`
inside an `AppError` function works: `From<DbError> for AppError` exists.

## Concepts

- **Pool spec.** A `PoolSpec` says how to build one named pool: URL (a
  `Secret`), connection limits, timeouts, laziness, whether it is required,
  session settings and an optional migrations directory. `from_config`
  reads it from keys under a prefix; the pool name is the prefix in lower
  case without the trailing `_` (`BILLING_DB_` → `billing_db`, an empty
  prefix → `default`).
- **Registry.** `PoolRegistry::build` connects every configured pool once at
  startup and fails closed, naming the pool, when anything is wrong. The
  backend is chosen from the URL scheme (`postgres`/`postgresql`,
  `mysql`/`mariadb`).
- **Required vs optional.** A required pool without a URL is a startup
  error. An optional one (`<P>REQUIRED=false`) with a blank URL is recorded
  as *not configured*: `get` reports that distinctly, and it gets no probe.
- **Distinct targets.** Two roles of one service (orders and audit, writer
  and reporting) must not silently share a database because a URL was
  copy-pasted. The registry compares the parsed host, port, database and
  backend of every configured pool.
- **Generic errors.** Database failures become `AppError`s whose code is
  derived from the driver error (unique violation → `ALREADY_EXISTS`,
  deadlock → `ABORTED`, …) and whose message is a fixed generic text. The
  driver error is kept only as the internal source; SQL, table and column
  names never reach a caller.
- **Explicit transactions.** There is no transaction helper. Call
  `pool.begin()` and pass the transaction to the code that needs it, so a
  business write and, say, an outbox row commit or roll back together.

## How to …

### Declare pools from configuration

```rust
pub fn from_config(source: &dyn ConfigSource, prefix: &str) -> Result<PoolSpec, ConfigError>;
pub fn config_keys(prefix: &str) -> Vec<String>;   // every key read, for unknown-key checks
```

All problems are reported together, naming the keys, never the URL: a
single problem comes back as that `ConfigError` itself, two or more as
`ConfigError::Multiple`. See [Configuration keys](#configuration-keys) for the
full list.

Build specs in code instead (tests, custom setups):

```rust
use std::time::Duration;
use sekvent::config::Secret;
use sekvent::db::PoolSpec;

let reports = PoolSpec::new("reports_db", Secret::new(url))  // required, eager, defaults below
    .optional()                                                 // blank URL = not configured
    .lazy()                                                     // no connection at startup
    .with_connections(1, 5)                                     // min, max
    .with_acquire_timeout(Duration::from_secs(2))
    .with_lifetimes(Some(Duration::from_secs(300)), None)       // idle timeout, max lifetime; None keeps
    .with_search_path("reports, public")                        // Postgres only
    .with_role("reports_ro")                                    // SET ROLE on every connection
    .read_only()                                                // default read-only transactions
    .with_migrations("migrations/reports");
reports.validate().map_err(anyhow::Error::msg)?;               // also done by the registry
```

`PoolSpec` is `#[non_exhaustive]` with public fields (`name`, `url`,
`max_connections`, `min_connections`, `acquire_timeout`, `idle_timeout`,
`max_lifetime`, `lazy`, `required`, `search_path`, `role`, `read_only`,
`migrations`); `is_configured()` says whether the URL is non-blank. Its
`Debug` prints the URL as `[redacted]`.

`validate` rejects `max_connections == 0`, `min > max`, and a role or
search path that is not a plain identifier (an ASCII letter or `_`, then
letters, digits, `_` or `$`, at most 63 bytes; search path entries
comma-separated). On Postgres, search path, role and read-only are sent as
startup parameters (`search_path`, `role`, `default_transaction_read_only`).
On MySQL, role and read-only run as `SET ROLE` and
`SET SESSION TRANSACTION READ ONLY` after every connect, and a search path is
an `InvalidSetting` error: put the database in the MySQL URL. Read-only is a
hint for replicas, not a security boundary; use grants for that.

### Build and use the registry

```rust
impl PoolRegistry {
    pub async fn build(specs: Vec<PoolSpec>) -> Result<Self, DbError>;
    pub fn insert(&mut self, name: impl Into<String>, pool: Pool);   // a ready pool, registered as required
    pub fn get(&self, name: &str) -> Result<&Pool, DbError>;         // UnknownPool or NotConfigured
    pub fn get_optional(&self, name: &str) -> Option<&Pool>;
    pub fn is_unconfigured(&self, name: &str) -> bool;
    pub fn names(&self) -> impl Iterator<Item = &str>;              // connected pools, sorted
    pub fn migrations_dir(&self, name: &str) -> Option<&Path>;
    pub async fn close(&self);
    pub async fn migrate_all(&self) -> Result<(), DbError>;          // feature db-migrate
    pub fn probes(&self) -> Vec<PoolProbe>;                          // feature runtime
}
```

`build` fails, naming the pool, when two specs share a name
(`DuplicatePool`), two configured pools point at the same database
(`SameTarget`), a required pool has no URL (`NotConfigured`), a setting is
invalid (`InvalidSetting`), the URL has no scheme (`BadUrl`) or names a
backend this build lacks (`UnsupportedScheme`), or an eager pool cannot
connect (`Connect { pool, reason }`). A lazy pool opens no connection in
`build`.

`Pool` is a `#[non_exhaustive]` enum, cheap to clone:

```rust
pub enum Pool {
    Postgres(sqlx::PgPool),   // feature db-sqlx-postgres
    MySql(sqlx::MySqlPool),   // feature db-sqlx-mysql
}
impl Pool {
    pub fn postgres(&self) -> Option<&sqlx::PgPool>;
    pub fn mysql(&self) -> Option<&sqlx::MySqlPool>;
    pub fn sea_orm(&self) -> Option<sea_orm::DatabaseConnection>;   // db-sea-orm-*
    pub async fn close(&self);
}
```

`sea_orm()` wraps the same sqlx pool (no new connections); it returns
`None` when sea-orm support for that backend is not compiled in. Close the
registry on shutdown with `registry.close().await`.

### Check that pools point at different databases

```rust
pub fn assert_distinct_targets(pools: &[(&str, &Secret)]) -> Result<(), DbError>;
pub fn parse_target(url: &str) -> Result<Target, &'static str>;

pub struct Target { pub scheme: String, pub host: String, pub port: u16, pub database: String }
```

`PoolRegistry::build` already runs `assert_distinct_targets` over its specs;
call it yourself when pools are built elsewhere or to fail earlier. Blank
URLs are skipped. Two pools collide when scheme, host, port and database are
all equal:

- `postgresql` normalises to `postgres`, `mariadb` to `mysql`;
- default ports (5432, 3306) are filled in;
- the host is lower-cased, a trailing dot dropped, and `localhost`,
  `127.0.0.1`, `::1` (and an empty host) are the same host; a Unix socket
  path is kept as written;
- user, host and database are percent-decoded; for Postgres an empty
  database means the user name;
- query parameters that move the connection count: Postgres `host`,
  `hostaddr`, `port`, `dbname`, `user`; MySQL `socket`.

With a sqlx backend compiled in, the driver's own URL parser decides host,
port, user and database, including its environment defaults (`PGHOST`,
`PGPORT`, …), so the identity is where the pool really connects. A URL
either parser rejects is a `BadUrl` error naming the pool: the check fails
closed. Errors name both pools (`SameTarget { first, second }`) and never
print a URL. Credentials are not part of the identity: two users on one
database still collide.

### Run migrations on boot

```rust
pub async fn migrate_on_boot<DB>(pool: &sqlx::Pool<DB>, dir: &Path) -> Result<(), DbError>;   // db-migrate
pub async fn run_sea_orm_migrations<M: MigratorTrait>(db: &DatabaseConnection) -> Result<(), DbError>; // db-sea-orm-migrate
```

- Per pool from configuration: set `<P>MIGRATIONS=migrations/orders` (or
  `PoolSpec::with_migrations`) and call `registry.migrate_all().await?` once
  at startup, before serving. Pools run in name order; already applied
  migrations are skipped (sqlx's migrator); an unconfigured optional pool is
  skipped.
- By hand: `migrate_on_boot(pg, Path::new("migrations"))`.
- sea-orm: `run_sea_orm_migrations::<Migrator>(&db).await?` applies every
  pending migration of your `MigratorTrait` type.

A failure is `DbError::Migrate { target, source }`: `target` is the pool
name (from `migrate_all`), the directory (from `migrate_on_boot`) or
`sea-orm migrator`; the migrator's error is the source, for logs only.

### Map database errors to `AppError`

```rust
pub fn classify(error: &sqlx::Error) -> ErrorCode;
pub fn to_app_error(error: sqlx::Error) -> AppError;
pub fn classify_connect(error: &sqlx::Error) -> ConnectFailure;
pub fn classify_db_err(error: &sea_orm::DbErr) -> ErrorCode;     // db-sea-orm-*
pub fn db_err_to_app_error(error: sea_orm::DbErr) -> AppError;  // db-sea-orm-*
pub fn public_message(code: ErrorCode) -> &'static str;
pub trait IntoAppError<T> { fn into_app_error(self) -> Result<T, AppError>; }
```

`IntoAppError` is implemented for `Result<T, sqlx::Error>` and
`Result<T, sea_orm::DbErr>`, so `query.await.into_app_error()?` is the usual
form; `.map_err(sekvent::db::to_app_error)` is equivalent.

| Driver error | Code | Reason | Message |
|---|---|---|---|
| I/O, TLS, pool timeout, pool closed, crashed worker | `UNAVAILABLE` | — | `database unavailable` |
| SQLSTATE class `08` (connection), `57P01`–`57P03` (shutdown), `53300` (too many connections) | `UNAVAILABLE` | — | `database unavailable` |
| unique violation | `ALREADY_EXISTS` | — | `already exists` |
| foreign-key violation | `FAILED_PRECONDITION` | — | `a related record is missing or still in use` |
| serialization failure `40001`, deadlock `40P01` | `ABORTED` | — | `conflicting concurrent update; retry` |
| missing grant: MySQL errno 1044, 1142, 1143, 1227, 1370; SQLSTATE `42501` | `PERMISSION_DENIED` | `DB_PERMISSION_DENIED` | `permission denied` |
| rejected credentials: MySQL errno 1045; SQLSTATE `28000`, `28P01` | `INTERNAL` | `DB_CREDENTIALS_REJECTED` | `internal error` |
| `RowNotFound` | `NOT_FOUND` | — | `not found` |
| anything else | `INTERNAL` | — | `internal error` |

Access refusals are decided first, before the error kind or SQLSTATE class.
The distinction matters: a missing grant is about one statement, while
rejected credentials mean the service itself is misconfigured, so the code
is `INTERNAL` (not retryable, not the caller's fault). SQLSTATE `42000` is
deliberately not mapped, since MySQL also uses it for syntax errors.

sea-orm errors that wrap a sqlx error get the same code and reason; a
wrapped connection error that would be `INTERNAL` becomes `UNAVAILABLE`
unless the server rejected the credentials. Other sea-orm errors:
`ConnectionAcquire`/`Conn` → `UNAVAILABLE`, `RecordNotFound` and
`RecordNotUpdated` → `NOT_FOUND`, unique/foreign-key `SqlErr` →
`ALREADY_EXISTS`/`FAILED_PRECONDITION`, the rest `INTERNAL`.

`DbError` (startup) converts into `AppError` too: `Connect` and
`NotConfigured` are `UNAVAILABLE`, everything else `INTERNAL`, always with
the generic message and the `DbError` as source. `DbError::code()` tells the
code without converting.

`classify_connect` gives the `ConnectFailure` class used in connect errors
and logs: `Unreachable` (I/O, TLS), `Auth` (credentials rejected, or MySQL
1044), `Timeout`, `BadUrl` (bad options) or `Other`; `as_str()` is
`unreachable`, `auth`, `timeout`, `bad-url`, `other`.

### Build list endpoints with filters, sorting and paging

`sekvent::db::list` (with a `db-sea-orm-*` feature) turns client list
parameters into a sea-orm sort and `Condition`, with server-side
allow-lists:

```rust
use sea_orm::{Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use sekvent::db::IntoAppError;
use sekvent::db::list::{ListParams, ListSpec, paginate, prefix_search};

let spec = ListSpec::<invoice::Entity>::new()
    .sortable([invoice::Column::CreatedAt, invoice::Column::Total])
    .filterable([invoice::Column::Status, invoice::Column::CreatedAt]);

let mut condition = spec.apply_filters(Condition::all(), &params.filters)?;
if let Some(term) = params.search.as_deref() {
    condition = condition.add(prefix_search::<invoice::Entity>(&[invoice::Column::Number], term));
}
let mut query = invoice::Entity::find().filter(condition);
if let Some((column, order)) = spec.sort(&params)? {
    query = query.order_by(column, order);
}
let page = paginate(params.page, params.page_size, 100)?;
let rows = query.offset(page.offset).limit(page.limit).all(&db).await.into_app_error()?;
```

| Item | Meaning |
|---|---|
| `ListParams { search, sort_by, sort_desc, page, page_size, filters }` | What a client sends. `page` is 1-based (0 is 1). |
| `ColumnFilter { field, value, value_to, op }`; `ColumnFilter::new(field, op, value)`, `ColumnFilter::range(field, from, to)` | One per-column filter; either `range` bound may be blank. |
| `ListSpec::new()`, `.sortable(cols)`, `.filterable(cols)` | Allow-lists; both start empty. |
| `spec.sort_column(field)`, `spec.sort(&params)` | The allowed column and `Order`; `None` when no field is named. |
| `spec.apply_filters(condition, &filters)` | Adds one condition per usable filter. |
| `prefix_search::<E>(columns, term)` | `col LIKE 'term%'` over server-chosen columns, OR-ed; a blank term filters nothing. |
| `paginate(page, page_size, max) -> Page { offset, limit }` | Size clamped to `1..=max`; size 0 means the maximum. |
| `to_snake_case(field)`, `start_of_day(date)` | Helpers. |

Field names may be camelCase or snake_case (`createdAt` and `created_at`
both resolve to the `created_at` column). The allow-lists are **strict**: a
sort or filter on any field that is not allowed — including one that is
not a column at all — is `INVALID_ARGUMENT` naming the field (echoed up to
64 characters) with a field violation on `sortBy` or `filters`. Never list
secret columns (password hashes, internal flags): sorting or filtering by
them leaks their contents one comparison at a time.

Inside an allowed column, an unknown operator, a blank value or a value that
does not parse for the column type is **skipped**, not rejected, so old
clients keep working:

| Column type | Operators | Value |
|---|---|---|
| text | `""` or `prefix` (`LIKE 'v%'`), `eq` | as is |
| integer | `""`/`eq`, `range`, `gte`, `lte`, `gt`, `lt` | `i64` |
| decimal, float, money | same as integer | decimal |
| date | same as integer; `range` inclusive | `YYYY-MM-DD` |
| date-time, timestamp (with or without zone) | same, by whole UTC days | `YYYY-MM-DD[…]` |
| boolean | `""`/`eq` | `1`, `0`, `true`, `false` |
| uuid | `""`/`eq` | hyphenated UUID |

Text matching is always a prefix match, never `%v%` (a leading wildcard
cannot use a b-tree index); `%` and `_` in the value are escaped and match
literally. Date-time columns compare by day as half-open ranges: `eq d` is
`[d, d+1)`, `range a..b` is `[a, b+1)`, `gte d` is `>= d`, `gt d` is
`>= d+1`, `lte d` is `< d+1`, `lt d` is `< d`. `paginate` fails with
`INVALID_ARGUMENT` (field `page`) when the offset would not fit in an `i64`.

### Report pool health in readiness

With `runtime` and a sqlx backend:

```rust
use sekvent::runtime::Runtime;

let builder = registry
    .probes()                                     // one per connected pool
    .into_iter()
    .fold(Runtime::builder(), |builder, probe| builder.probe(probe));

// by hand:
use sekvent::db::PoolProbe;
let probe = PoolProbe::new("reports_db", registry.get("reports_db")?.clone()).optional();
```

A probe is named after the pool and is required exactly when the pool's
spec was; unconfigured optional pools get none. An optional probe reports
its state without making the service unready. Each check acquires a
connection and pings it, within 1.5 s (below the runtime's default 2 s
`probe_timeout`). A lazy pool is probed like any other. When every
connection is in use, the probe reports `Up` without queueing behind the
load as long as its last successful check is under 30 s old, so readiness
does not flap on a busy service.

| Situation | Status |
|---|---|
| pool closed | `Down(Unreachable("pool closed"))` |
| credentials rejected | `Down(Rejected("credentials rejected"))` |
| missing grant (incl. MySQL "access denied to the database") | `Down(Rejected("access denied"))` |
| pool acquire timeout | `Down(Unreachable("no connection available in time"))` |
| no connection and ping within 1.5 s | `Down(Unreachable("no answer in time"))` |
| I/O, TLS, connection-class SQLSTATE | `Down(Unreachable("unreachable"))` |
| anything else | `Down(Unreachable("ping failed"))` |

Details never quote the driver or the URL. See [runtime](runtime.md) for how
probes feed `/readyz`.

### Hold a lease (exclusive work across instances)

With `db-lease` and a sqlx backend. A lease is one row per name in a table
your application owns; every acquisition increments the row's **fencing
token**. Expiry is computed by the **database's clock** (`now()` on
Postgres, `UTC_TIMESTAMP(6)` on MySQL — never the session time zone), so
instance clocks do not need to agree.

#### Create the table

The default table is `sekvent_leases` (`DEFAULT_LEASE_TABLE`). Put the DDL
in your migrations (`store.schema_sql()` prints it for the pool's backend)
or call `store.ensure_schema().await?` (needs `CREATE`). Postgres:

```sql
CREATE TABLE IF NOT EXISTS "sekvent_leases" (
    name         VARCHAR(200) PRIMARY KEY,
    owner        VARCHAR(64)  NOT NULL DEFAULT '',
    holder       VARCHAR(200) NOT NULL DEFAULT '',
    fence        BIGINT       NOT NULL DEFAULT 0,
    expires_at   TIMESTAMPTZ  NOT NULL,
    acquired_at  TIMESTAMPTZ  NULL,
    last_tick_us BIGINT       NULL
);
```

MySQL / MariaDB:

```sql
CREATE TABLE IF NOT EXISTS `sekvent_leases` (
    name         VARCHAR(200) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
    owner        VARCHAR(64)  CHARACTER SET ascii COLLATE ascii_bin NOT NULL DEFAULT '',
    holder       VARCHAR(200) CHARACTER SET ascii NOT NULL DEFAULT '',
    fence        BIGINT       NOT NULL DEFAULT 0,
    expires_at   DATETIME(6)  NOT NULL,
    acquired_at  DATETIME(6)  NULL,
    last_tick_us BIGINT       NULL
) ENGINE = InnoDB;
```

At startup, `store.verify_schema().await?` fails with `FAILED_PRECONDITION`
/ `LEASE_SCHEMA_MISSING` (metadata `table`) when the table or a column is
missing; other failures are classified like any database error.

#### The store

```rust
impl LeaseStore {
    pub fn new(pool: &Pool) -> Self;                                    // table sekvent_leases, holder "pid-<pid>"
    pub fn with_table(self, table: &str) -> Result<Self, AppError>;     // 1–63 bytes of [a-z0-9_], not starting with a digit
    pub fn with_holder(self, holder: &str) -> Result<Self, AppError>;   // label shown in info and logs: 1–200 bytes visible ASCII
    pub fn table(&self) -> &str;
    pub fn holder(&self) -> &str;
    pub fn schema_sql(&self) -> String;
    pub async fn ensure_schema(&self) -> Result<(), AppError>;
    pub async fn verify_schema(&self) -> Result<(), AppError>;
    pub async fn try_acquire(&self, name: &str, ttl: Duration) -> Result<Option<Lease>, AppError>;
    pub async fn try_acquire_tick(&self, name: &str, tick: SystemTime, ttl: Duration) -> Result<Option<Lease>, AppError>;
    pub async fn last_tick(&self, name: &str) -> Result<Option<SystemTime>, AppError>;
    pub async fn info(&self, name: &str) -> Result<Option<LeaseInfo>, AppError>;
    pub async fn check_fence_postgres(&self, conn: &mut sqlx::PgConnection, name: &str, fence: FencingToken) -> Result<(), AppError>;
    pub async fn check_fence_mysql(&self, conn: &mut sqlx::MySqlConnection, name: &str, fence: FencingToken) -> Result<(), AppError>;
}
```

`LeaseStore` is cheap to clone. Lease names are 1–200 bytes of
`[A-Za-z0-9._:/-]`; TTLs are 1 s to 24 h (`DEFAULT_LEASE_TTL` is 30 s);
ticks are at or after the Unix epoch. Anything else is `INVALID_ARGUMENT`.
The holder label is for humans (pod or host name); it is not a secret and
not what proves ownership — each acquisition generates a random owner id
that is never logged.

- `try_acquire` returns `Ok(None)` when someone else holds the name. A
  store does not re-enter: acquiring a name it already holds also returns
  `None`. Of several racing acquirers exactly one wins, also when the row
  does not exist yet.
- `try_acquire_tick` additionally refuses when `tick` (or a later one)
  already ran, and records `tick` on success — the per-tick primitive the
  job guard uses. `last_tick` reads that record.
- `info` returns `LeaseInfo { holder, fence, remaining }` for a held lease,
  `None` when free; `remaining` is by the database clock.
- Acquisition is one transaction. If its outcome is unknown (an error after
  the commit was sent, or the caller dropped the future), the row is
  released in the background by owner id, so it never blocks the name for a
  full TTL.

#### A lease, by hand or kept alive

```rust
impl Lease {
    pub fn name(&self) -> &str;
    pub fn fence(&self) -> FencingToken;
    pub fn valid_until(&self) -> tokio::time::Instant;     // TTL from when the last acquire/renew was sent, minus 10 %
    pub async fn renew(&mut self) -> Result<(), AppError>;  // ABORTED / LEASE_LOST when no longer ours
    pub async fn release(self) -> Result<(), AppError>;     // the fence stays; a no-op when no longer ours
    pub fn keep_alive(self) -> HeldLease;                   // panics outside a tokio runtime
}

impl HeldLease {
    pub fn name(&self) -> &str;
    pub fn fence(&self) -> FencingToken;
    pub fn lost(&self) -> tokio_util::sync::CancellationToken;   // fires when the lease is lost
    pub fn is_lost(&self) -> bool;
    pub async fn release(self) -> Result<(), AppError>;
}
```

A plain `Lease` dropped without `release` simply expires after its TTL.
`keep_alive` starts a background heartbeat that renews every `ttl / 3` and
retries a failed renewal after `ttl / 10` (at least 100 ms). The `lost`
token fires when a renewal finds the row taken by someone else, or when the
validity passes without a successful renewal; a lease whose validity had
already passed when `keep_alive` was called is lost from the start.
Dropping a `HeldLease` (or cancelling its `release` midway) stops the
heartbeat and releases the lease in the background inside a tokio runtime.

`FencingToken` is `Copy + Ord`; `FencingToken::new(u64)`, `.get()`, and
`Display` give the number.

#### Fence your writes

A holder can lose its lease without noticing in time (a long GC-like pause,
a network partition). Guard writes that must not come from a stale holder:

```rust
use sekvent::db::{FencingToken, IntoAppError, LeaseStore};
use sekvent::error::AppError;

async fn write_ledger(pg: &sqlx::PgPool, store: &LeaseStore, fence: FencingToken) -> Result<(), AppError> {
    let mut tx = pg.begin().await.into_app_error()?;
    store.check_fence_postgres(&mut tx, "ledger", fence).await?;   // ABORTED / LEASE_LOST after a takeover
    sqlx::query("UPDATE ledger SET closed = true WHERE day = current_date")
        .execute(&mut *tx)
        .await
        .into_app_error()?;
    tx.commit().await.into_app_error()
}
```

The check passes only while the row still carries this fence and has not
expired (by `clock_timestamp()` on Postgres, since the transaction may have
started a while ago), and it takes a **shared lock** on the row, so no new
holder can take over before the transaction ends. A failed check is
`ABORTED` / `LEASE_LOST` with metadata `lease` and `fence`. Alternatively,
store the fence in the target rows and update with
`WHERE last_fence <= $fence`.

#### Singleton jobs

With `db-lease` and `runtime`, `LeaseGuard` makes a
[job](jobs.md) run on one instance per tick:

```rust
use std::time::Duration;
use sekvent::db::{LeaseGuard, LeaseStore};
use sekvent::runtime::{JobContext, JobSpec, Runtime, Stage};

let store = LeaseStore::new(registry.get("orders_db")?).with_holder(&pod_name)?;
store.verify_schema().await?;
let guard = LeaseGuard::new(store.clone())          // lease named after the job, TTL 30 s
    .with_ttl(Duration::from_secs(60))?;            // 1 s to 24 h
let spec = JobSpec::interval(Duration::from_secs(900)).singleton(guard);
let builder = Runtime::builder().job("orders-sync", Stage::Workers, spec, move |cx: JobContext| async move {
    let fence = cx.fence().expect("singleton runs carry a fence");
    // … guarded writes with FencingToken::new(fence) …
    Ok::<(), sekvent::error::AppError>(())
});
```

A scheduled or catch-up run takes the lease for its tick
(`try_acquire_tick`), so each tick runs once across all instances; a manual
run takes it without a tick and never touches the tick record. The lease is
kept alive during the run, its fence reaches the run as `cx.fence()`, and it
is released afterwards. Losing it cancels the run
(`cx.cancel_reason() == Some(CancelReason::LeaseLost)`; the run ends
`ABORTED`). `with_lease_name("orders-sync")?` shares one lease between
several users: share it between one scheduled job and on-demand work, not
between two scheduled jobs (the tick record belongs to the row, so of two
scheduled jobs due at the same tick only one would run).

#### Recipe: on-demand exclusive work

Two shapes, for "start a full sync now, unless one is already running":

```rust
// 1. A manual job: the runtime drives the lease, heartbeat, drain and panics.
let spec = JobSpec::manual()
    .singleton(LeaseGuard::new(store.clone()).with_lease_name("orders-sync")?);
let sync = spec.handle();                         // hand the JobHandle to the RPC service
let builder = builder.job("orders-full-sync", Stage::Workers, spec, full_sync);
// In the RPC handler:
let started = sync.trigger().await.map_err(AppError::from)?;  // FAILED_PRECONDITION / JOB_HELD_ELSEWHERE while held
```

```rust
// 2. Direct: code that is not a job holds the lease itself.
use sekvent::db::DEFAULT_LEASE_TTL;
use sekvent::error::{AppError, ErrorCode};

let Some(lease) = store.try_acquire("orders-sync", DEFAULT_LEASE_TTL).await? else {
    return Err(AppError::failed_precondition("a sync is already running").with_reason("SYNC_RUNNING"));
};
let held = lease.keep_alive();
let fence = held.fence();
let lost = held.lost();                           // owned token: select! must not borrow a temporary
tokio::select! {
    result = sync_all(fence) => { held.release().await?; result }
    () = lost.cancelled() => Err(AppError::new(ErrorCode::Aborted, "the sync lost its lease")),
}
```

Both exclude each other and the scheduled `orders-sync` job when they share
the lease name.

## Configuration keys

Per pool, under the prefix passed to `PoolSpec::from_config` (`<P>`). For
`ORDERS_DB_` the pool is named `orders_db`.

| Key | Default | Meaning |
|---|---|---|
| `<P>URL` | required unless `<P>REQUIRED=false` | connection URL (`Secret`); blank = not configured |
| `<P>REQUIRED` | `true` | `false` makes the pool optional |
| `<P>MAX_CONNECTIONS` | `10` | at least 1 |
| `<P>MIN_CONNECTIONS` | `0` | at most `MAX_CONNECTIONS` |
| `<P>ACQUIRE_TIMEOUT` | `5s` | how long `acquire` waits |
| `<P>IDLE_TIMEOUT` | `10m` | `0` keeps idle connections |
| `<P>MAX_LIFETIME` | `30m` | `0` keeps connections forever |
| `<P>LAZY` | `false` | connect on first use instead of at startup |
| `<P>SEARCH_PATH` (alias `<P>SCHEMA`) | unset | Postgres `search_path`, comma-separated identifiers |
| `<P>ROLE` | unset | role set on every connection |
| `<P>READ_ONLY` | `false` | read-only transactions by default |
| `<P>MIGRATIONS` | unset | sqlx migrations directory for `migrate_all` |

Durations use the config readers' syntax (`2s`, `5m`); booleans accept the
usual spellings (`true`, `false`, `yes`, …). A required URL that is missing
is `ConfigError::Missing`, one that is blank `ConfigError::EmptySecret`.

Leases and probes have no keys: TTL, lease name and table are set in code.

## Errors and reasons

`sekvent::db::reasons`:

| Constant | Code | When |
|---|---|---|
| `DB_PERMISSION_DENIED` | `PERMISSION_DENIED` | a statement lacked a grant |
| `DB_CREDENTIALS_REJECTED` | `INTERNAL` | the server rejected the service's own credentials |
| `LEASE_LOST` | `ABORTED` | a lease is no longer held under this owner or fence (same value as `sekvent::runtime::reasons::LEASE_LOST`) |
| `LEASE_SCHEMA_MISSING` | `FAILED_PRECONDITION` | the lease table or a column is missing |

Startup errors are `DbError` variants: `UnknownPool`, `NotConfigured`,
`DuplicatePool`, `Connect { pool, reason: ConnectFailure }`,
`BadUrl { pool, reason }`, `UnsupportedScheme { pool, scheme }`,
`InvalidSetting { pool, reason }`, `SameTarget { first, second }`,
`Migrate { target, source }`. Their messages name pools and settings only.

## Testing tips

- Use [`sekvent-testing`](testing.md): one shared Postgres or MySQL
  container per test binary and a fresh database per test.
  `Pool::Postgres(sqlx::PgPool::connect(&db.url).await?)` turns a test
  database into a `Pool` for `LeaseStore` or `PoolRegistry::insert`.
- Mark database tests `#[ignore = "needs Docker; …"]` and return early
  unless `sekvent_testing::docker_tests_enabled()`.
- Run each scenario on both backends when you support both: write it as an
  `async fn(pool: &Pool)` and call it from a `postgres` and a `mysql` test
  module (the crate's own `tests/lease.rs` does this with a small macro).
- Lease and probe tests talk to a real server, so they run on the real
  clock: wait on observable state (`store.info(..)`, `held.lost()`) with
  `await_until!` or a 30 s `tokio::time::timeout`, and assert structure,
  never exact timings. To simulate a stalled or stolen lease, update
  `expires_at` or `owner` in the table directly.
- Error classification needs no server for most cases; for grant and
  credential failures, create a restricted user in the test database.

## Pitfalls and security

- **Never log URLs.** Keep them in `Secret`; never format a driver connect
  error yourself (drivers can quote URL fragments). Use the registry's
  errors, which name only the pool and a class.
- **Generic messages are deliberate.** Do not replace them with the driver
  text. Driver errors can quote values from the statement or the row, so
  keep them server-side and never in a message, reason or metadata. The
  attached source chain is logged by the serving boundary only for
  server-side codes (`UNKNOWN`, `INTERNAL`, `DATA_LOSS`); a caller error
  such as `ALREADY_EXISTS` is not logged.
- **A unique violation is `ALREADY_EXISTS`, not a bug.** Insert-if-absent
  flows can rely on it; a foreign-key violation is `FAILED_PRECONDITION`.
- **`ABORTED` is retryable** (deadlock, serialization failure, lost lease):
  retry the whole transaction, not the single statement.
- **Fenced transactions stay short.** `check_fence_*` holds a shared lock on
  the lease row until the transaction ends, which also delays the holder's
  own renewal; keep such transactions shorter than `ttl / 3`, or the
  heartbeat stalls and the lease is lost.
- **Lease rows are not secrets, owner ids are.** `holder` is a label you
  choose; never put credentials in it.
- **Read-only is not authorization.** Use database grants for that; the
  grant errors then map to `PERMISSION_DENIED`.
- **List allow-lists.** Add only columns the caller may see to `sortable`,
  `filterable` and `prefix_search`.

## See also

- [Runtime](runtime.md) — readiness probes and the staged lifecycle
- [Jobs](jobs.md) — interval, cron and manual jobs; singleton guards
- [Errors](error.md) — `AppError`, codes and wire mappings
- [Configuration](config.md) — `ConfigSource`, `Secret`, duration syntax
- [Testing](testing.md) — container harness and deterministic tests
- [Design: P8 service essentials](../design/p8-service-essentials.md) — probes and leases design
