//! The reference control schema and its startup check.

use sekvent_db::{sqlstate, to_app_error};
use sekvent_error::AppError;
use sqlx::PgPool;

use crate::reasons;

/// The reference control schema: the DDL a product copies into a migration of its own.
pub const REFERENCE_SCHEMA: &str = include_str!("reference_schema.sql");

/// Checks that the control schema has every column and named constraint this crate relies on.
/// `FAILED_PRECONDITION` / `CONTROL_SCHEMA_MISSING` (no domain) with metadata `table` (a missing table
/// or column: SQLSTATE `42P01` or `42703` on the false-predicate select) or `constraint` (a named
/// constraint or index that the catalog does not show on its table as a valid unique index: absent,
/// not unique, invalid, for example left behind by a failed `CREATE INDEX CONCURRENTLY`, or with a
/// `WHERE` where the reference schema has none, or without one where it has one). Any other failure
/// maps through `sekvent_db::to_app_error`. Extra columns and extra indexes pass. `admin_audit` and
/// its key are checked only when feature `admin` is enabled.
pub async fn verify_schema(pool: &PgPool) -> Result<(), AppError> {
    for (table, select) in TABLES {
        sqlx::query(*select)
            .execute(pool)
            .await
            .map_err(|error| table_error(table, error))?;
    }
    let names: Vec<&str> = KEYS.iter().map(|key| key.name).collect();
    let tables: Vec<&str> = KEYS.iter().map(|key| key.table).collect();
    let partial: Vec<bool> = KEYS.iter().map(|key| key.partial).collect();
    let missing: Vec<String> = sqlx::query_scalar(MISSING_KEYS)
        .bind(names)
        .bind(tables)
        .bind(partial)
        .fetch_all(pool)
        .await
        .map_err(to_app_error)?;
    match missing.first() {
        Some(name) => Err(schema_missing("constraint", name)),
        None => Ok(()),
    }
}

/// `tenants` with every column the crate reads or writes.
const TENANTS: (&str, &str) = (
    "tenants",
    "SELECT id, subdomain, display_name, state, provision_step, db_cluster, db_name, db_role, \
     db_role_key_id, db_role_generation, schema_version, created_at, updated_at \
     FROM tenants WHERE false",
);

/// `tenant_hosts` with every column the crate reads or writes.
const TENANT_HOSTS: (&str, &str) = (
    "tenant_hosts",
    "SELECT host, tenant_id, is_primary FROM tenant_hosts WHERE false",
);

/// A unique constraint or unique index the crate relies on.
#[derive(Clone, Copy)]
struct Key {
    /// The name of the constraint or index.
    name: &'static str,
    /// Its table.
    table: &'static str,
    /// Whether it is a partial unique index (with a `WHERE`); otherwise it must cover every row.
    partial: bool,
}

impl Key {
    /// A key that covers every row: a primary key or a unique constraint.
    const fn total(name: &'static str, table: &'static str) -> Self {
        Self {
            name,
            table,
            partial: false,
        }
    }

    /// A partial unique index.
    const fn partial(name: &'static str, table: &'static str) -> Self {
        Self {
            name,
            table,
            partial: true,
        }
    }
}

/// The unique constraints and indexes the crate's error mapping and reads rely on, in the order
/// they are checked.
const SERVING_KEYS: [Key; 5] = [
    Key::total("tenants_pkey", "tenants"),
    Key::total("tenants_db_role_key", "tenants"),
    Key::total("tenant_hosts_pkey", "tenant_hosts"),
    Key::partial("tenants_db_name_key", "tenants"),
    Key::partial("tenant_hosts_primary_key", "tenant_hosts"),
];

/// The tables checked, in order, each with its false-predicate select.
#[cfg(not(feature = "admin"))]
const TABLES: &[(&str, &str)] = &[TENANTS, TENANT_HOSTS];
/// The tables checked, in order, each with its false-predicate select.
#[cfg(feature = "admin")]
const TABLES: &[(&str, &str)] = &[TENANTS, TENANT_HOSTS, ADMIN_AUDIT];

/// The keys checked, in order.
#[cfg(not(feature = "admin"))]
const KEYS: &[Key] = &SERVING_KEYS;
/// The keys checked, in order.
#[cfg(feature = "admin")]
const KEYS: &[Key] = &[
    SERVING_KEYS[0],
    SERVING_KEYS[1],
    SERVING_KEYS[2],
    SERVING_KEYS[3],
    SERVING_KEYS[4],
    Key::total("admin_audit_pkey", "admin_audit"),
];

/// `admin_audit` with its six columns.
#[cfg(feature = "admin")]
const ADMIN_AUDIT: (&str, &str) = (
    "admin_audit",
    "SELECT token_id, occurred_at, operation, actor, outcome, tenant_id \
     FROM admin_audit WHERE false",
);

/// The names of `$1` that are not a valid unique index (a primary key, a unique constraint or a
/// unique index) of the table at the same position of `$2`, partial exactly when `$3` says so, in
/// the order given. Tables resolve through the session's `search_path`.
const MISSING_KEYS: &str = "SELECT k.name \
     FROM unnest($1::text[], $2::text[], $3::bool[]) \
         WITH ORDINALITY AS k(name, tbl, is_partial, ord) \
     WHERE NOT EXISTS ( \
         SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname = k.name AND i.indrelid = to_regclass(k.tbl) \
         AND i.indisunique AND i.indisvalid AND (i.indpred IS NOT NULL) = k.is_partial \
     ) \
     ORDER BY k.ord";

/// `FAILED_PRECONDITION` / `CONTROL_SCHEMA_MISSING` with `key` = `name`, no domain.
fn schema_missing(key: &'static str, name: &str) -> AppError {
    AppError::failed_precondition("the control schema is missing or incomplete")
        .with_reason(reasons::CONTROL_SCHEMA_MISSING)
        .with_metadata(key, name)
}

/// A missing table or column names `table`; any other failure maps through `to_app_error`.
fn table_error(table: &str, error: sqlx::Error) -> AppError {
    if matches!(sqlstate(&error).as_deref(), Some("42P01" | "42703")) {
        schema_missing("table", table).with_source(error)
    } else {
        to_app_error(error)
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    #[test]
    fn every_checked_name_is_in_the_reference_schema() {
        for (table, select) in TABLES {
            assert!(
                REFERENCE_SCHEMA.contains(&format!("CREATE TABLE {table} (")),
                "{table}"
            );
            let columns = select
                .strip_prefix("SELECT ")
                .and_then(|rest| rest.split(" FROM ").next())
                .expect("a select of columns");
            for column in columns.split(", ") {
                assert!(
                    REFERENCE_SCHEMA.contains(&format!("    {} ", column.trim())),
                    "{table}.{column}"
                );
            }
        }
        for key in KEYS {
            assert!(REFERENCE_SCHEMA.contains(key.name), "{}", key.name);
            assert!(REFERENCE_SCHEMA.contains(key.table), "{}", key.table);
        }
    }

    #[test]
    fn a_key_is_partial_exactly_when_the_reference_schema_gives_it_a_where() {
        for key in KEYS {
            let index = format!("CREATE UNIQUE INDEX {} ON ", key.name);
            let statement = REFERENCE_SCHEMA
                .split(';')
                .find(|statement| statement.trim_start().starts_with(&index));
            if key.partial {
                let statement = statement.expect("a partial key is a unique index of its own");
                let on_table = format!(" ON {} (", key.table);
                assert!(statement.contains(&on_table), "{}", key.name);
                assert!(statement.contains(" WHERE "), "{}", key.name);
            } else {
                assert!(statement.is_none(), "{}", key.name);
                let constraint = format!("CONSTRAINT {} ", key.name);
                assert!(REFERENCE_SCHEMA.contains(&constraint), "{}", key.name);
            }
        }
        let partial: Vec<&str> = KEYS
            .iter()
            .filter(|key| key.partial)
            .map(|key| key.name)
            .collect();
        assert_eq!(partial, ["tenants_db_name_key", "tenant_hosts_primary_key"]);
    }

    #[test]
    fn the_reference_schema_always_holds_the_three_tables() {
        for table in ["tenants", "tenant_hosts", "admin_audit"] {
            assert!(
                REFERENCE_SCHEMA.contains(&format!("CREATE TABLE {table} (")),
                "{table}"
            );
        }
        for name in SERVING_KEYS
            .iter()
            .map(|key| key.name)
            .chain(["admin_audit_pkey"])
        {
            assert!(REFERENCE_SCHEMA.contains(name), "{name}");
        }
    }

    #[test]
    fn the_checks_run_in_the_documented_order() {
        let tables: Vec<&str> = TABLES.iter().map(|(table, _)| *table).collect();
        assert_eq!(tables[..2], ["tenants", "tenant_hosts"]);
        let names: Vec<&str> = KEYS.iter().map(|key| key.name).collect();
        assert_eq!(
            names[..5],
            [
                "tenants_pkey",
                "tenants_db_role_key",
                "tenant_hosts_pkey",
                "tenants_db_name_key",
                "tenant_hosts_primary_key",
            ]
        );
        #[cfg(feature = "admin")]
        {
            assert_eq!(tables, ["tenants", "tenant_hosts", "admin_audit"]);
            assert_eq!(names.len(), 6);
            assert_eq!(names[5], "admin_audit_pkey");
        }
        #[cfg(not(feature = "admin"))]
        {
            assert_eq!(tables.len(), 2);
            assert_eq!(names.len(), 5);
        }
    }

    #[test]
    fn a_missing_part_is_a_failed_precondition_naming_it_without_a_domain() {
        for key in ["table", "constraint"] {
            let error = schema_missing(key, "tenant_hosts");
            assert_eq!(error.code(), ErrorCode::FailedPrecondition);
            assert_eq!(error.reason(), Some(reasons::CONTROL_SCHEMA_MISSING));
            assert_eq!(
                error.message(),
                "the control schema is missing or incomplete"
            );
            assert_eq!(error.domain(), None);
            assert_eq!(error.metadata().len(), 1);
            assert_eq!(
                error.metadata().get(key).map(String::as_str),
                Some("tenant_hosts")
            );
        }
    }
}
