//! Run sea-query statements through sqlx.
//!
//! Build a statement with sea-query's typed identifiers, then hand it to one
//! of these functions together with any sqlx executor: a pool (`&pool`), a
//! connection (`&mut *conn`) or a transaction (`&mut *tx`). The statement is
//! rendered for the executor's database with placeholders and every value is
//! bound, never spliced into the SQL text.
//!
//! ```ignore
//! use sekvent_db::query;
//! use sekvent_db::sea_query::{Expr, ExprTrait, Iden, Query};
//!
//! #[derive(Iden)]
//! enum Orders { Table, Id, Reference }
//!
//! let mut tx = pool.begin().await.into_app_error()?;
//! let (id,): (i64,) = query::fetch_one(
//!     &mut *tx,
//!     Query::update()
//!         .table(Orders::Table)
//!         .value(Orders::Reference, "ord-2")
//!         .and_where(Expr::col(Orders::Id).eq(7))
//!         .returning_col(Orders::Id),
//! )
//! .await
//! .into_app_error()?;
//! tx.commit().await.into_app_error()?;
//! ```
//!
//! Errors are sqlx's own, so a caller can look at the violated constraint
//! ([`constraint_name`](crate::constraint_name)) before mapping the rest with
//! [`IntoAppError`](crate::IntoAppError).

use sea_query::QueryBuilder;
use sea_query_sqlx::{SqlxBinder, SqlxValues};
use sqlx::{AssertSqlSafe, Database, Executor, FromRow, IntoArguments};

/// A sqlx database that sea-query renders statements for.
pub trait QueryBackend: Database {
    /// The sea-query builder of the database's SQL dialect.
    type Builder: QueryBuilder + Default;
}

#[cfg(feature = "sea-query-postgres")]
impl QueryBackend for sqlx::Postgres {
    type Builder = sea_query::PostgresQueryBuilder;
}

#[cfg(feature = "sea-query-mysql")]
impl QueryBackend for sqlx::MySql {
    type Builder = sea_query::MysqlQueryBuilder;
}

/// Render `statement` for `DB`: the SQL text with placeholders and the
/// values to bind, in order.
pub fn build<DB, S>(statement: &S) -> (String, SqlxValues)
where
    DB: QueryBackend,
    S: SqlxBinder,
{
    statement.build_sqlx(DB::Builder::default())
}

/// Run `statement` and return the database's result (rows affected).
pub async fn execute<'c, E, S>(
    executor: E,
    statement: &S,
) -> Result<<E::Database as Database>::QueryResult, sqlx::Error>
where
    E: Executor<'c>,
    E::Database: QueryBackend,
    SqlxValues: IntoArguments<E::Database>,
    S: SqlxBinder,
{
    let (sql, values) = build::<E::Database, S>(statement);
    sqlx::query_with(AssertSqlSafe(sql), values)
        .execute(executor)
        .await
}

/// Run `statement` and decode exactly one row; `RowNotFound` when there is
/// none.
pub async fn fetch_one<'c, T, E, S>(executor: E, statement: &S) -> Result<T, sqlx::Error>
where
    E: Executor<'c>,
    E::Database: QueryBackend,
    SqlxValues: IntoArguments<E::Database>,
    T: for<'r> FromRow<'r, <E::Database as Database>::Row> + Send + Unpin,
    S: SqlxBinder,
{
    let (sql, values) = build::<E::Database, S>(statement);
    sqlx::query_as_with(AssertSqlSafe(sql), values)
        .fetch_one(executor)
        .await
}

/// Run `statement` and decode the first row, if any.
pub async fn fetch_optional<'c, T, E, S>(
    executor: E,
    statement: &S,
) -> Result<Option<T>, sqlx::Error>
where
    E: Executor<'c>,
    E::Database: QueryBackend,
    SqlxValues: IntoArguments<E::Database>,
    T: for<'r> FromRow<'r, <E::Database as Database>::Row> + Send + Unpin,
    S: SqlxBinder,
{
    let (sql, values) = build::<E::Database, S>(statement);
    sqlx::query_as_with(AssertSqlSafe(sql), values)
        .fetch_optional(executor)
        .await
}

/// Run `statement` and decode every row. Bound the result set (`LIMIT`):
/// all rows are held in memory.
pub async fn fetch_all<'c, T, E, S>(executor: E, statement: &S) -> Result<Vec<T>, sqlx::Error>
where
    E: Executor<'c>,
    E::Database: QueryBackend,
    SqlxValues: IntoArguments<E::Database>,
    T: for<'r> FromRow<'r, <E::Database as Database>::Row> + Send + Unpin,
    S: SqlxBinder,
{
    let (sql, values) = build::<E::Database, S>(statement);
    sqlx::query_as_with(AssertSqlSafe(sql), values)
        .fetch_all(executor)
        .await
}

#[cfg(test)]
mod tests {
    use sea_query::{Expr, ExprTrait, Query};

    use super::build;

    #[cfg(feature = "sea-query-postgres")]
    #[test]
    fn postgres_statements_use_numbered_placeholders() {
        let statement = Query::select()
            .column("id")
            .from("orders")
            .and_where(Expr::col("reference").eq("ord-1"))
            .to_owned();
        let (sql, values) = build::<sqlx::Postgres, _>(&statement);
        assert_eq!(sql, r#"SELECT "id" FROM "orders" WHERE "reference" = $1"#);
        assert_eq!(values.0.0.len(), 1);
    }

    #[cfg(feature = "sea-query-mysql")]
    #[test]
    fn mysql_statements_use_question_marks() {
        let statement = Query::delete()
            .from_table("orders")
            .and_where(Expr::col("id").eq(7))
            .to_owned();
        let (sql, values) = build::<sqlx::MySql, _>(&statement);
        assert_eq!(sql, "DELETE FROM `orders` WHERE `id` = ?");
        assert_eq!(values.0.0.len(), 1);
    }
}
