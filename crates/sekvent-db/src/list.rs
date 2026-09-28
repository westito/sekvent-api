//! Generic list filtering, search and pagination for sea-orm entities.
//!
//! Clients send [`ListParams`]; the server declares, per entity, which
//! columns a client may sort and filter by in a [`ListSpec`], then turns the
//! parameters into a sort order and a sea-orm [`Condition`]. The allow-lists
//! are fail-closed: a column is usable only when the spec names it, and a
//! sort or filter on any other field (including one that is not a column at
//! all) is an `INVALID_ARGUMENT` error naming the field. Sorting or filtering
//! on a secret column (a password hash, an internal flag) would leak its
//! contents one comparison at a time, so such columns are never listed.
//!
//! ```ignore
//! let spec = ListSpec::<invoice::Entity>::new()
//!     .sortable([invoice::Column::CreatedAt, invoice::Column::Total])
//!     .filterable([invoice::Column::Status, invoice::Column::CreatedAt]);
//! let condition = spec.apply_filters(Condition::all(), &params.filters)?;
//! let mut query = invoice::Entity::find().filter(condition);
//! if let Some((column, order)) = spec.sort(&params)? {
//!     query = query.order_by(column, order);
//! }
//! let page = paginate(params.page, params.page_size, 100)?;
//! let rows = query.offset(page.offset).limit(page.limit).all(&db).await?;
//! ```
//!
//! Within an allowed column, unsupported operators and unparseable values
//! are ignored rather than rejected, so an old client never breaks a list
//! view.
//!
//! | column type | operators | value |
//! |---|---|---|
//! | text | `""`/`prefix` (`LIKE 'v%'`), `eq` | as is |
//! | integer | `""`/`eq`, `range`, `gte`, `lte`, `gt`, `lt` | `i64` |
//! | decimal, float | same as integer | decimal |
//! | date | same as integer; `range` is inclusive | `YYYY-MM-DD` |
//! | date-time | same, by whole days (see below) | `YYYY-MM-DD[...]` |
//! | boolean | `""`/`eq` | `1`, `0`, `true`, `false` |
//! | uuid | `""`/`eq` | hyphenated UUID |
//!
//! Text matching is a prefix match, never `%v%`: a leading wildcard cannot
//! use a b-tree index and turns every keystroke into a table scan. `%` and
//! `_` in the value are escaped, so they match literally.
//!
//! Date-time columns compare by day, as half-open ranges in UTC:
//! `eq d` is `[d, d+1)`, `range a..b` is `[a, b+1)`, `gte d` is `>= d`,
//! `gt d` is `>= d+1`, `lte d` is `< d+1` and `lt d` is `< d`.

use std::fmt;

use sea_orm::prelude::{ChronoDate, ChronoDateTime, Decimal, Uuid};
use sea_orm::sea_query::{ColumnType, LikeExpr, Order};
use sea_orm::{ColumnTrait, Condition, EntityTrait, IdenStatic, Iterable, Value};
use sekvent_error::AppError;

/// The escape character used in generated `LIKE` patterns. It is neither a
/// backslash (whose meaning differs between backends and SQL modes) nor
/// a pattern metacharacter.
const LIKE_ESCAPE: char = '!';

/// The largest offset or limit a query may bind: drivers bind them as
/// signed 64-bit integers.
const MAX_BIND: u64 = i64::MAX.unsigned_abs();

/// Client-supplied field names are echoed in errors up to this many
/// characters.
const MAX_ECHOED_FIELD: usize = 64;

/// List query parameters as a client sends them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "camelCase"))]
pub struct ListParams {
    /// Free-text search term, for [`prefix_search`].
    pub search: Option<String>,
    /// Field to sort by (`camelCase` or `snake_case`), for [`ListSpec::sort`].
    pub sort_by: Option<String>,
    /// Sort descending.
    pub sort_desc: bool,
    /// 1-based page number; 0 is treated as 1.
    pub page: u64,
    /// Page size; clamped by [`paginate`].
    pub page_size: u64,
    /// Per-column filters, for [`ListSpec::apply_filters`].
    pub filters: Vec<ColumnFilter>,
}

/// One per-column filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "camelCase"))]
pub struct ColumnFilter {
    /// Field name, `camelCase` or `snake_case`.
    pub field: String,
    /// The value, or the lower bound for `range`.
    pub value: String,
    /// The upper bound for `range`.
    pub value_to: Option<String>,
    /// `""`, `prefix`, `eq`, `range`, `gte`, `lte`, `gt` or `lt`.
    pub op: String,
}

impl ColumnFilter {
    /// A filter with an operator and a single value.
    pub fn new(field: impl Into<String>, op: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            value: value.into(),
            value_to: None,
            op: op.into(),
        }
    }

    /// A `range` filter; either bound may be blank for an open end.
    pub fn range(field: impl Into<String>, from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            value: from.into(),
            value_to: Some(to.into()),
            op: "range".to_owned(),
        }
    }
}

/// A parsed filter operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Default,
    Prefix,
    Eq,
    Range,
    Gte,
    Lte,
    Gt,
    Lt,
}

impl Op {
    fn parse(op: &str) -> Option<Self> {
        Some(match op.trim().to_ascii_lowercase().as_str() {
            "" => Self::Default,
            "prefix" => Self::Prefix,
            "eq" => Self::Eq,
            "range" => Self::Range,
            "gte" => Self::Gte,
            "lte" => Self::Lte,
            "gt" => Self::Gt,
            "lt" => Self::Lt,
            _ => return None,
        })
    }
}

/// How a column's values are parsed and compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Integer,
    Decimal,
    Date,
    DateTime,
    DateTimeUtc,
    Boolean,
    Uuid,
}

fn kind_of(column_type: &ColumnType) -> Option<Kind> {
    Some(match column_type {
        ColumnType::Char(_) | ColumnType::String(_) | ColumnType::Text => Kind::Text,
        ColumnType::TinyInteger
        | ColumnType::SmallInteger
        | ColumnType::Integer
        | ColumnType::BigInteger
        | ColumnType::TinyUnsigned
        | ColumnType::SmallUnsigned
        | ColumnType::Unsigned
        | ColumnType::BigUnsigned => Kind::Integer,
        ColumnType::Float | ColumnType::Double | ColumnType::Decimal(_) | ColumnType::Money(_) => {
            Kind::Decimal
        }
        ColumnType::Date => Kind::Date,
        ColumnType::DateTime | ColumnType::Timestamp => Kind::DateTime,
        ColumnType::TimestampWithTimeZone => Kind::DateTimeUtc,
        ColumnType::Boolean => Kind::Boolean,
        ColumnType::Uuid => Kind::Uuid,
        _ => return None,
    })
}

/// `createdAt` and `created_at` both become `created_at`; `HTTPStatus`
/// becomes `http_status`.
pub fn to_snake_case(field: &str) -> String {
    let chars: Vec<char> = field.trim().chars().collect();
    let mut out = String::with_capacity(chars.len() + 4);
    for (index, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let previous = index.checked_sub(1).map(|i| chars[i]);
            let next = chars.get(index + 1);
            let boundary = previous.is_some_and(|p| {
                p.is_ascii_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_ascii_uppercase() && next.is_some_and(char::is_ascii_lowercase))
            });
            if boundary && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The column of `E` named `field` (`camelCase` or `snake_case`).
fn resolve_column<E: EntityTrait>(field: &str) -> Option<E::Column> {
    let wanted = to_snake_case(field);
    E::Column::iter().find(|column| column.as_str() == wanted)
}

/// The columns of one entity a client may sort and filter by.
///
/// Both lists start empty: a new spec rejects every sort and every filter
/// until columns are allowed explicitly. See the [module documentation](self)
/// for an example.
pub struct ListSpec<E: EntityTrait> {
    sortable: Vec<E::Column>,
    filterable: Vec<E::Column>,
}

impl<E: EntityTrait> ListSpec<E> {
    /// A spec that allows no sorting and no filtering.
    pub fn new() -> Self {
        Self {
            sortable: Vec::new(),
            filterable: Vec::new(),
        }
    }

    /// Allow sorting by `columns` (added to any already allowed).
    #[must_use]
    pub fn sortable(mut self, columns: impl IntoIterator<Item = E::Column>) -> Self {
        self.sortable.extend(columns);
        self
    }

    /// Allow filtering by `columns` (added to any already allowed).
    #[must_use]
    pub fn filterable(mut self, columns: impl IntoIterator<Item = E::Column>) -> Self {
        self.filterable.extend(columns);
        self
    }

    /// The sortable column named `field` (`camelCase` or `snake_case`).
    ///
    /// Fails with `INVALID_ARGUMENT` naming the field when it is not a
    /// sortable column of `E`.
    pub fn sort_column(&self, field: &str) -> Result<E::Column, AppError> {
        allowed_column::<E>(&self.sortable, field)
            .ok_or_else(|| disallowed("sortBy", field, "is not a sortable field"))
    }

    /// The column and direction `params` asks to sort by, or `None` when it
    /// names no field.
    ///
    /// Fails with `INVALID_ARGUMENT` when the field is not sortable.
    pub fn sort(&self, params: &ListParams) -> Result<Option<(E::Column, Order)>, AppError> {
        let Some(field) = params.sort_by.as_deref().and_then(non_blank) else {
            return Ok(None);
        };
        let order = if params.sort_desc {
            Order::Desc
        } else {
            Order::Asc
        };
        Ok(Some((self.sort_column(field)?, order)))
    }

    /// Add one condition per usable filter to `condition`.
    ///
    /// Fails with `INVALID_ARGUMENT` naming the field when any filter is on
    /// a field that is not a filterable column of `E`. Within allowed
    /// columns, filters with unknown operators, blank values or values that
    /// do not parse for the column's type are skipped; see the
    /// [module documentation](self) for the operators per column type.
    pub fn apply_filters(
        &self,
        condition: Condition,
        filters: &[ColumnFilter],
    ) -> Result<Condition, AppError> {
        filters.iter().try_fold(condition, |condition, filter| {
            let column = allowed_column::<E>(&self.filterable, &filter.field)
                .ok_or_else(|| disallowed("filters", &filter.field, "is not a filterable field"))?;
            Ok(match filter_condition(column, filter) {
                Some(extra) => condition.add(extra),
                None => condition,
            })
        })
    }
}

impl<E: EntityTrait> Default for ListSpec<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: EntityTrait> Clone for ListSpec<E> {
    fn clone(&self) -> Self {
        Self {
            sortable: self.sortable.clone(),
            filterable: self.filterable.clone(),
        }
    }
}

impl<E: EntityTrait> fmt::Debug for ListSpec<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = |columns: &[E::Column]| -> Vec<&'static str> {
            columns.iter().map(IdenStatic::as_str).collect()
        };
        f.debug_struct("ListSpec")
            .field("sortable", &names(&self.sortable))
            .field("filterable", &names(&self.filterable))
            .finish()
    }
}

/// The column named `field` if it is one of `allowed`.
fn allowed_column<E: EntityTrait>(allowed: &[E::Column], field: &str) -> Option<E::Column> {
    let column = resolve_column::<E>(field)?;
    allowed
        .iter()
        .any(|candidate| candidate.as_str() == column.as_str())
        .then_some(column)
}

/// `INVALID_ARGUMENT` naming a rejected client field, echoed only up to
/// [`MAX_ECHOED_FIELD`] characters.
fn disallowed(parameter: &str, field: &str, problem: &str) -> AppError {
    let mut shown: String = field.trim().chars().take(MAX_ECHOED_FIELD).collect();
    if field.trim().chars().nth(MAX_ECHOED_FIELD).is_some() {
        shown.push('…');
    }
    AppError::invalid_argument(format!("field `{shown}` {problem}"))
        .with_field_violation(parameter, format!("`{shown}` {problem}"))
}

fn filter_condition<C: ColumnTrait>(column: C, filter: &ColumnFilter) -> Option<Condition> {
    let op = Op::parse(&filter.op)?;
    let kind = kind_of(column.def().get_column_type())?;
    let from = non_blank(&filter.value);
    let to = filter.value_to.as_deref().and_then(non_blank);
    match kind {
        Kind::Text => text_condition(column, op, from?),
        Kind::Integer => ordered_condition(column, op, from, to, |v| v.parse::<i64>().ok()),
        Kind::Decimal => ordered_condition(column, op, from, to, |v| v.parse::<Decimal>().ok()),
        Kind::Date => ordered_condition(column, op, from, to, parse_day),
        Kind::DateTime => day_condition(column, op, from, to, |day| {
            day.and_hms_opt(0, 0, 0).map(Value::from)
        }),
        Kind::DateTimeUtc => day_condition(column, op, from, to, |day| {
            day.and_hms_opt(0, 0, 0)
                .map(|naive| Value::from(naive.and_utc()))
        }),
        Kind::Boolean => match op {
            Op::Default | Op::Eq => Some(Condition::all().add(column.eq(parse_bool(from?)?))),
            _ => None,
        },
        Kind::Uuid => match op {
            Op::Default | Op::Eq => {
                Some(Condition::all().add(column.eq(from?.parse::<Uuid>().ok()?)))
            }
            _ => None,
        },
    }
}

fn non_blank(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn text_condition<C: ColumnTrait>(column: C, op: Op, value: &str) -> Option<Condition> {
    let expr = match op {
        Op::Default | Op::Prefix => column.like(prefix_pattern(value)),
        Op::Eq => column.eq(value),
        _ => return None,
    };
    Some(Condition::all().add(expr))
}

/// `LIKE 'value%' ESCAPE '!'` with the value's metacharacters escaped.
fn prefix_pattern(value: &str) -> LikeExpr {
    let mut pattern = String::with_capacity(value.len() + 1);
    for c in value.chars() {
        if matches!(c, '%' | '_' | LIKE_ESCAPE) {
            pattern.push(LIKE_ESCAPE);
        }
        pattern.push(c);
    }
    pattern.push('%');
    LikeExpr::new(pattern).escape(LIKE_ESCAPE)
}

/// Integers, decimals and dates: plain comparisons, `range` inclusive.
fn ordered_condition<C, T, P>(
    column: C,
    op: Op,
    from: Option<&str>,
    to: Option<&str>,
    parse: P,
) -> Option<Condition>
where
    C: ColumnTrait,
    T: Into<Value>,
    P: Fn(&str) -> Option<T>,
{
    let value = || from.and_then(&parse);
    let condition = Condition::all();
    Some(match op {
        Op::Default | Op::Eq => condition.add(column.eq(value()?)),
        Op::Gte => condition.add(column.gte(value()?)),
        Op::Lte => condition.add(column.lte(value()?)),
        Op::Gt => condition.add(column.gt(value()?)),
        Op::Lt => condition.add(column.lt(value()?)),
        Op::Range => {
            let lower = value();
            let upper = to.and_then(&parse);
            if lower.is_none() && upper.is_none() {
                return None;
            }
            let condition = match lower {
                Some(lower) => condition.add(column.gte(lower)),
                None => condition,
            };
            match upper {
                Some(upper) => condition.add(column.lte(upper)),
                None => condition,
            }
        }
        Op::Prefix => return None,
    })
}

/// Date-time columns by whole days: every bound is a midnight, and upper
/// bounds are exclusive.
fn day_condition<C, M>(
    column: C,
    op: Op,
    from: Option<&str>,
    to: Option<&str>,
    midnight: M,
) -> Option<Condition>
where
    C: ColumnTrait,
    M: Fn(ChronoDate) -> Option<Value>,
{
    let day = |value: Option<&str>| value.and_then(parse_day);
    let next = |date: ChronoDate| date.succ_opt().and_then(&midnight);
    let condition = Condition::all();
    Some(match op {
        Op::Default | Op::Eq => {
            let date = day(from)?;
            condition
                .add(column.gte(midnight(date)?))
                .add(column.lt(next(date)?))
        }
        Op::Gte => condition.add(column.gte(midnight(day(from)?)?)),
        Op::Gt => condition.add(column.gte(next(day(from)?)?)),
        Op::Lte => condition.add(column.lt(next(day(from)?)?)),
        Op::Lt => condition.add(column.lt(midnight(day(from)?)?)),
        Op::Range => {
            let lower = day(from).and_then(&midnight);
            let upper = day(to).and_then(next);
            if lower.is_none() && upper.is_none() {
                return None;
            }
            let condition = match lower {
                Some(lower) => condition.add(column.gte(lower)),
                None => condition,
            };
            match upper {
                Some(upper) => condition.add(column.lt(upper)),
                None => condition,
            }
        }
        Op::Prefix => return None,
    })
}

/// `YYYY-MM-DD`, also accepted as the start of a longer timestamp
/// (`2024-03-01T12:00:00Z`).
fn parse_day(value: &str) -> Option<ChronoDate> {
    let day = value.get(..10).unwrap_or(value);
    ChronoDate::parse_from_str(day, "%Y-%m-%d").ok()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// Match `term` as a prefix of any of `columns` (`col LIKE 'term%'`,
/// metacharacters escaped). A blank term yields an empty condition that
/// filters nothing.
///
/// The columns come from the server, never from the client; pass only
/// columns whose contents the caller may see.
pub fn prefix_search<E: EntityTrait>(columns: &[E::Column], term: &str) -> Condition {
    let Some(term) = non_blank(term) else {
        return Condition::all();
    };
    columns.iter().fold(Condition::any(), |condition, column| {
        condition.add(column.like(prefix_pattern(term)))
    })
}

/// Offset and limit for one page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// Rows to skip.
    pub offset: u64,
    /// Rows to return.
    pub limit: u64,
}

/// Turn a 1-based `page` and a requested `page_size` into offset and limit.
///
/// Page 0 is page 1. The size is clamped to `1..=max_page_size`, and a size
/// of 0 means the maximum; the limit never exceeds `i64::MAX`. A page whose
/// offset would not fit in an `i64` (what drivers bind) is an
/// `INVALID_ARGUMENT` error naming `page`.
pub fn paginate(page: u64, page_size: u64, max_page_size: u64) -> Result<Page, AppError> {
    let max = max_page_size.clamp(1, MAX_BIND);
    let limit = if page_size == 0 {
        max
    } else {
        page_size.min(max)
    };
    let offset = page
        .max(1)
        .saturating_sub(1)
        .checked_mul(limit)
        .filter(|offset| *offset <= MAX_BIND)
        .ok_or_else(|| {
            AppError::invalid_argument("page is out of range")
                .with_field_violation("page", "the page is beyond the last possible row")
        })?;
    Ok(Page { offset, limit })
}

/// A `NaiveDateTime` at midnight, for callers building their own day ranges.
pub fn start_of_day(day: ChronoDate) -> Option<ChronoDateTime> {
    day.and_hms_opt(0, 0, 0)
}

#[cfg(test)]
mod tests {
    use sea_orm::{DbBackend, QueryFilter, QueryOrder, QueryTrait};
    use sekvent_error::ErrorCode;

    use super::*;

    #[allow(unreachable_pub)]
    mod item {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "item")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i64,
            pub name: String,
            pub quantity: i32,
            pub price: Decimal,
            pub weight: f64,
            pub born_on: Date,
            pub created_at: DateTime,
            pub updated_at: DateTimeUtc,
            pub active: bool,
            pub external_id: Uuid,
            pub password_hash: String,
            pub payload: Json,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// Every column filterable, for the per-type operator tests.
    fn every_column() -> ListSpec<item::Entity> {
        ListSpec::new().filterable(item::Column::iter())
    }

    fn where_clause(backend: DbBackend, filters: &[ColumnFilter]) -> String {
        let condition = every_column()
            .apply_filters(Condition::all(), filters)
            .unwrap();
        let sql = item::Entity::find()
            .filter(condition)
            .build(backend)
            .to_string();
        sql.split_once(" WHERE ")
            .map_or_else(String::new, |(_, clause)| clause.to_owned())
    }

    fn pg(filters: &[ColumnFilter]) -> String {
        where_clause(DbBackend::Postgres, filters)
    }

    fn my(filters: &[ColumnFilter]) -> String {
        where_clause(DbBackend::MySql, filters)
    }

    /// sea-query renders an empty `Condition::all()` as the constant `TRUE`.
    const NO_FILTER: &str = "TRUE";

    fn f(field: &str, op: &str, value: &str) -> ColumnFilter {
        ColumnFilter::new(field, op, value)
    }

    #[test]
    fn snake_case_normalisation() {
        assert_eq!(to_snake_case("createdAt"), "created_at");
        assert_eq!(to_snake_case("created_at"), "created_at");
        assert_eq!(to_snake_case("HTTPStatus"), "http_status");
        assert_eq!(to_snake_case("externalID"), "external_id");
        assert_eq!(to_snake_case("line2Total"), "line2_total");
        assert_eq!(to_snake_case("Name"), "name");
        assert_eq!(to_snake_case(" bornOn "), "born_on");
    }

    #[test]
    fn columns_resolve_from_either_case() {
        assert!(matches!(
            resolve_column::<item::Entity>("createdAt"),
            Some(item::Column::CreatedAt)
        ));
        assert!(matches!(
            resolve_column::<item::Entity>("password_hash"),
            Some(item::Column::PasswordHash)
        ));
        assert!(resolve_column::<item::Entity>("nope").is_none());
    }

    #[test]
    fn text_defaults_to_an_escaped_prefix_match() {
        assert_eq!(
            pg(&[f("name", "", "50%_off!")]),
            r#""item"."name" LIKE '50!%!_off!!%' ESCAPE '!'"#
        );
        assert_eq!(
            my(&[f("name", "prefix", "ab")]),
            "`item`.`name` LIKE 'ab%' ESCAPE '!'"
        );
        let clause = pg(&[f("name", "", "ab")]);
        assert!(!clause.contains("'%"), "never a leading wildcard: {clause}");
    }

    #[test]
    fn text_eq_is_exact_and_other_ops_are_ignored() {
        assert_eq!(
            pg(&[f("name", "eq", "Widget")]),
            r#""item"."name" = 'Widget'"#
        );
        assert_eq!(pg(&[f("name", "gt", "a")]), NO_FILTER);
        assert_eq!(pg(&[f("name", "", "   ")]), NO_FILTER);
    }

    #[test]
    fn integers_support_every_comparison() {
        assert_eq!(pg(&[f("quantity", "", "5")]), r#""item"."quantity" = 5"#);
        assert_eq!(pg(&[f("quantity", "eq", "5")]), r#""item"."quantity" = 5"#);
        assert_eq!(
            pg(&[f("quantity", "gte", "5")]),
            r#""item"."quantity" >= 5"#
        );
        assert_eq!(
            pg(&[f("quantity", "lte", "5")]),
            r#""item"."quantity" <= 5"#
        );
        assert_eq!(pg(&[f("quantity", "gt", "5")]), r#""item"."quantity" > 5"#);
        assert_eq!(
            pg(&[f("quantity", "lt", "-5")]),
            r#""item"."quantity" < -5"#
        );
        assert_eq!(
            pg(&[ColumnFilter::range("quantity", "1", "9")]),
            r#""item"."quantity" >= 1 AND "item"."quantity" <= 9"#
        );
        assert_eq!(
            pg(&[ColumnFilter::range("quantity", "", "9")]),
            r#""item"."quantity" <= 9"#
        );
        assert_eq!(
            pg(&[ColumnFilter::range("quantity", "1", "")]),
            r#""item"."quantity" >= 1"#
        );
        assert_eq!(pg(&[ColumnFilter::range("quantity", "", "")]), NO_FILTER);
        assert_eq!(pg(&[ColumnFilter::range("quantity", "x", "y")]), NO_FILTER);
        assert_eq!(pg(&[f("quantity", "", "five")]), NO_FILTER);
        assert_eq!(pg(&[f("quantity", "prefix", "5")]), NO_FILTER);
        assert_eq!(pg(&[f("id", "eq", "42")]), r#""item"."id" = 42"#);
    }

    #[test]
    fn decimals_and_floats_parse_as_decimal() {
        assert_eq!(
            pg(&[f("price", "gte", "12.50")]),
            r#""item"."price" >= 12.50"#
        );
        assert_eq!(pg(&[f("weight", "lt", "0.5")]), r#""item"."weight" < 0.5"#);
        assert_eq!(pg(&[f("price", "", "abc")]), NO_FILTER);
    }

    #[test]
    fn dates_compare_by_day_with_an_inclusive_range() {
        assert_eq!(
            pg(&[f("bornOn", "", "2024-03-01")]),
            r#""item"."born_on" = '2024-03-01'"#
        );
        assert_eq!(
            pg(&[ColumnFilter::range("born_on", "2024-03-01", "2024-03-31")]),
            r#""item"."born_on" >= '2024-03-01' AND "item"."born_on" <= '2024-03-31'"#
        );
        assert_eq!(
            pg(&[f("born_on", "gt", "2024-03-01T10:00:00Z")]),
            r#""item"."born_on" > '2024-03-01'"#
        );
        assert_eq!(pg(&[f("born_on", "", "03/01/2024")]), NO_FILTER);
    }

    #[test]
    fn datetimes_use_half_open_day_ranges() {
        let at = |day: &str| format!("'{day} 00:00:00.000000'");
        assert_eq!(
            pg(&[f("createdAt", "", "2024-02-28")]),
            format!(
                r#""item"."created_at" >= {} AND "item"."created_at" < {}"#,
                at("2024-02-28"),
                at("2024-02-29")
            )
        );
        assert_eq!(
            pg(&[f("created_at", "gte", "2024-03-01")]),
            format!(r#""item"."created_at" >= {}"#, at("2024-03-01"))
        );
        assert_eq!(
            pg(&[f("created_at", "gt", "2024-03-01")]),
            format!(r#""item"."created_at" >= {}"#, at("2024-03-02"))
        );
        assert_eq!(
            pg(&[f("created_at", "lte", "2024-03-01")]),
            format!(r#""item"."created_at" < {}"#, at("2024-03-02"))
        );
        assert_eq!(
            pg(&[f("created_at", "lt", "2024-03-01")]),
            format!(r#""item"."created_at" < {}"#, at("2024-03-01"))
        );
        assert_eq!(
            pg(&[ColumnFilter::range(
                "created_at",
                "2024-03-01",
                "2024-03-31"
            )]),
            format!(
                r#""item"."created_at" >= {} AND "item"."created_at" < {}"#,
                at("2024-03-01"),
                at("2024-04-01")
            )
        );
        assert_eq!(
            pg(&[ColumnFilter::range("created_at", "", "2024-12-31")]),
            format!(r#""item"."created_at" < {}"#, at("2025-01-01"))
        );
        assert_eq!(
            pg(&[ColumnFilter::range("created_at", "2024-03-01", "")]),
            format!(r#""item"."created_at" >= {}"#, at("2024-03-01"))
        );
        assert_eq!(pg(&[ColumnFilter::range("created_at", "", "")]), NO_FILTER);
        assert_eq!(pg(&[f("created_at", "prefix", "2024-03-01")]), NO_FILTER);
        assert_eq!(pg(&[f("created_at", "", "yesterday")]), NO_FILTER);
    }

    #[test]
    fn utc_timestamps_bind_utc_midnights() {
        let clause = pg(&[f("updatedAt", "eq", "2024-03-01")]);
        assert_eq!(
            clause,
            r#""item"."updated_at" >= '2024-03-01 00:00:00.000000 +00:00' AND "item"."updated_at" < '2024-03-02 00:00:00.000000 +00:00'"#
        );
    }

    #[test]
    fn booleans_accept_digits_and_words() {
        for (value, expected) in [
            ("1", "TRUE"),
            ("true", "TRUE"),
            ("0", "FALSE"),
            ("FALSE", "FALSE"),
        ] {
            assert_eq!(
                pg(&[f("active", "", value)]),
                format!(r#""item"."active" = {expected}"#)
            );
        }
        assert_eq!(pg(&[f("active", "", "yes")]), NO_FILTER);
        assert_eq!(pg(&[f("active", "gt", "1")]), NO_FILTER);
    }

    #[test]
    fn uuids_match_exactly() {
        let id = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        assert!(pg(&[f("externalId", "", id)]).contains(id));
        assert_eq!(pg(&[f("externalId", "", "not-a-uuid")]), NO_FILTER);
        assert_eq!(pg(&[f("externalId", "gt", id)]), NO_FILTER);
    }

    #[test]
    fn unknown_ops_and_types_are_ignored() {
        assert_eq!(pg(&[f("name", "contains", "x")]), NO_FILTER);
        assert_eq!(pg(&[f("payload", "eq", "{}")]), NO_FILTER);
    }

    #[test]
    fn filters_combine_with_and() {
        assert_eq!(
            pg(&[
                f("name", "", "a"),
                f("quantity", "gt", "1"),
                f("quantity", "between", "x")
            ]),
            r#""item"."name" LIKE 'a%' ESCAPE '!' AND "item"."quantity" > 1"#
        );
    }

    fn safe_spec() -> ListSpec<item::Entity> {
        ListSpec::new()
            .sortable([item::Column::Name])
            .sortable([item::Column::CreatedAt])
            .filterable([item::Column::Name, item::Column::CreatedAt])
    }

    #[test]
    fn a_new_spec_allows_nothing() {
        let spec = ListSpec::<item::Entity>::default();
        assert!(spec.sort_column("name").is_err());
        assert!(
            spec.apply_filters(Condition::all(), &[f("name", "", "a")])
                .is_err()
        );
        assert!(spec.apply_filters(Condition::all(), &[]).is_ok());
    }

    #[test]
    fn filters_outside_the_allow_list_are_rejected_naming_the_field() {
        let spec = safe_spec();
        for field in ["passwordHash", "password_hash", "quantity", "nope"] {
            let error = spec
                .apply_filters(
                    Condition::all(),
                    &[f("name", "", "a"), f(field, "prefix", "$argon2")],
                )
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(
                error.message(),
                format!("field `{field}` is not a filterable field")
            );
            assert_eq!(error.field_violations()[0].field, "filters");
        }
        let condition = spec
            .apply_filters(
                Condition::all(),
                &[f("name", "", "a"), f("createdAt", "gte", "2024-01-01")],
            )
            .unwrap();
        let sql = item::Entity::find()
            .filter(condition)
            .build(DbBackend::Postgres)
            .to_string();
        assert!(sql.contains(r#""item"."name" LIKE 'a%'"#), "{sql}");
        assert!(sql.contains(r#""item"."created_at" >="#), "{sql}");
    }

    #[test]
    fn sorting_is_limited_to_the_allow_list() {
        let spec = safe_spec();
        assert!(matches!(
            spec.sort_column("createdAt"),
            Ok(item::Column::CreatedAt)
        ));
        for field in ["passwordHash", "quantity", "nope"] {
            let error = spec.sort_column(field).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(
                error.message(),
                format!("field `{field}` is not a sortable field")
            );
            assert_eq!(error.field_violations()[0].field, "sortBy");
        }
    }

    #[test]
    fn sort_reads_the_params() {
        let spec = safe_spec();
        let mut params = ListParams::default();
        assert!(spec.sort(&params).unwrap().is_none());
        params.sort_by = Some("  ".to_owned());
        assert!(spec.sort(&params).unwrap().is_none());

        params.sort_by = Some("name".to_owned());
        let (column, order) = spec.sort(&params).unwrap().unwrap();
        assert!(matches!(column, item::Column::Name));
        assert_eq!(order, Order::Asc);

        params.sort_by = Some("created_at".to_owned());
        params.sort_desc = true;
        let (column, order) = spec.sort(&params).unwrap().unwrap();
        let sql = item::Entity::find()
            .order_by(column, order)
            .build(DbBackend::Postgres)
            .to_string();
        assert!(
            sql.ends_with(r#"ORDER BY "item"."created_at" DESC"#),
            "{sql}"
        );

        params.sort_by = Some("passwordHash".to_owned());
        assert_eq!(
            spec.sort(&params).unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn long_rejected_fields_are_truncated_in_errors() {
        let field = "x".repeat(500);
        let error = safe_spec().sort_column(&field).unwrap_err();
        let expected = format!("field `{}…` is not a sortable field", "x".repeat(64));
        assert_eq!(error.message(), expected);
        let exact = "y".repeat(64);
        let error = safe_spec().sort_column(&exact).unwrap_err();
        assert_eq!(
            error.message(),
            format!("field `{exact}` is not a sortable field")
        );
    }

    #[test]
    fn specs_debug_and_clone_by_column_name() {
        let spec = safe_spec();
        let copy = spec.clone();
        assert_eq!(
            format!("{copy:?}"),
            r#"ListSpec { sortable: ["name", "created_at"], filterable: ["name", "created_at"] }"#
        );
    }

    #[test]
    fn prefix_search_ors_the_columns() {
        let sql = item::Entity::find()
            .filter(prefix_search::<item::Entity>(
                &[item::Column::Name, item::Column::PasswordHash],
                "wid_",
            ))
            .build(DbBackend::MySql)
            .to_string();
        assert!(sql.ends_with(
            "WHERE `item`.`name` LIKE 'wid!_%' ESCAPE '!' OR `item`.`password_hash` LIKE 'wid!_%' ESCAPE '!'"
        ));
        let unfiltered = item::Entity::find()
            .filter(prefix_search::<item::Entity>(&[item::Column::Name], "  "))
            .build(DbBackend::MySql)
            .to_string();
        assert!(unfiltered.ends_with(&format!("WHERE {NO_FILTER}")));
    }

    #[test]
    fn pagination_clamps_the_size() {
        let page = |page, size, max| paginate(page, size, max).unwrap();
        assert_eq!(
            page(1, 20, 100),
            Page {
                offset: 0,
                limit: 20
            }
        );
        assert_eq!(
            page(0, 20, 100),
            Page {
                offset: 0,
                limit: 20
            }
        );
        assert_eq!(
            page(3, 20, 100),
            Page {
                offset: 40,
                limit: 20
            }
        );
        assert_eq!(
            page(2, 500, 100),
            Page {
                offset: 100,
                limit: 100
            }
        );
        assert_eq!(
            page(2, 0, 50),
            Page {
                offset: 50,
                limit: 50
            }
        );
        assert_eq!(
            page(2, 10, 0),
            Page {
                offset: 1,
                limit: 1
            }
        );
        assert_eq!(page(1, 0, u64::MAX).limit, MAX_BIND);
    }

    #[test]
    fn offsets_beyond_i64_are_rejected() {
        let max = i64::MAX.unsigned_abs();
        // The largest offset that still binds.
        assert_eq!(paginate(max / 7 + 1, 7, 7).unwrap().offset, max / 7 * 7);
        assert_eq!(paginate(max + 1, 1, 1).unwrap().offset, max);
        for (page, size, limit) in [
            (u64::MAX, 100, 100),
            (max + 2, 1, 1),
            (3, 0, u64::MAX),
            (3, max, max),
        ] {
            let error = paginate(page, size, limit).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.message(), "page is out of range");
            assert_eq!(error.field_violations()[0].field, "page");
        }
    }

    #[test]
    fn op_parsing_is_case_insensitive() {
        assert_eq!(Op::parse(" GTE "), Some(Op::Gte));
        assert_eq!(Op::parse("between"), None);
    }

    #[test]
    fn column_types_map_to_kinds() {
        assert_eq!(kind_of(&ColumnType::Timestamp), Some(Kind::DateTime));
        assert_eq!(kind_of(&ColumnType::Money(None)), Some(Kind::Decimal));
        assert_eq!(kind_of(&ColumnType::BigUnsigned), Some(Kind::Integer));
        assert_eq!(kind_of(&ColumnType::Char(None)), Some(Kind::Text));
        assert_eq!(kind_of(&ColumnType::Blob), None);
    }

    #[test]
    fn list_params_default_to_the_first_page() {
        let params = ListParams::default();
        assert_eq!(params.page, 0);
        assert!(params.filters.is_empty());
        assert_eq!(
            start_of_day(parse_day("2024-01-02").unwrap())
                .unwrap()
                .to_string(),
            "2024-01-02 00:00:00"
        );
    }
}
