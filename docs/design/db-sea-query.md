# sea-query execution and embedded migrations (`sekvent-db`)

> Status: implemented. The user guide is
> [modules/db.md](../modules/db.md#build-queries-with-sea-query).

## 1. Problem

Some services want typed query building without an ORM: sea-query for
every statement (typed identifiers, bound values, no SQL in strings), sqlx
to run it, sqlx's versioned migrator with the migrations compiled into the
binary, all on tokio. sekvent supported sqlx and sea-orm, but not sea-query
on its own: each service would wire the binder, the error mapping and the
migrator by hand, and might pick a binder with a second TLS stack.

## 2. Scope

### In

- Features `sea-query-postgres` / `sea-query-mysql` (facade
  `db-sea-query-postgres` / `db-sea-query-mysql`), re-exports
  `sekvent_db::sea_query` and `sekvent_db::sea_query_sqlx`.
- `sekvent_db::query`: `build`, `execute`, `fetch_one`, `fetch_optional`,
  `fetch_all`, generic over sqlx executors.
- `constraint_name` and `sqlstate` for precise violation mapping.
- `run_embedded_migrations` and `PoolSpec::with_embedded_migrations`, so
  `PoolRegistry::migrate_all` runs directory and embedded migrations alike.

### Out

- Pagination or cursor helpers, transaction helpers, a repository layer.
- Replacing sea-orm support: it stays as it is for its consumers.
- SQLite.

## 3. Decisions

### 3.1 Pins and TLS

sea-query `1.0.2` and the sqlx 0.9 binder `sea-query-sqlx` `0.9.1` (not the
older `sea-query-binder`) are pinned in the root `[workspace.dependencies]`
with `default-features = false`. They were already in the lock file through
sea-orm, so the graph gains no new crate versions. The binder gets only
backend (`sqlx-postgres`, `sqlx-mysql`, chosen by sekvent-db's features) and
value features (`with-chrono`, `with-json`, `with-uuid`,
`with-rust_decimal`, `postgres-array`), never a runtime or TLS feature:
sqlx's own pin already selects tokio and rustls with ring. Because the pins
live in the root manifest, `cargo sekvent deps check/sync` and `new` hand
the same versions to consumers.

### 3.2 Feature naming

`sea-query-<backend>` mirrors `sea-orm-<backend>` and `sqlx-<backend>`; each
implies the sqlx backend of the same name, so a service turns on one
feature. The facade prefixes `db-` as for every sekvent-db feature.

### 3.3 Free functions over executors, sqlx errors out

`query::fetch_one(executor, &statement)` takes any `sqlx::Executor`
(`&pool`, `&mut *conn`, `&mut *tx`), so transactions stay explicit as the
crate requires. The SQL dialect follows the executor's database through a
small `QueryBackend` trait (`Postgres` → `PostgresQueryBuilder`, `MySql` →
`MysqlQueryBuilder`), so callers never pass a builder and cannot pair the
wrong one with a pool. The rendered SQL is owned and handed to sqlx as
`AssertSqlSafe`: sea-query produced it with placeholders, every value is in
`SqlxValues`.

The functions return `sqlx::Error`, not `AppError`: a caller often has to
inspect the error first (which constraint failed) and then maps the rest
with the existing `IntoAppError`. Returning `AppError` would hide the
constraint; a second error type would duplicate the classification.

### 3.4 Constraint names

The generic mapping (unique → `ALREADY_EXISTS`, …) cannot tell two unique
keys apart. `constraint_name(&sqlx::Error)` exposes the name Postgres
reports so the application can match its own named constraints and attach a
precise reason. MySQL does not report it; the function returns `None`
there. `sqlstate` returns the SQLSTATE for the same purpose. Neither is
ever put in a message by sekvent.

### 3.5 Embedded migrations

`sqlx::migrate!` produces a `static Migrator`; `run_embedded_migrations`
runs it and maps failures to `DbError::Migrate { target: "embedded
migrations", source }`, as `migrate_on_boot` does for a directory. The
spec keeps the `&'static Migrator` next to the optional directory; setting
both is an `InvalidSetting`. The registry stores one migration source per
pool and `migrate_all` renames the target to the pool name for both. The
migrator already takes a Postgres advisory lock and runs each migration in
a transaction, so nothing is added around it.

### 3.6 Macros resolve their own paths

`#[derive(Iden)]` expands to `sea_query::Iden`; `sqlx::migrate!` and
`#[derive(sqlx::FromRow)]` to `::sqlx::…`. Re-exports cannot redirect those
paths, so a consumer using the macros depends on `sea-query` and `sqlx`
directly, at the pinned versions. This is documented rather than worked
around. sqlx's `query!` macros are discouraged: they need a database or
offline data at compile time.

## 4. Tests

Unit tests cover placeholder rendering per dialect and the accessors on a
fake database error. Container tests (`tests/sea_query.rs`, Postgres and
MySQL) cover insert, select and update with `RETURNING` with JSONB, UUID,
timestamps and arrays (`= ANY`), use on a pool, a connection and a
transaction (commit and rollback), unique / foreign-key / check violations
with their names and codes, an upsert, embedded migrations applied twice, a
broken embedded migration, and `migrate_all` with embedded specs.
