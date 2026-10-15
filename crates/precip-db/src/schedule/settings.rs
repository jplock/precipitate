//! Stored settings, including the global scheduling rules.

use super::parse_date;
use crate::kinds::Setting;
use crate::query::Bound;
use crate::schema::Settings;
use crate::{DbError, Store};
use precip_engine::schedule::GlobalRules;
use sea_query::{Expr, ExprTrait, OnConflict, Query};
use sqlx::{Row, SqliteConnection};

impl Store {
    pub(crate) async fn setting(
        &self,
        key: Setting,
    ) -> Result<Option<String>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::setting_in(&mut conn, key).await
    }

    pub(crate) async fn setting_in(
        conn: &mut SqliteConnection,
        key: Setting,
    ) -> Result<Option<String>, DbError> {
        let row = (Query::select()
            .column(Settings::Value)
            .from(Settings::Table)
            .and_where(Expr::col(Settings::Key).eq(key.as_str())))
        .bound()
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(|r| r.try_get::<String, _>("value")).transpose()?)
    }

    pub(crate) async fn set_setting(
        conn: &mut SqliteConnection,
        key: Setting,
        value: &str,
    ) -> Result<(), DbError> {
        (Query::insert()
            .into_table(Settings::Table)
            .columns([Settings::Key, Settings::Value])
            .values_panic([key.as_str().into(), value.into()])
            .on_conflict(
                OnConflict::column(Settings::Key)
                    .update_column(Settings::Value)
                    .to_owned(),
            ))
        .bound()
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// The scan selection last used, as JSON of the scanner's options,
    /// so a headless rescan repeats what was chosen in the interface.
    pub async fn saved_scan_options(&self) -> Result<Option<String>, DbError> {
        self.setting(Setting::ScanOptions).await
    }

    pub async fn save_scan_options(&self, json: &str) -> Result<(), DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::set_setting(&mut conn, Setting::ScanOptions, json).await
    }

    /// The sizing profile the workspace sizes with.
    pub async fn sizing_profile(&self) -> Result<Option<String>, DbError> {
        self.setting(Setting::SizingProfile).await
    }

    pub async fn set_sizing_profile(&self, name: &str) -> Result<(), DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::set_setting(&mut conn, Setting::SizingProfile, name).await
    }

    pub async fn global_rules(&self) -> Result<GlobalRules, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::global_rules_in(&mut conn).await
    }

    pub(crate) async fn global_rules_in(
        conn: &mut SqliteConnection,
    ) -> Result<GlobalRules, DbError> {
        let d = GlobalRules::default();
        let num = |v: Option<String>, default: u32| {
            v.and_then(|s| s.parse().ok()).unwrap_or(default)
        };
        let target_end =
            match Self::setting_in(conn, Setting::TargetEnd).await? {
                Some(s) if !s.is_empty() => Some(parse_date(&s)?),
                _ => None,
            };
        Ok(GlobalRules {
            per_day: num(
                Self::setting_in(conn, Setting::PerDay).await?,
                d.per_day,
            ),
            per_week: num(
                Self::setting_in(conn, Setting::PerWeek).await?,
                d.per_week,
            ),
            per_month: num(
                Self::setting_in(conn, Setting::PerMonth).await?,
                d.per_month,
            ),
            weekdays: Self::setting_in(conn, Setting::Weekdays)
                .await?
                .unwrap_or_default()
                .parse()?,
            target_end,
            nonprod_before_prod: Self::setting_in(
                conn,
                Setting::NonprodBeforeProd,
            )
            .await?
            .is_none_or(|v| v == "true"),
        })
    }
}
