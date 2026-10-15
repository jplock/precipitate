//! How the store runs SQL: every statement is built with sea-query and
//! bound in one step, and typed columns are parsed in one place.

use sea_query::SqliteQueryBuilder;
use sea_query_sqlx::{SqlxBinder, SqlxValues};
use sqlx::Row;
use sqlx::sqlite::{Sqlite, SqliteRow};
use std::str::FromStr;

use crate::DbError;

/// A sea-query statement ready to run.
pub(crate) trait Bound {
    fn bound(&self) -> sqlx::query::Query<'static, Sqlite, SqlxValues>;
}

impl<T: SqlxBinder> Bound for T {
    fn bound(&self) -> sqlx::query::Query<'static, Sqlite, SqlxValues> {
        let (sql, values) = self.build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
    }
}

/// Columns stored as text and read back as their own types: dates,
/// timestamps, enums and names.
pub(crate) trait RowExt {
    fn parsed<T>(&self, column: &str) -> Result<T, DbError>
    where
        T: FromStr,
        DbError: From<T::Err>;

    fn parsed_opt<T>(&self, column: &str) -> Result<Option<T>, DbError>
    where
        T: FromStr,
        DbError: From<T::Err>;
}

impl RowExt for SqliteRow {
    fn parsed<T>(&self, column: &str) -> Result<T, DbError>
    where
        T: FromStr,
        DbError: From<T::Err>,
    {
        Ok(self.try_get::<String, _>(column)?.parse()?)
    }

    fn parsed_opt<T>(&self, column: &str) -> Result<Option<T>, DbError>
    where
        T: FromStr,
        DbError: From<T::Err>,
    {
        self.try_get::<Option<String>, _>(column)?
            .map(|s| s.parse().map_err(DbError::from))
            .transpose()
    }
}
