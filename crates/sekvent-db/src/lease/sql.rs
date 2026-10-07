//! The lease statements per backend.
//!
//! Parameters appear in the same order in both dialects, so callers bind
//! them once whatever the backend. Expiry is always computed by the
//! database's clock: `now()` in Postgres (the statement's start in
//! autocommit, the transaction's start in the acquiring transaction, which
//! the store opens just before it), `UTC_TIMESTAMP(6)` in MySQL (never
//! `NOW()`, which follows the session time zone). The fence check runs
//! inside the caller's transaction, whose start may lie well in the past,
//! so it reads `clock_timestamp()` instead.

/// The SQL flavour of a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dialect {
    #[cfg(feature = "sqlx-postgres")]
    Postgres,
    #[cfg(feature = "sqlx-mysql")]
    MySql,
}

/// Every statement a store runs, built once per table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Statements {
    /// `CREATE TABLE IF NOT EXISTS`.
    pub(super) schema: String,
    /// Selects every column and no row: fails when the table or a column is missing.
    pub(super) verify: String,
    /// Binds owner, holder, ttl µs, tick µs, name, tick µs, tick µs.
    /// Postgres returns the new fence; MySQL reads it back with `read_fence`.
    pub(super) acquire: String,
    /// Creates the row, already expired, unless it exists. Binds name.
    pub(super) create: String,
    /// The fence under `owner` (MySQL, which has no `RETURNING`). Binds name, owner.
    #[cfg_attr(not(feature = "sqlx-mysql"), allow(dead_code))]
    pub(super) read_fence: String,
    /// Binds ttl µs, name, owner.
    pub(super) renew: String,
    /// Binds name, owner. The fence stays.
    pub(super) release: String,
    /// Binds name.
    pub(super) last_tick: String,
    /// Holder, fence and remaining µs of a held lease. Binds name.
    pub(super) info: String,
}

impl Statements {
    pub(super) fn new(dialect: Dialect, table: &str) -> Self {
        let t = quote(dialect, table);
        match dialect {
            #[cfg(feature = "sqlx-postgres")]
            Dialect::Postgres => Self {
                schema: schema(dialect, table),
                verify: verify(&t),
                acquire: format!(
                    "UPDATE {t} SET owner = $1, holder = $2, fence = fence + 1, acquired_at = now(), \
                     expires_at = now() + $3::double precision * interval '1 microsecond', \
                     last_tick_us = COALESCE($4, last_tick_us) \
                     WHERE name = $5 AND expires_at <= now() \
                     AND ($6::bigint IS NULL OR last_tick_us IS NULL OR last_tick_us < $7) \
                     RETURNING fence"
                ),
                create: format!(
                    "INSERT INTO {t} (name, expires_at) \
                     VALUES ($1, TIMESTAMPTZ '1970-01-01 00:00:00+00') \
                     ON CONFLICT (name) DO NOTHING"
                ),
                read_fence: format!("SELECT fence FROM {t} WHERE name = $1 AND owner = $2"),
                renew: format!(
                    "UPDATE {t} SET expires_at = now() + $1::double precision * interval '1 microsecond' \
                     WHERE name = $2 AND owner = $3 AND expires_at > now()"
                ),
                release: format!(
                    "UPDATE {t} SET expires_at = now(), owner = '' WHERE name = $1 AND owner = $2"
                ),
                last_tick: format!("SELECT last_tick_us FROM {t} WHERE name = $1"),
                info: format!(
                    "SELECT holder, fence, \
                     CAST(EXTRACT(EPOCH FROM (expires_at - now())) * 1000000 AS BIGINT) \
                     FROM {t} WHERE name = $1 AND owner <> '' AND expires_at > now()"
                ),
            },
            #[cfg(feature = "sqlx-mysql")]
            Dialect::MySql => Self {
                schema: schema(dialect, table),
                verify: verify(&t),
                acquire: format!(
                    "UPDATE {t} SET owner = ?, holder = ?, fence = fence + 1, \
                     acquired_at = UTC_TIMESTAMP(6), \
                     expires_at = UTC_TIMESTAMP(6) + INTERVAL ? MICROSECOND, \
                     last_tick_us = COALESCE(?, last_tick_us) \
                     WHERE name = ? AND expires_at <= UTC_TIMESTAMP(6) \
                     AND (? IS NULL OR last_tick_us IS NULL OR last_tick_us < ?)"
                ),
                create: format!(
                    "INSERT IGNORE INTO {t} (name, expires_at) VALUES (?, '1970-01-01 00:00:00')"
                ),
                read_fence: format!("SELECT fence FROM {t} WHERE name = ? AND owner = ?"),
                renew: format!(
                    "UPDATE {t} SET expires_at = UTC_TIMESTAMP(6) + INTERVAL ? MICROSECOND \
                     WHERE name = ? AND owner = ? AND expires_at > UTC_TIMESTAMP(6)"
                ),
                release: format!(
                    "UPDATE {t} SET expires_at = UTC_TIMESTAMP(6), owner = '' \
                     WHERE name = ? AND owner = ?"
                ),
                last_tick: format!("SELECT last_tick_us FROM {t} WHERE name = ?"),
                info: format!(
                    "SELECT holder, fence, TIMESTAMPDIFF(MICROSECOND, UTC_TIMESTAMP(6), expires_at) \
                     FROM {t} WHERE name = ? AND owner <> '' AND expires_at > UTC_TIMESTAMP(6)"
                ),
            },
        }
    }
}

/// The table name quoted for `dialect`. Names are validated identifiers
/// (`[a-z_][a-z0-9_]*`), so quoting only shields reserved words.
fn quote(dialect: Dialect, table: &str) -> String {
    match dialect {
        #[cfg(feature = "sqlx-postgres")]
        Dialect::Postgres => format!("\"{table}\""),
        #[cfg(feature = "sqlx-mysql")]
        Dialect::MySql => format!("`{table}`"),
    }
}

fn verify(t: &str) -> String {
    format!(
        "SELECT name, owner, holder, fence, expires_at, acquired_at, last_tick_us FROM {t} WHERE 1 = 0"
    )
}

/// The DDL of the lease table.
pub(super) fn schema(dialect: Dialect, table: &str) -> String {
    let t = quote(dialect, table);
    match dialect {
        #[cfg(feature = "sqlx-postgres")]
        Dialect::Postgres => format!(
            "CREATE TABLE IF NOT EXISTS {t} (
    name         VARCHAR(200) PRIMARY KEY,
    owner        VARCHAR(64)  NOT NULL DEFAULT '',
    holder       VARCHAR(200) NOT NULL DEFAULT '',
    fence        BIGINT       NOT NULL DEFAULT 0,
    expires_at   TIMESTAMPTZ  NOT NULL,
    acquired_at  TIMESTAMPTZ  NULL,
    last_tick_us BIGINT       NULL
);
"
        ),
        #[cfg(feature = "sqlx-mysql")]
        Dialect::MySql => format!(
            "CREATE TABLE IF NOT EXISTS {t} (
    name         VARCHAR(200) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
    owner        VARCHAR(64)  CHARACTER SET ascii COLLATE ascii_bin NOT NULL DEFAULT '',
    holder       VARCHAR(200) CHARACTER SET ascii NOT NULL DEFAULT '',
    fence        BIGINT       NOT NULL DEFAULT 0,
    expires_at   DATETIME(6)  NOT NULL,
    acquired_at  DATETIME(6)  NULL,
    last_tick_us BIGINT       NULL
) ENGINE = InnoDB;
"
        ),
    }
}

/// The fence check run in the caller's transaction. Binds name, fence.
/// The shared row lock keeps a new holder from taking over until the
/// transaction ends.
pub(super) fn check_fence(dialect: Dialect, table: &str) -> String {
    let t = quote(dialect, table);
    match dialect {
        #[cfg(feature = "sqlx-postgres")]
        Dialect::Postgres => format!(
            "SELECT 1 FROM {t} WHERE name = $1 AND fence = $2 AND expires_at > clock_timestamp() FOR SHARE"
        ),
        // `LOCK IN SHARE MODE` rather than MySQL 8's `FOR SHARE`: MariaDB
        // accepts only the former.
        #[cfg(feature = "sqlx-mysql")]
        Dialect::MySql => format!(
            "SELECT 1 FROM {t} WHERE name = ? AND fence = ? AND expires_at > UTC_TIMESTAMP(6) LOCK IN SHARE MODE"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sqlx-postgres")]
    #[test]
    fn postgres_statements_bind_in_the_shared_order() {
        let sql = Statements::new(Dialect::Postgres, "sekvent_leases");
        assert_eq!(
            sql.schema,
            "CREATE TABLE IF NOT EXISTS \"sekvent_leases\" (
    name         VARCHAR(200) PRIMARY KEY,
    owner        VARCHAR(64)  NOT NULL DEFAULT '',
    holder       VARCHAR(200) NOT NULL DEFAULT '',
    fence        BIGINT       NOT NULL DEFAULT 0,
    expires_at   TIMESTAMPTZ  NOT NULL,
    acquired_at  TIMESTAMPTZ  NULL,
    last_tick_us BIGINT       NULL
);
"
        );
        assert_eq!(
            sql.acquire,
            "UPDATE \"sekvent_leases\" SET owner = $1, holder = $2, fence = fence + 1, \
             acquired_at = now(), expires_at = now() + $3::double precision * interval '1 microsecond', \
             last_tick_us = COALESCE($4, last_tick_us) WHERE name = $5 AND expires_at <= now() \
             AND ($6::bigint IS NULL OR last_tick_us IS NULL OR last_tick_us < $7) RETURNING fence"
        );
        assert_eq!(
            sql.renew,
            "UPDATE \"sekvent_leases\" SET expires_at = now() + $1::double precision * interval \
             '1 microsecond' WHERE name = $2 AND owner = $3 AND expires_at > now()"
        );
        assert!(sql.create.contains("ON CONFLICT (name) DO NOTHING"));
        assert!(sql.verify.ends_with("FROM \"sekvent_leases\" WHERE 1 = 0"));
        assert!(
            sql.release
                .starts_with("UPDATE \"sekvent_leases\" SET expires_at = now(), owner = ''")
        );
        assert!(sql.last_tick.contains("last_tick_us"));
        assert!(
            sql.info
                .contains("EXTRACT(EPOCH FROM (expires_at - now()))")
        );
        assert!(sql.read_fence.contains("owner = $2"));
        assert_eq!(
            check_fence(Dialect::Postgres, "leases"),
            "SELECT 1 FROM \"leases\" WHERE name = $1 AND fence = $2 \
             AND expires_at > clock_timestamp() FOR SHARE"
        );
    }

    #[cfg(feature = "sqlx-mysql")]
    #[test]
    fn mysql_statements_use_utc_and_positional_parameters() {
        let sql = Statements::new(Dialect::MySql, "sekvent_leases");
        assert_eq!(
            sql.schema,
            "CREATE TABLE IF NOT EXISTS `sekvent_leases` (
    name         VARCHAR(200) CHARACTER SET ascii COLLATE ascii_bin NOT NULL PRIMARY KEY,
    owner        VARCHAR(64)  CHARACTER SET ascii COLLATE ascii_bin NOT NULL DEFAULT '',
    holder       VARCHAR(200) CHARACTER SET ascii NOT NULL DEFAULT '',
    fence        BIGINT       NOT NULL DEFAULT 0,
    expires_at   DATETIME(6)  NOT NULL,
    acquired_at  DATETIME(6)  NULL,
    last_tick_us BIGINT       NULL
) ENGINE = InnoDB;
"
        );
        for statement in [
            &sql.acquire,
            &sql.renew,
            &sql.release,
            &sql.info,
            &sql.create,
        ] {
            assert!(!statement.contains("NOW()"), "{statement}");
            assert!(!statement.contains('$'), "{statement}");
        }
        assert_eq!(sql.acquire.matches('?').count(), 7);
        assert_eq!(sql.renew.matches('?').count(), 3);
        assert!(
            sql.create
                .starts_with("INSERT IGNORE INTO `sekvent_leases`")
        );
        assert!(sql.read_fence.contains("owner = ?"));
        assert!(
            sql.info
                .contains("TIMESTAMPDIFF(MICROSECOND, UTC_TIMESTAMP(6), expires_at)")
        );
        assert!(sql.verify.ends_with("FROM `sekvent_leases` WHERE 1 = 0"));
        assert!(sql.last_tick.ends_with("WHERE name = ?"));
        assert!(check_fence(Dialect::MySql, "leases").ends_with("LOCK IN SHARE MODE"));
    }
}
