//! Schedule persistence: rules, blackouts, status, events and versions.

use crate::schema::{
    DispositionHistory, MigrationEvents, MigrationStatus, PlanEvents,
    ResourceVersions, ScheduleBlackouts, ScheduleDependencies, ScheduleRules,
    ScheduleSlots, ScheduleVersions, ScheduleWaves, Settings, WorkloadAliases,
    WorkloadEnvironments, WorkloadMembers,
};
use crate::{DbError, Store};
use jiff::civil::{Date, Weekday};
use precip_engine::schedule::{
    Blackout, GlobalRules, PairInput, Schedule, Status, Wave,
};
use sea_query::{
    Alias, Expr, ExprTrait, Func, OnConflict, Order, Query, SqliteQueryBuilder,
    UnionType,
};
use sea_query_sqlx::SqlxBinder;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};

/// Build a query and bind it, the one way every statement here runs.
macro_rules! bound {
    ($stmt:expr) => {{
        let (sql, values) = $stmt.build_sqlx(SqliteQueryBuilder);
        sqlx::query_with(sqlx::AssertSqlSafe(sql), values)
    }};
}

fn parse_weekdays(s: &str) -> Vec<Weekday> {
    s.split(',')
        .filter_map(|t| match t.trim().to_lowercase().as_str() {
            "mon" | "monday" => Some(Weekday::Monday),
            "tue" | "tuesday" => Some(Weekday::Tuesday),
            "wed" | "wednesday" => Some(Weekday::Wednesday),
            "thu" | "thursday" => Some(Weekday::Thursday),
            "fri" | "friday" => Some(Weekday::Friday),
            "sat" | "saturday" => Some(Weekday::Saturday),
            "sun" | "sunday" => Some(Weekday::Sunday),
            _ => None,
        })
        .collect()
}

pub fn weekday_name(w: Weekday) -> &'static str {
    match w {
        Weekday::Monday => "mon",
        Weekday::Tuesday => "tue",
        Weekday::Wednesday => "wed",
        Weekday::Thursday => "thu",
        Weekday::Friday => "fri",
        Weekday::Saturday => "sat",
        Weekday::Sunday => "sun",
    }
}

fn parse_date(s: &str) -> Result<Date, DbError> {
    Ok(s.parse::<Date>()?)
}

/// One change to the plan. The log of these is the source of truth; the
/// rules, freeze, wave, dependency and status tables are projections of
/// it, and every applied event yields a new schedule version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanEvent {
    RulesSet {
        per_day: u32,
        per_week: u32,
        per_month: u32,
        /// Comma list of day names, empty for any day.
        weekdays: String,
        target_end: Option<Date>,
        nonprod_before_prod: bool,
    },
    BlackoutAdded {
        name: String,
        start: Date,
        end: Date,
        workload: Option<String>,
        environment: Option<String>,
    },
    BlackoutRemoved {
        name: String,
        start: Date,
        end: Date,
        workload: Option<String>,
        environment: Option<String>,
    },
    PairRuleSet {
        workload: String,
        environment: String,
        weekdays: String,
        earliest: Option<Date>,
        priority: i32,
    },
    PairPlanSet {
        workload: String,
        environment: String,
        wave: u32,
        duration_days: u32,
        weight: u32,
    },
    WaveSet {
        number: u32,
        name: String,
        target_end: Option<Date>,
    },
    /// The wave's pairs go back to wave 1.
    WaveRemoved {
        number: u32,
    },
    DependencyAdded {
        workload: String,
        environment: String,
        on_workload: String,
        on_environment: String,
    },
    DependencyRemoved {
        workload: String,
        environment: String,
        on_workload: String,
        on_environment: String,
    },
    StatusChanged {
        workload: String,
        environment: String,
        status: Status,
        date: Option<Date>,
        note: Option<String>,
    },
    DispositionSet {
        workload: String,
        environment: String,
        disposition: precip_types::Disposition,
        /// `user` for a direct change, `bulk` for a default applied to
        /// every pair.
        source: String,
        note: Option<String>,
    },
    DefaultDispositionSet {
        disposition: precip_types::Disposition,
        apply_to_existing: bool,
    },
    /// `alias` is really the workload `into`; resources move there on
    /// the next regroup.
    WorkloadMerged {
        alias: String,
        into: String,
    },
}

impl PlanEvent {
    fn rules(rules: &GlobalRules) -> Self {
        let days: Vec<&str> =
            rules.weekdays.iter().map(|w| weekday_name(*w)).collect();
        PlanEvent::RulesSet {
            per_day: rules.per_day,
            per_week: rules.per_week,
            per_month: rules.per_month,
            weekdays: days.join(","),
            target_end: rules.target_end,
            nonprod_before_prod: rules.nonprod_before_prod,
        }
    }

    fn blackout(b: &Blackout, added: bool) -> Self {
        let (workload, environment) = match &b.pair {
            Some((w, e)) => (Some(w.clone()), Some(e.clone())),
            None => (None, None),
        };
        if added {
            PlanEvent::BlackoutAdded {
                name: b.name.clone(),
                start: b.start,
                end: b.end,
                workload,
                environment,
            }
        } else {
            PlanEvent::BlackoutRemoved {
                name: b.name.clone(),
                start: b.start,
                end: b.end,
                workload,
                environment,
            }
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            PlanEvent::RulesSet { .. } => "rules_set",
            PlanEvent::BlackoutAdded { .. } => "blackout_added",
            PlanEvent::BlackoutRemoved { .. } => "blackout_removed",
            PlanEvent::PairRuleSet { .. } => "pair_rule_set",
            PlanEvent::PairPlanSet { .. } => "pair_plan_set",
            PlanEvent::WaveSet { .. } => "wave_set",
            PlanEvent::WaveRemoved { .. } => "wave_removed",
            PlanEvent::DependencyAdded { .. } => "dependency_added",
            PlanEvent::DependencyRemoved { .. } => "dependency_removed",
            PlanEvent::StatusChanged { .. } => "status_changed",
            PlanEvent::DispositionSet { .. } => "disposition_set",
            PlanEvent::DefaultDispositionSet { .. } => {
                "default_disposition_set"
            }
            PlanEvent::WorkloadMerged { .. } => "workload_merged",
        }
    }

    /// The change in a sentence, for the history.
    pub fn describe(&self) -> String {
        let span = |w: &Option<String>, e: &Option<String>| match (w, e) {
            (Some(w), Some(e)) => format!(" for {w} / {e}"),
            _ => String::new(),
        };
        match self {
            PlanEvent::RulesSet {
                per_day,
                per_week,
                per_month,
                weekdays,
                target_end,
                nonprod_before_prod,
            } => format!(
                "rules: {per_day} per day, {per_week} per week, {per_month} per month, on {}{}{}",
                if weekdays.is_empty() { "any day" } else { weekdays },
                target_end
                    .map(|d| format!(", done by {d}"))
                    .unwrap_or_default(),
                if *nonprod_before_prod { ", non-prod first" } else { "" }
            ),
            PlanEvent::BlackoutAdded {
                name,
                start,
                end,
                workload,
                environment,
            } => {
                format!(
                    "freeze {name} from {start} to {end}{}",
                    span(workload, environment)
                )
            }
            PlanEvent::BlackoutRemoved {
                name,
                start,
                end,
                workload,
                environment,
            } => {
                format!(
                    "freeze {name} ({start} to {end}) removed{}",
                    span(workload, environment)
                )
            }
            PlanEvent::PairRuleSet {
                workload,
                environment,
                weekdays,
                earliest,
                priority,
            } => {
                format!(
                    "{workload} / {environment}: runs on {}{}, priority {priority}",
                    if weekdays.is_empty() {
                        "the allowed days"
                    } else {
                        weekdays
                    },
                    earliest
                        .map(|d| format!(", not before {d}"))
                        .unwrap_or_default()
                )
            }
            PlanEvent::PairPlanSet {
                workload,
                environment,
                wave,
                duration_days,
                weight,
            } => {
                format!(
                    "{workload} / {environment}: wave {wave}, {duration_days} day(s), weight {weight}"
                )
            }
            PlanEvent::WaveSet { number, name, target_end } => format!(
                "wave {number} named {}{}",
                if name.is_empty() { "(unnamed)" } else { name },
                target_end
                    .map(|d| format!(", done by {d}"))
                    .unwrap_or_default()
            ),
            PlanEvent::WaveRemoved { number } => {
                format!("wave {number} removed, its pairs back in wave 1")
            }
            PlanEvent::DependencyAdded {
                workload,
                environment,
                on_workload,
                on_environment,
            } => {
                format!(
                    "{workload} / {environment} waits for {on_workload} / {on_environment}"
                )
            }
            PlanEvent::DependencyRemoved {
                workload,
                environment,
                on_workload,
                on_environment,
            } => {
                format!(
                    "{workload} / {environment} no longer waits for {on_workload} / {on_environment}"
                )
            }
            PlanEvent::StatusChanged {
                workload,
                environment,
                status,
                date,
                note,
            } => format!(
                "{workload} / {environment}: {}{}{}",
                match status {
                    Status::Proposed => "back to proposed",
                    Status::Confirmed => "date agreed",
                    Status::Missed => "didn't happen",
                    Status::Migrated => "migrated",
                    Status::RolledBack => "rolled back",
                },
                date.map(|d| format!(" ({d})")).unwrap_or_default(),
                note.as_ref().map(|n| format!(", {n}")).unwrap_or_default()
            ),
            PlanEvent::DispositionSet {
                workload,
                environment,
                disposition,
                source,
                note,
            } => {
                format!(
                    "{workload} / {environment}: {}{}{}",
                    disposition_name(*disposition),
                    if source == "bulk" { " (default applied)" } else { "" },
                    note.as_ref().map(|n| format!(", {n}")).unwrap_or_default()
                )
            }
            PlanEvent::DefaultDispositionSet {
                disposition,
                apply_to_existing,
            } => format!(
                "new pairs start as {}{}",
                disposition_name(*disposition),
                if *apply_to_existing {
                    ", applied to untouched pairs"
                } else {
                    ""
                }
            ),
            PlanEvent::WorkloadMerged { alias, into } => {
                format!("{alias} is part of {into}")
            }
        }
    }
}

fn disposition_name(d: precip_types::Disposition) -> String {
    serde_json::to_value(d)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// An event as stored, with the schedule version it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEventRecord {
    pub id: i64,
    pub at: jiff::Timestamp,
    pub event: PlanEvent,
    pub version_id: Option<i64>,
    /// Projected end of the schedule after this event.
    pub ends: Option<Date>,
}

/// A pair's current standing, with when it was confirmed or migrated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairStatus {
    pub status: Status,
    pub confirmed_date: Option<Date>,
    pub migrated_date: Option<Date>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationEvent {
    pub event: String,
    pub date: Option<Date>,
    pub at: jiff::Timestamp,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleVersion {
    pub id: i64,
    pub computed_at: jiff::Timestamp,
    pub as_of: Date,
    pub target_end: Option<Date>,
    pub ends: Option<Date>,
    pub late: i64,
    pub trigger: String,
}

impl Store {
    async fn setting(&self, key: &str) -> Result<Option<String>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::setting_in(&mut conn, key).await
    }

    async fn setting_in(
        conn: &mut SqliteConnection,
        key: &str,
    ) -> Result<Option<String>, DbError> {
        let row = bound!(
            Query::select()
                .column(Settings::Value)
                .from(Settings::Table)
                .and_where(Expr::col(Settings::Key).eq(key))
        )
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    async fn set_setting(
        conn: &mut SqliteConnection,
        key: &str,
        value: &str,
    ) -> Result<(), DbError> {
        bound!(
            Query::insert()
                .into_table(Settings::Table)
                .columns([Settings::Key, Settings::Value])
                .values_panic([key.into(), value.into()])
                .on_conflict(
                    OnConflict::column(Settings::Key)
                        .update_column(Settings::Value)
                        .to_owned(),
                )
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// The scan selection last used, as JSON of the scanner's options,
    /// so a headless rescan repeats what was chosen in the interface.
    pub async fn saved_scan_options(&self) -> Result<Option<String>, DbError> {
        self.setting("scan.options").await
    }

    pub async fn save_scan_options(&self, json: &str) -> Result<(), DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::set_setting(&mut conn, "scan.options", json).await
    }

    /// The sizing profile the workspace sizes with.
    pub async fn sizing_profile(&self) -> Result<Option<String>, DbError> {
        self.setting("sizing.profile").await
    }

    pub async fn set_sizing_profile(&self, name: &str) -> Result<(), DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::set_setting(&mut conn, "sizing.profile", name).await
    }

    pub async fn global_rules(&self) -> Result<GlobalRules, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::global_rules_in(&mut conn).await
    }

    async fn global_rules_in(
        conn: &mut SqliteConnection,
    ) -> Result<GlobalRules, DbError> {
        let d = GlobalRules::default();
        let num = |v: Option<String>, default: u32| {
            v.and_then(|s| s.parse().ok()).unwrap_or(default)
        };
        let target_end =
            match Self::setting_in(conn, "schedule.target_end").await? {
                Some(s) if !s.is_empty() => Some(parse_date(&s)?),
                _ => None,
            };
        Ok(GlobalRules {
            per_day: num(
                Self::setting_in(conn, "schedule.per_day").await?,
                d.per_day,
            ),
            per_week: num(
                Self::setting_in(conn, "schedule.per_week").await?,
                d.per_week,
            ),
            per_month: num(
                Self::setting_in(conn, "schedule.per_month").await?,
                d.per_month,
            ),
            weekdays: parse_weekdays(
                &Self::setting_in(conn, "schedule.weekdays")
                    .await?
                    .unwrap_or_default(),
            ),
            target_end,
            nonprod_before_prod: Self::setting_in(
                conn,
                "schedule.nonprod_before_prod",
            )
            .await?
            .is_none_or(|v| v == "true"),
        })
    }

    async fn project_rules(
        conn: &mut SqliteConnection,
        rules: &GlobalRules,
    ) -> Result<(), DbError> {
        Self::set_setting(conn, "schedule.per_day", &rules.per_day.to_string())
            .await?;
        Self::set_setting(
            conn,
            "schedule.per_week",
            &rules.per_week.to_string(),
        )
        .await?;
        Self::set_setting(
            conn,
            "schedule.per_month",
            &rules.per_month.to_string(),
        )
        .await?;
        let days: Vec<&str> =
            rules.weekdays.iter().map(|w| weekday_name(*w)).collect();
        Self::set_setting(conn, "schedule.weekdays", &days.join(",")).await?;
        Self::set_setting(
            conn,
            "schedule.target_end",
            &rules.target_end.map(|d| d.to_string()).unwrap_or_default(),
        )
        .await?;
        Self::set_setting(
            conn,
            "schedule.nonprod_before_prod",
            &rules.nonprod_before_prod.to_string(),
        )
        .await?;
        Ok(())
    }

    async fn project_blackout_added(
        conn: &mut SqliteConnection,
        b: &Blackout,
    ) -> Result<(), DbError> {
        bound!(
            Query::insert()
                .into_table(ScheduleBlackouts::Table)
                .columns([
                    ScheduleBlackouts::Name,
                    ScheduleBlackouts::StartDate,
                    ScheduleBlackouts::EndDate,
                    ScheduleBlackouts::Workload,
                    ScheduleBlackouts::Environment,
                ])
                .values_panic([
                    b.name.clone().into(),
                    b.start.to_string().into(),
                    b.end.to_string().into(),
                    b.pair.as_ref().map(|p| p.0.clone()).into(),
                    b.pair.as_ref().map(|p| p.1.clone()).into(),
                ])
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn project_blackout_removed(
        conn: &mut SqliteConnection,
        b: &Blackout,
    ) -> Result<(), DbError> {
        let mut q = Query::delete();
        q.from_table(ScheduleBlackouts::Table)
            .and_where(Expr::col(ScheduleBlackouts::Name).eq(b.name.clone()))
            .and_where(
                Expr::col(ScheduleBlackouts::StartDate).eq(b.start.to_string()),
            )
            .and_where(
                Expr::col(ScheduleBlackouts::EndDate).eq(b.end.to_string()),
            );
        match &b.pair {
            Some((w, e)) => {
                q.and_where(
                    Expr::col(ScheduleBlackouts::Workload).eq(w.clone()),
                )
                .and_where(
                    Expr::col(ScheduleBlackouts::Environment).eq(e.clone()),
                );
            }
            None => {
                q.and_where(Expr::col(ScheduleBlackouts::Workload).is_null())
                    .and_where(
                        Expr::col(ScheduleBlackouts::Environment).is_null(),
                    );
            }
        }
        bound!(q).execute(&mut *conn).await?;
        Ok(())
    }

    pub async fn blackouts(&self) -> Result<Vec<Blackout>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::blackouts_in(&mut conn).await
    }

    async fn blackouts_in(
        conn: &mut SqliteConnection,
    ) -> Result<Vec<Blackout>, DbError> {
        let rows = bound!(
            Query::select()
                .columns([
                    ScheduleBlackouts::Name,
                    ScheduleBlackouts::StartDate,
                    ScheduleBlackouts::EndDate,
                    ScheduleBlackouts::Workload,
                    ScheduleBlackouts::Environment,
                ])
                .from(ScheduleBlackouts::Table)
                .order_by(ScheduleBlackouts::StartDate, Order::Asc)
                .order_by(ScheduleBlackouts::Name, Order::Asc)
        )
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .map(|r| {
                let w: Option<String> = r.get("workload");
                let e: Option<String> = r.get("environment");
                Ok(Blackout {
                    name: r.get("name"),
                    start: parse_date(&r.get::<String, _>("start_date"))?,
                    end: parse_date(&r.get::<String, _>("end_date"))?,
                    pair: w.zip(e),
                })
            })
            .collect()
    }

    async fn project_pair_rule(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
        weekdays: &[Weekday],
        earliest: Option<Date>,
        priority: i32,
    ) -> Result<(), DbError> {
        let days: Vec<&str> =
            weekdays.iter().map(|w| weekday_name(*w)).collect();
        bound!(
            Query::insert()
                .into_table(ScheduleRules::Table)
                .columns([
                    ScheduleRules::Workload,
                    ScheduleRules::Environment,
                    ScheduleRules::Weekdays,
                    ScheduleRules::Earliest,
                    ScheduleRules::Priority,
                ])
                .values_panic([
                    workload.into(),
                    environment.into(),
                    days.join(",").into(),
                    earliest.map(|d| d.to_string()).into(),
                    priority.into(),
                ])
                .on_conflict(
                    OnConflict::columns([
                        ScheduleRules::Workload,
                        ScheduleRules::Environment,
                    ])
                    .update_columns([
                        ScheduleRules::Weekdays,
                        ScheduleRules::Earliest,
                        ScheduleRules::Priority,
                    ])
                    .to_owned(),
                )
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Which wave a pair is in, how many consecutive days its migration
    /// takes, and how many capacity units it uses on each of them.
    async fn project_pair_plan(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
        wave: u32,
        duration_days: u32,
        weight: u32,
    ) -> Result<(), DbError> {
        if wave == 0 || duration_days == 0 || weight == 0 {
            return Err(DbError::Invalid(
                "wave, duration and weight must be at least 1".into(),
            ));
        }
        bound!(
            Query::insert()
                .into_table(ScheduleRules::Table)
                .columns([
                    ScheduleRules::Workload,
                    ScheduleRules::Environment,
                    ScheduleRules::Wave,
                    ScheduleRules::DurationDays,
                    ScheduleRules::Weight,
                ])
                .values_panic([
                    workload.into(),
                    environment.into(),
                    wave.into(),
                    duration_days.into(),
                    weight.into(),
                ])
                .on_conflict(
                    OnConflict::columns([
                        ScheduleRules::Workload,
                        ScheduleRules::Environment,
                    ])
                    .update_columns([
                        ScheduleRules::Wave,
                        ScheduleRules::DurationDays,
                        ScheduleRules::Weight,
                    ])
                    .to_owned(),
                )
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn project_wave(
        conn: &mut SqliteConnection,
        number: u32,
        name: &str,
        target_end: Option<Date>,
    ) -> Result<(), DbError> {
        if number == 0 {
            return Err(DbError::Invalid("waves start at 1".into()));
        }
        bound!(
            Query::insert()
                .into_table(ScheduleWaves::Table)
                .columns([
                    ScheduleWaves::Wave,
                    ScheduleWaves::Name,
                    ScheduleWaves::TargetEnd,
                ])
                .values_panic([
                    number.into(),
                    name.into(),
                    target_end.map(|d| d.to_string()).into(),
                ])
                .on_conflict(
                    OnConflict::column(ScheduleWaves::Wave)
                        .update_columns([
                            ScheduleWaves::Name,
                            ScheduleWaves::TargetEnd,
                        ])
                        .to_owned(),
                )
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn project_wave_removed(
        conn: &mut SqliteConnection,
        number: u32,
    ) -> Result<(), DbError> {
        bound!(
            Query::update()
                .table(ScheduleRules::Table)
                .value(ScheduleRules::Wave, 1)
                .and_where(Expr::col(ScheduleRules::Wave).eq(number))
        )
        .execute(&mut *conn)
        .await?;
        bound!(
            Query::delete()
                .from_table(ScheduleWaves::Table)
                .and_where(Expr::col(ScheduleWaves::Wave).eq(number))
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Every wave number in use or named, in order, with its name and
    /// target when a row exists.
    pub async fn waves(&self) -> Result<Vec<Wave>, DbError> {
        // Every wave number in use, from the named waves, the pair plans,
        // and wave 1 which always exists.
        let in_use = Alias::new("w");
        let numbers = Query::select()
            .column(ScheduleWaves::Wave)
            .from(ScheduleWaves::Table)
            .union(
                UnionType::Distinct,
                Query::select()
                    .column(ScheduleRules::Wave)
                    .from(ScheduleRules::Table)
                    .to_owned(),
            )
            .union(
                UnionType::Distinct,
                Query::select().expr(Expr::val(1)).to_owned(),
            )
            .to_owned();
        let rows = bound!(
            Query::select()
                .column((in_use.clone(), ScheduleWaves::Wave))
                .column((ScheduleWaves::Table, ScheduleWaves::Name))
                .column((ScheduleWaves::Table, ScheduleWaves::TargetEnd))
                .from_subquery(numbers, in_use.clone())
                .left_join(
                    ScheduleWaves::Table,
                    Expr::col((ScheduleWaves::Table, ScheduleWaves::Wave))
                        .equals((in_use.clone(), ScheduleWaves::Wave)),
                )
                .order_by((in_use.clone(), ScheduleWaves::Wave), Order::Asc)
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let target: Option<String> = r.get("target_end");
                let name: Option<String> = r.get("name");
                Ok(Wave {
                    number: r.get::<i64, _>("wave").try_into().unwrap_or(1),
                    name: name.unwrap_or_default(),
                    target_end: target
                        .as_deref()
                        .map(parse_date)
                        .transpose()?,
                })
            })
            .collect()
    }

    async fn project_dependency_added(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
        on_workload: &str,
        on_environment: &str,
    ) -> Result<(), DbError> {
        bound!(
            Query::insert()
                .into_table(ScheduleDependencies::Table)
                .columns([
                    ScheduleDependencies::Workload,
                    ScheduleDependencies::Environment,
                    ScheduleDependencies::OnWorkload,
                    ScheduleDependencies::OnEnvironment,
                ])
                .values_panic([
                    workload.into(),
                    environment.into(),
                    on_workload.into(),
                    on_environment.into(),
                ])
                .on_conflict(
                    OnConflict::columns([
                        ScheduleDependencies::Workload,
                        ScheduleDependencies::Environment,
                        ScheduleDependencies::OnWorkload,
                        ScheduleDependencies::OnEnvironment,
                    ])
                    .do_nothing()
                    .to_owned(),
                )
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn project_dependency_removed(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
        on_workload: &str,
        on_environment: &str,
    ) -> Result<(), DbError> {
        bound!(
            Query::delete()
                .from_table(ScheduleDependencies::Table)
                .and_where(
                    Expr::col(ScheduleDependencies::Workload).eq(workload),
                )
                .and_where(
                    Expr::col(ScheduleDependencies::Environment)
                        .eq(environment),
                )
                .and_where(
                    Expr::col(ScheduleDependencies::OnWorkload).eq(on_workload),
                )
                .and_where(
                    Expr::col(ScheduleDependencies::OnEnvironment)
                        .eq(on_environment),
                )
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn pair_status(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<PairStatus, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::pair_status_in(&mut conn, workload, environment).await
    }

    async fn pair_status_in(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
    ) -> Result<PairStatus, DbError> {
        let row = bound!(
            Query::select()
                .columns([
                    MigrationStatus::Status,
                    MigrationStatus::ConfirmedDate,
                    MigrationStatus::MigratedDate,
                ])
                .from(MigrationStatus::Table)
                .and_where(Expr::col(MigrationStatus::Workload).eq(workload))
                .and_where(
                    Expr::col(MigrationStatus::Environment).eq(environment),
                )
        )
        .fetch_optional(&mut *conn)
        .await?;
        let Some(r) = row else {
            return Ok(PairStatus {
                status: Status::Proposed,
                confirmed_date: None,
                migrated_date: None,
            });
        };
        let status: String = r.get("status");
        let confirmed: Option<String> = r.get("confirmed_date");
        let migrated: Option<String> = r.get("migrated_date");
        Ok(PairStatus {
            status: serde_json::from_value(serde_json::Value::String(status))?,
            confirmed_date: confirmed.as_deref().map(parse_date).transpose()?,
            migrated_date: migrated.as_deref().map(parse_date).transpose()?,
        })
    }

    /// Record a status change and its event. Confirming requires a date
    /// that is today or later.
    #[expect(clippy::too_many_arguments, reason = "projection of one event")]
    async fn project_status(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
        status: Status,
        date: Option<Date>,
        today: Date,
        note: Option<&str>,
        at: jiff::Timestamp,
        validate: bool,
    ) -> Result<(), DbError> {
        if validate && status == Status::Confirmed {
            match date {
                Some(d) if d >= today => {}
                _ => {
                    return Err(DbError::Invalid(
                        "a confirmed date must be today or later".into(),
                    ));
                }
            }
        }
        let current = Self::pair_status_in(conn, workload, environment).await?;
        // Transitions that cannot mean anything are refused with a reason
        // the screen can show, so a slip of the hand changes nothing.
        let refuse = |why: &str| Err(DbError::Invalid(why.to_string()));
        if validate && status == Status::Confirmed {
            if let Some((w, e)) =
                Self::unfinished_dependencies_in(conn, workload, environment)
                    .await?
                    .first()
            {
                return refuse(&format!(
                    "it waits for {w} / {e}, which has not migrated; agree that date first"
                ));
            }
            let pairs = Self::schedule_inputs_in(conn).await?;
            let rules = Self::global_rules_in(conn).await?;
            let blackouts = Self::blackouts_in(conn).await?;
            if let (Some(me), Some(start)) = (
                pairs.iter().find(|p| {
                    p.workload == workload && p.environment == environment
                }),
                date,
            ) {
                if let Err(why) = precip_engine::schedule::check_agreed_date(
                    me, start, &pairs, &rules, &blackouts,
                ) {
                    return refuse(&why);
                }
            }
        }
        if validate {
            match (current.status, status) {
                (
                    Status::Proposed
                    | Status::Confirmed
                    | Status::Missed
                    | Status::RolledBack,
                    Status::Proposed | Status::Confirmed | Status::Migrated,
                )
                | (Status::Migrated, Status::Proposed | Status::RolledBack) => {
                }
                (Status::Confirmed, Status::Missed) => {
                    if current.confirmed_date.is_some_and(|d| d > today) {
                        return refuse(
                            "its confirmed date has not arrived; agree another date instead",
                        );
                    }
                }
                (
                    Status::Proposed | Status::Missed | Status::RolledBack,
                    Status::Missed,
                ) => {
                    return refuse(
                        "nothing was agreed for this pair; only a confirmed date can be missed",
                    );
                }
                (Status::Migrated, Status::Migrated) => {
                    return refuse("already migrated");
                }
                (Status::Migrated, Status::Confirmed | Status::Missed) => {
                    return refuse(
                        "already migrated; mark it rolled back first if it moved back",
                    );
                }
                (
                    Status::Proposed
                    | Status::Confirmed
                    | Status::Missed
                    | Status::RolledBack,
                    Status::RolledBack,
                ) => {
                    return refuse("only a migrated pair can be rolled back");
                }
            }
        }
        let name = serde_json::to_value(status)?
            .as_str()
            .unwrap_or_default()
            .to_string();
        let (confirmed, migrated) = match status {
            Status::Confirmed => (date, None),
            Status::Migrated => (None, date.or(Some(today))),
            Status::Proposed | Status::Missed | Status::RolledBack => {
                (None, None)
            }
        };
        bound!(
            Query::insert()
                .into_table(MigrationStatus::Table)
                .columns([
                    MigrationStatus::Workload,
                    MigrationStatus::Environment,
                    MigrationStatus::Status,
                    MigrationStatus::ConfirmedDate,
                    MigrationStatus::MigratedDate,
                ])
                .values_panic([
                    workload.into(),
                    environment.into(),
                    name.clone().into(),
                    confirmed.map(|d| d.to_string()).into(),
                    migrated.map(|d| d.to_string()).into(),
                ])
                .on_conflict(
                    OnConflict::columns([
                        MigrationStatus::Workload,
                        MigrationStatus::Environment,
                    ])
                    .update_columns([
                        MigrationStatus::Status,
                        MigrationStatus::ConfirmedDate,
                        MigrationStatus::MigratedDate,
                    ])
                    .to_owned(),
                )
        )
        .execute(&mut *conn)
        .await?;
        // A confirmed date never moves: a second confirmation replaces it
        // and is recorded as a reschedule.
        let event = match status {
            Status::Confirmed
                if current.status == Status::Confirmed
                    && current.confirmed_date != date =>
            {
                "reconfirmed"
            }
            Status::Confirmed => "confirmed",
            Status::Proposed => "unconfirmed",
            Status::Missed => "missed",
            Status::Migrated => "migrated",
            Status::RolledBack => "rolled_back",
        };
        bound!(
            Query::insert()
                .into_table(MigrationEvents::Table)
                .columns([
                    MigrationEvents::Workload,
                    MigrationEvents::Environment,
                    MigrationEvents::Event,
                    MigrationEvents::Date,
                    MigrationEvents::At,
                    MigrationEvents::Note,
                ])
                .values_panic([
                    workload.into(),
                    environment.into(),
                    event.into(),
                    confirmed
                        .or(migrated)
                        .or(date)
                        .map(|d| d.to_string())
                        .into(),
                    at.to_string().into(),
                    note.into(),
                ])
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn migration_events(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<MigrationEvent>, DbError> {
        let rows = bound!(
            Query::select()
                .columns([
                    MigrationEvents::Event,
                    MigrationEvents::Date,
                    MigrationEvents::At,
                    MigrationEvents::Note,
                ])
                .from(MigrationEvents::Table)
                .and_where(Expr::col(MigrationEvents::Workload).eq(workload))
                .and_where(
                    Expr::col(MigrationEvents::Environment).eq(environment),
                )
                .order_by(MigrationEvents::Id, Order::Asc)
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let date: Option<String> = r.get("date");
                Ok(MigrationEvent {
                    event: r.get("event"),
                    date: date.as_deref().map(parse_date).transpose()?,
                    at: r.get::<String, _>("at").parse()?,
                    note: r.get("note"),
                })
            })
            .collect()
    }

    /// The pairs this one waits for that have not migrated yet.
    pub async fn unfinished_dependencies(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::unfinished_dependencies_in(&mut conn, workload, environment).await
    }

    async fn unfinished_dependencies_in(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<(String, String)>, DbError> {
        let pairs = Self::schedule_inputs_in(conn).await?;
        let rules = Self::global_rules_in(conn).await?;
        let Some(me) = pairs
            .iter()
            .find(|p| p.workload == workload && p.environment == environment)
        else {
            return Ok(Vec::new());
        };
        Ok(precip_engine::schedule::dependencies_of(me, &pairs, &rules)
            .into_iter()
            .filter(|d| {
                pairs.iter().any(|p| {
                    (p.workload.as_str(), p.environment.as_str())
                        == (d.0.as_str(), d.1.as_str())
                        && p.status != Status::Migrated
                })
            })
            .collect())
    }

    /// Every pair that migrates (any disposition but retire and retain),
    /// with its rules, dependencies and status, as scheduler input.
    pub async fn schedule_inputs(&self) -> Result<Vec<PairInput>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::schedule_inputs_in(&mut conn).await
    }

    async fn schedule_inputs_in(
        conn: &mut SqliteConnection,
    ) -> Result<Vec<PairInput>, DbError> {
        let dispositions = Self::pair_dispositions_in(conn).await?;
        let mut out = Vec::new();
        for ((workload, environment), d) in dispositions {
            if matches!(
                d,
                precip_types::Disposition::Retire
                    | precip_types::Disposition::Retain
            ) {
                continue;
            }
            let rule = bound!(
                Query::select()
                    .columns([
                        ScheduleRules::Weekdays,
                        ScheduleRules::Earliest,
                        ScheduleRules::Priority,
                        ScheduleRules::Wave,
                        ScheduleRules::DurationDays,
                        ScheduleRules::Weight,
                    ])
                    .from(ScheduleRules::Table)
                    .and_where(
                        Expr::col(ScheduleRules::Workload).eq(workload.clone()),
                    )
                    .and_where(
                        Expr::col(ScheduleRules::Environment)
                            .eq(environment.clone()),
                    )
            )
            .fetch_optional(&mut *conn)
            .await?;
            let small = |r: &sqlx::sqlite::SqliteRow, col: &str| -> u32 {
                Ord::max(r.get::<i64, _>(col).try_into().unwrap_or(1), 1)
            };
            let (weekdays, earliest, priority, wave, duration_days, weight) =
                match rule {
                    Some(r) => {
                        let e: Option<String> = r.get("earliest");
                        (
                            parse_weekdays(&r.get::<String, _>("weekdays")),
                            e.as_deref().map(parse_date).transpose()?,
                            r.get::<i64, _>("priority").try_into().unwrap_or(0),
                            small(&r, "wave"),
                            small(&r, "duration_days"),
                            small(&r, "weight"),
                        )
                    }
                    None => (Vec::new(), None, 0, 1, 1, 1),
                };
            let deps = bound!(
                Query::select()
                    .columns([
                        ScheduleDependencies::OnWorkload,
                        ScheduleDependencies::OnEnvironment,
                    ])
                    .from(ScheduleDependencies::Table)
                    .and_where(
                        Expr::col(ScheduleDependencies::Workload)
                            .eq(workload.clone()),
                    )
                    .and_where(
                        Expr::col(ScheduleDependencies::Environment)
                            .eq(environment.clone()),
                    )
            )
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|r| (r.get("on_workload"), r.get("on_environment")))
            .collect();
            let status =
                Self::pair_status_in(conn, &workload, &environment).await?;
            out.push(PairInput {
                workload,
                environment,
                wave,
                priority,
                duration_days,
                weight,
                weekdays,
                earliest,
                status: status.status,
                locked: status.confirmed_date,
                depends_on: deps,
            });
        }
        Ok(out)
    }

    /// Compute the schedule from the stored rules and status, record it as
    /// a version, and write back any expired confirmations as missed.
    pub async fn compute_schedule(
        &self,
        today: Date,
        trigger: &str,
    ) -> Result<(i64, Schedule), DbError> {
        let mut tx = self.pool.begin().await?;
        let out = self.compute_version(&mut tx, today, trigger, None).await?;
        tx.commit().await?;
        Ok(out)
    }

    async fn compute_version(
        &self,
        conn: &mut SqliteConnection,
        today: Date,
        trigger: &str,
        event_id: Option<i64>,
    ) -> Result<(i64, Schedule), DbError> {
        let pairs = Self::schedule_inputs_in(conn).await?;
        let rules = Self::global_rules_in(conn).await?;
        let blackouts = Self::blackouts_in(conn).await?;
        let schedule =
            precip_engine::schedule::compute(&pairs, &rules, &blackouts, today);

        // A confirmed date that passed is itself a change to the plan,
        // recorded like any other, without computing again.
        for (w, e) in &schedule.expired {
            let prior = Self::pair_status_in(conn, w, e).await?;
            Self::record(
                conn,
                &PlanEvent::StatusChanged {
                    workload: w.clone(),
                    environment: e.clone(),
                    status: Status::Missed,
                    date: prior.confirmed_date,
                    note: Some(
                        "confirmed date passed without a migration".into(),
                    ),
                },
                jiff::Timestamp::now(),
                today,
            )
            .await?;
        }

        let id: i64 = bound!(
            Query::insert()
                .into_table(ScheduleVersions::Table)
                .columns([
                    ScheduleVersions::ComputedAt,
                    ScheduleVersions::AsOf,
                    ScheduleVersions::TargetEnd,
                    ScheduleVersions::Ends,
                    ScheduleVersions::Late,
                    ScheduleVersions::Trigger,
                    ScheduleVersions::Schedule,
                    ScheduleVersions::EventId,
                ])
                .values_panic([
                    jiff::Timestamp::now().to_string().into(),
                    today.to_string().into(),
                    rules.target_end.map(|d| d.to_string()).into(),
                    schedule.ends.map(|d| d.to_string()).into(),
                    i64::try_from(schedule.late.len())
                        .unwrap_or(i64::MAX)
                        .into(),
                    trigger.into(),
                    serde_json::to_string(&schedule)?.into(),
                    event_id.into(),
                ])
                .returning_col(ScheduleVersions::Id)
        )
        .fetch_one(&mut *conn)
        .await?
        .get("id");
        for s in &schedule.slots {
            let alts: Vec<String> =
                s.alternatives.iter().map(|d| d.to_string()).collect();
            bound!(
                Query::insert()
                    .into_table(ScheduleSlots::Table)
                    .columns([
                        ScheduleSlots::VersionId,
                        ScheduleSlots::Workload,
                        ScheduleSlots::Environment,
                        ScheduleSlots::Date,
                        ScheduleSlots::EndDate,
                        ScheduleSlots::Wave,
                        ScheduleSlots::Locked,
                        ScheduleSlots::Reason,
                        ScheduleSlots::Alternatives,
                    ])
                    .values_panic([
                        id.into(),
                        s.workload.clone().into(),
                        s.environment.clone().into(),
                        s.date.to_string().into(),
                        s.end.to_string().into(),
                        s.wave.into(),
                        i32::from(s.locked).into(),
                        s.reason.clone().into(),
                        alts.join(",").into(),
                    ])
            )
            .execute(&mut *conn)
            .await?;
        }
        if let Some(event_id) = event_id {
            bound!(
                Query::update()
                    .table(PlanEvents::Table)
                    .value(PlanEvents::VersionId, id)
                    .and_where(Expr::col(PlanEvents::Id).eq(event_id))
            )
            .execute(&mut *conn)
            .await?;
        }
        Ok((id, schedule))
    }

    /// The schedule of the latest version, as computed, without
    /// computing again.
    pub async fn latest_schedule(
        &self,
    ) -> Result<Option<(i64, Date, Schedule)>, DbError> {
        let row = bound!(
            Query::select()
                .columns([
                    ScheduleVersions::Id,
                    ScheduleVersions::AsOf,
                    ScheduleVersions::Schedule,
                ])
                .from(ScheduleVersions::Table)
                .order_by(ScheduleVersions::Id, Order::Desc)
                .limit(1)
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let json: Option<String> = r.get("schedule");
        let Some(json) = json else { return Ok(None) };
        Ok(Some((
            r.get("id"),
            parse_date(&r.get::<String, _>("as_of"))?,
            serde_json::from_str(&json)?,
        )))
    }

    // ---- the event log ----------------------------------------------

    /// Append an event and project it, without computing a schedule.
    async fn record(
        conn: &mut SqliteConnection,
        event: &PlanEvent,
        at: jiff::Timestamp,
        today: Date,
    ) -> Result<i64, DbError> {
        Self::project(conn, event, at, today, true).await?;
        let id: i64 = bound!(
            Query::insert()
                .into_table(PlanEvents::Table)
                .columns([PlanEvents::At, PlanEvents::Kind, PlanEvents::Body])
                .values_panic([
                    at.to_string().into(),
                    event.kind().into(),
                    serde_json::to_string(event)?.into(),
                ])
                .returning_col(PlanEvents::Id)
        )
        .fetch_one(&mut *conn)
        .await?
        .get("id");
        Ok(id)
    }

    /// Append an event, project it, and compute the schedule it leads
    /// to. Every change to the plan goes through here.
    pub async fn apply(
        &self,
        event: PlanEvent,
    ) -> Result<(i64, Schedule), DbError> {
        let today = self.today();
        self.apply_as_of(event, today).await
    }

    /// `apply` from a chosen day, for status changes that carry one.
    pub async fn apply_as_of(
        &self,
        event: PlanEvent,
        today: Date,
    ) -> Result<(i64, Schedule), DbError> {
        // The event, its projection and the version it leads to commit
        // together, so the log and the tables built from it cannot differ.
        let mut tx = self.pool.begin().await?;
        let id = Self::record(&mut tx, &event, jiff::Timestamp::now(), today)
            .await?;
        let out = self
            .compute_version(&mut tx, today, event.kind(), Some(id))
            .await?;
        tx.commit().await?;
        Ok(out)
    }

    async fn project(
        conn: &mut SqliteConnection,
        event: &PlanEvent,
        at: jiff::Timestamp,
        today: Date,
        validate: bool,
    ) -> Result<(), DbError> {
        match event {
            PlanEvent::RulesSet {
                per_day,
                per_week,
                per_month,
                weekdays,
                target_end,
                nonprod_before_prod,
            } => {
                Self::project_rules(
                    conn,
                    &GlobalRules {
                        per_day: *per_day,
                        per_week: *per_week,
                        per_month: *per_month,
                        weekdays: parse_weekdays(weekdays),
                        target_end: *target_end,
                        nonprod_before_prod: *nonprod_before_prod,
                    },
                )
                .await
            }
            PlanEvent::BlackoutAdded {
                name,
                start,
                end,
                workload,
                environment,
            } => {
                Self::project_blackout_added(
                    conn,
                    &Blackout {
                        name: name.clone(),
                        start: *start,
                        end: *end,
                        pair: workload.clone().zip(environment.clone()),
                    },
                )
                .await
            }
            PlanEvent::BlackoutRemoved {
                name,
                start,
                end,
                workload,
                environment,
            } => {
                Self::project_blackout_removed(
                    conn,
                    &Blackout {
                        name: name.clone(),
                        start: *start,
                        end: *end,
                        pair: workload.clone().zip(environment.clone()),
                    },
                )
                .await
            }
            PlanEvent::PairRuleSet {
                workload,
                environment,
                weekdays,
                earliest,
                priority,
            } => {
                Self::project_pair_rule(
                    conn,
                    workload,
                    environment,
                    &parse_weekdays(weekdays),
                    *earliest,
                    *priority,
                )
                .await
            }
            PlanEvent::PairPlanSet {
                workload,
                environment,
                wave,
                duration_days,
                weight,
            } => {
                Self::project_pair_plan(
                    conn,
                    workload,
                    environment,
                    *wave,
                    *duration_days,
                    *weight,
                )
                .await
            }
            PlanEvent::WaveSet { number, name, target_end } => {
                Self::project_wave(conn, *number, name, *target_end).await
            }
            PlanEvent::WaveRemoved { number } => {
                Self::project_wave_removed(conn, *number).await
            }
            PlanEvent::DependencyAdded {
                workload,
                environment,
                on_workload,
                on_environment,
            } => {
                Self::project_dependency_added(
                    conn,
                    workload,
                    environment,
                    on_workload,
                    on_environment,
                )
                .await
            }
            PlanEvent::DependencyRemoved {
                workload,
                environment,
                on_workload,
                on_environment,
            } => {
                Self::project_dependency_removed(
                    conn,
                    workload,
                    environment,
                    on_workload,
                    on_environment,
                )
                .await
            }
            PlanEvent::DispositionSet {
                workload,
                environment,
                disposition,
                source,
                note,
            } => {
                Self::project_disposition(
                    conn,
                    workload,
                    environment,
                    *disposition,
                    source,
                    note.as_deref(),
                    at,
                )
                .await
            }
            PlanEvent::DefaultDispositionSet {
                disposition,
                apply_to_existing,
            } => {
                Self::project_default_disposition(
                    conn,
                    *disposition,
                    *apply_to_existing,
                    at,
                )
                .await
            }
            PlanEvent::WorkloadMerged { alias, into } => {
                Self::project_merge(conn, alias, into).await
            }
            PlanEvent::StatusChanged {
                workload,
                environment,
                status,
                date,
                note,
            } => {
                Self::project_status(
                    conn,
                    workload,
                    environment,
                    *status,
                    *date,
                    today,
                    note.as_deref(),
                    at,
                    validate,
                )
                .await
            }
        }
    }

    /// Every event, oldest first, with the end date of the schedule it
    /// produced.
    pub async fn events(&self) -> Result<Vec<PlanEventRecord>, DbError> {
        let rows = bound!(
            Query::select()
                .column((PlanEvents::Table, PlanEvents::Id))
                .column((PlanEvents::Table, PlanEvents::At))
                .column((PlanEvents::Table, PlanEvents::Body))
                .column((PlanEvents::Table, PlanEvents::VersionId))
                .column((ScheduleVersions::Table, ScheduleVersions::Ends))
                .from(PlanEvents::Table)
                .left_join(
                    ScheduleVersions::Table,
                    Expr::col((ScheduleVersions::Table, ScheduleVersions::Id))
                        .equals((PlanEvents::Table, PlanEvents::VersionId)),
                )
                .order_by((PlanEvents::Table, PlanEvents::Id), Order::Asc)
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let ends: Option<String> = r.get("ends");
                Ok(PlanEventRecord {
                    id: r.get("id"),
                    at: r.get::<String, _>("at").parse()?,
                    event: serde_json::from_str(&r.get::<String, _>("body"))?,
                    version_id: r.get("version_id"),
                    ends: ends.as_deref().map(parse_date).transpose()?,
                })
            })
            .collect()
    }

    /// Rebuild every projection from the log and compute the schedule
    /// once at the end. Proves the log is the source of truth. The wipe
    /// and the re-projection commit together, so a failed replay leaves
    /// the tables as they were.
    pub async fn replay(&self) -> Result<(i64, Schedule), DbError> {
        let events = self.events().await?;
        let mut tx = self.pool.begin().await?;
        for stmt in [
            Query::delete().from_table(ScheduleBlackouts::Table).to_owned(),
            Query::delete().from_table(ScheduleRules::Table).to_owned(),
            Query::delete().from_table(ScheduleWaves::Table).to_owned(),
            Query::delete().from_table(ScheduleDependencies::Table).to_owned(),
            Query::delete().from_table(MigrationStatus::Table).to_owned(),
            Query::delete().from_table(MigrationEvents::Table).to_owned(),
        ] {
            bound!(stmt).execute(&mut *tx).await?;
        }
        Self::project_rules(&mut tx, &GlobalRules::default()).await?;
        // Dispositions start from the built-in default and the merges
        // from none; the log brings both back.
        Self::reset_dispositions(&mut tx).await?;
        for e in &events {
            // The log is accepted as written: each event passed the
            // checks of its day, and those checks may have changed since.
            let day = e.at.to_zoned(jiff::tz::TimeZone::UTC).date();
            Self::project(&mut tx, &e.event, e.at, day, false).await?;
        }
        tx.commit().await?;
        if let Some(latest) = self.list_scans().await?.into_iter().next() {
            self.group_scan(
                &latest.id,
                &precip_types::GroupingConfig::default(),
            )
            .await?;
        }
        self.compute_schedule(self.today(), "replay").await
    }

    /// Every pair back to the built-in default, user and bulk history
    /// cleared, aliases cleared; only the log can set them again.
    async fn reset_dispositions(
        conn: &mut SqliteConnection,
    ) -> Result<(), DbError> {
        let builtin = disposition_name(precip_types::Disposition::Rehost);
        bound!(
            Query::update()
                .table(Settings::Table)
                .value(Settings::Value, builtin.clone())
                .and_where(
                    Expr::col(Settings::Key).eq(crate::DEFAULT_DISPOSITION_KEY)
                )
        )
        .execute(&mut *conn)
        .await?;
        bound!(
            Query::update()
                .table(WorkloadEnvironments::Table)
                .value(WorkloadEnvironments::Disposition, builtin)
        )
        .execute(&mut *conn)
        .await?;
        bound!(
            Query::delete().from_table(DispositionHistory::Table).and_where(
                Expr::col(DispositionHistory::Source).ne(crate::SOURCE_DEFAULT)
            )
        )
        .execute(&mut *conn)
        .await?;
        bound!(Query::delete().from_table(WorkloadAliases::Table))
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    fn today(&self) -> Date {
        self.today_override
            .lock()
            .ok()
            .and_then(|t| *t)
            .unwrap_or_else(|| jiff::Zoned::now().date())
    }

    /// Pin "today" so schedules computed by `apply` are reproducible.
    pub fn freeze_today(&self, day: Date) {
        if let Ok(mut t) = self.today_override.lock() {
            *t = Some(day);
        }
    }

    // ---- the public mutators, each one event ------------------------

    pub async fn set_disposition(
        &self,
        workload: &str,
        environment: &str,
        disposition: precip_types::Disposition,
    ) -> Result<(), DbError> {
        self.set_disposition_with(
            workload,
            environment,
            disposition,
            "user",
            None,
        )
        .await
    }

    /// A change to the value the pair already has is not recorded.
    pub async fn set_disposition_with(
        &self,
        workload: &str,
        environment: &str,
        disposition: precip_types::Disposition,
        source: &str,
        note: Option<&str>,
    ) -> Result<(), DbError> {
        if self.disposition_of(workload, environment).await?
            == Some(disposition)
        {
            return Ok(());
        }
        self.apply(PlanEvent::DispositionSet {
            workload: workload.into(),
            environment: environment.into(),
            disposition,
            source: source.into(),
            note: note.map(str::to_string),
        })
        .await
        .map(|_| ())
    }

    /// Change the default for pairs seen from now on. Existing pairs keep
    /// theirs unless `apply_to_existing` is set, which overwrites every
    /// pair, for example to start from "replatform everything".
    pub async fn set_default_disposition(
        &self,
        disposition: precip_types::Disposition,
        apply_to_existing: bool,
    ) -> Result<(), DbError> {
        self.apply(PlanEvent::DefaultDispositionSet {
            disposition,
            apply_to_existing,
        })
        .await
        .map(|_| ())
    }

    /// Record that `alias` is really `workload`; the next regroup moves
    /// its resources.
    pub async fn merge_workload(
        &self,
        alias: &str,
        workload: &str,
    ) -> Result<(), DbError> {
        self.apply(PlanEvent::WorkloadMerged {
            alias: alias.into(),
            into: workload.into(),
        })
        .await
        .map(|_| ())
    }

    pub async fn set_global_rules(
        &self,
        rules: &GlobalRules,
    ) -> Result<(), DbError> {
        self.apply(PlanEvent::rules(rules)).await.map(|_| ())
    }

    /// Add the chosen holidays of a year as freezes, one each.
    pub async fn add_holidays(
        &self,
        holidays: &[precip_engine::schedule::holidays::Holiday],
    ) -> Result<(), DbError> {
        for h in holidays {
            self.add_blackout(&Blackout {
                name: h.name.to_string(),
                start: h.date,
                end: h.date,
                pair: None,
            })
            .await?;
        }
        Ok(())
    }

    pub async fn add_blackout(&self, b: &Blackout) -> Result<(), DbError> {
        self.apply(PlanEvent::blackout(b, true)).await.map(|_| ())
    }

    pub async fn remove_blackout(&self, b: &Blackout) -> Result<(), DbError> {
        self.apply(PlanEvent::blackout(b, false)).await.map(|_| ())
    }

    pub async fn set_pair_rule(
        &self,
        workload: &str,
        environment: &str,
        weekdays: &[Weekday],
        earliest: Option<Date>,
        priority: i32,
    ) -> Result<(), DbError> {
        let days: Vec<&str> =
            weekdays.iter().map(|w| weekday_name(*w)).collect();
        self.apply(PlanEvent::PairRuleSet {
            workload: workload.into(),
            environment: environment.into(),
            weekdays: days.join(","),
            earliest,
            priority,
        })
        .await
        .map(|_| ())
    }

    pub async fn set_pair_plan(
        &self,
        workload: &str,
        environment: &str,
        wave: u32,
        duration_days: u32,
        weight: u32,
    ) -> Result<(), DbError> {
        self.apply(PlanEvent::PairPlanSet {
            workload: workload.into(),
            environment: environment.into(),
            wave,
            duration_days,
            weight,
        })
        .await
        .map(|_| ())
    }

    pub async fn set_wave(
        &self,
        number: u32,
        name: &str,
        target_end: Option<Date>,
    ) -> Result<(), DbError> {
        self.apply(PlanEvent::WaveSet { number, name: name.into(), target_end })
            .await
            .map(|_| ())
    }

    /// Remove a wave; its pairs go back to wave 1, which cannot be
    /// removed.
    pub async fn remove_wave(&self, number: u32) -> Result<usize, DbError> {
        if number <= 1 {
            return Err(DbError::Invalid(
                "wave 1 is where pairs start; it cannot be removed".into(),
            ));
        }
        let moved = self
            .schedule_inputs()
            .await?
            .iter()
            .filter(|p| p.wave == number)
            .count();
        self.apply(PlanEvent::WaveRemoved { number }).await?;
        Ok(moved)
    }

    pub async fn add_dependency(
        &self,
        workload: &str,
        environment: &str,
        on_workload: &str,
        on_environment: &str,
    ) -> Result<(), DbError> {
        self.apply(PlanEvent::DependencyAdded {
            workload: workload.into(),
            environment: environment.into(),
            on_workload: on_workload.into(),
            on_environment: on_environment.into(),
        })
        .await
        .map(|_| ())
    }

    pub async fn remove_dependency(
        &self,
        workload: &str,
        environment: &str,
        on_workload: &str,
        on_environment: &str,
    ) -> Result<(), DbError> {
        self.apply(PlanEvent::DependencyRemoved {
            workload: workload.into(),
            environment: environment.into(),
            on_workload: on_workload.into(),
            on_environment: on_environment.into(),
        })
        .await
        .map(|_| ())
    }

    /// Record a status change as of `today`. Confirming needs a date that
    /// is today or later; senseless transitions are refused.
    pub async fn set_pair_status(
        &self,
        workload: &str,
        environment: &str,
        status: Status,
        date: Option<Date>,
        today: Date,
        note: Option<&str>,
    ) -> Result<(), DbError> {
        self.apply_as_of(
            PlanEvent::StatusChanged {
                workload: workload.into(),
                environment: environment.into(),
                status,
                date,
                note: note.map(str::to_string),
            },
            today,
        )
        .await
        .map(|_| ())
    }

    pub async fn schedule_versions(
        &self,
    ) -> Result<Vec<ScheduleVersion>, DbError> {
        let rows = bound!(
            Query::select()
                .columns([
                    ScheduleVersions::Id,
                    ScheduleVersions::ComputedAt,
                    ScheduleVersions::AsOf,
                    ScheduleVersions::TargetEnd,
                    ScheduleVersions::Ends,
                    ScheduleVersions::Late,
                    ScheduleVersions::Trigger,
                ])
                .from(ScheduleVersions::Table)
                .order_by(ScheduleVersions::Id, Order::Asc)
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let target: Option<String> = r.get("target_end");
                let ends: Option<String> = r.get("ends");
                Ok(ScheduleVersion {
                    id: r.get("id"),
                    computed_at: r.get::<String, _>("computed_at").parse()?,
                    as_of: parse_date(&r.get::<String, _>("as_of"))?,
                    target_end: target
                        .as_deref()
                        .map(parse_date)
                        .transpose()?,
                    ends: ends.as_deref().map(parse_date).transpose()?,
                    late: r.get("late"),
                    trigger: r.get("trigger"),
                })
            })
            .collect()
    }
}

/// Why an agreed date moved. The same list in every view, so the
/// reschedule rate can be split by cause.
pub const RESCHEDULE_REASONS: [&str; 5] = [
    "owners asked to move it",
    "technical problem found",
    "freeze or holiday added",
    "waiting on another migration",
    "not ready in time",
];

/// Headline numbers for the migration, derived from the versions, slots,
/// events and status tables. Rates are in `0.0..=1.0`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    pub total: usize,
    pub migrated: usize,
    pub confirmed: usize,
    pub proposed: usize,
    pub missed: usize,
    pub rolled_back: usize,
    /// Pairs with a rolled_back event over pairs that ever migrated.
    pub rollback_rate: f64,
    /// Pairs that have had a confirmed date at some point.
    pub scheduled: usize,
    /// Pairs whose confirmed date was missed or replaced by another.
    pub rescheduled: usize,
    /// Rescheduled over scheduled.
    pub reschedule_rate: f64,
    /// Migrated pairs whose first confirmed date held, over migrated.
    pub first_time_success_rate: f64,
    /// Confirmed over outstanding (not migrated).
    pub confirmation_rate: f64,
    pub projected_end: Option<Date>,
    pub first_projected_end: Option<Date>,
    /// Days the projected end moved since the first version. Positive is
    /// later.
    pub end_drift_days: i64,
    /// Mean days between a migrated pair's first confirmed date and its
    /// migration date.
    pub average_slip_days: f64,
    /// Migrations completed in the last 30 days, and the capacity the
    /// rules allowed in that window.
    pub migrated_last_30_days: usize,
    pub capacity_last_30_days: u32,
    pub target_end: Option<Date>,
    pub waves: Vec<WaveMetrics>,
    /// Missed and re-agreed dates by reason, most common first. A move
    /// recorded without a reason counts under "no reason given".
    pub reschedule_reasons: Vec<(String, usize)>,
    /// How the projected end moved: one point per day a schedule was
    /// computed, the last version of that day.
    pub end_history: Vec<(Date, Date)>,
    /// Mean days between a migrated pair's first proposed date and the
    /// day it migrated. Positive means the first plan was optimistic.
    pub forecast_error_days: f64,
    /// Migrated pairs the first plan placed within a week of the day
    /// they migrated, over migrated pairs with a first plan.
    pub forecast_within_week_rate: f64,
}

/// Progress of one wave.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaveMetrics {
    pub number: u32,
    pub name: String,
    pub target_end: Option<Date>,
    pub total: usize,
    pub migrated: usize,
    /// Last day of the wave in the latest schedule, when anything is
    /// still to run.
    pub projected_end: Option<Date>,
}

/// A migrated pair whose compute is still present in the latest scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub workload: String,
    pub environment: String,
    /// Resources still placed in the pair, except terminated instances.
    pub remaining: usize,
}

impl Store {
    /// Migrated pairs that still have resources in the given scan. A
    /// migration that left things behind is either incomplete or was
    /// never done.
    pub async fn verify_migrated(
        &self,
        scan_id: &str,
    ) -> Result<Vec<Verification>, DbError> {
        let seq = self.seq_of(scan_id).await?;
        let state = Func::cust(Alias::new("json_extract"))
            .arg(Expr::col((ResourceVersions::Table, ResourceVersions::Body)))
            .arg(Expr::val("$.state"));
        let rows = bound!(
            Query::select()
                .column((WorkloadMembers::Table, WorkloadMembers::Workload))
                .column((WorkloadMembers::Table, WorkloadMembers::Environment))
                .expr_as(
                    Expr::col((WorkloadMembers::Table, WorkloadMembers::Arn))
                        .count(),
                    Alias::new("remaining"),
                )
                .from(WorkloadMembers::Table)
                .inner_join(
                    MigrationStatus::Table,
                    Expr::col((
                        MigrationStatus::Table,
                        MigrationStatus::Workload
                    ))
                    .equals((WorkloadMembers::Table, WorkloadMembers::Workload))
                    .and(
                        Expr::col((
                            MigrationStatus::Table,
                            MigrationStatus::Environment,
                        ))
                        .equals((
                            WorkloadMembers::Table,
                            WorkloadMembers::Environment,
                        )),
                    ),
                )
                .inner_join(
                    ResourceVersions::Table,
                    Expr::col((ResourceVersions::Table, ResourceVersions::Arn))
                        .equals((WorkloadMembers::Table, WorkloadMembers::Arn))
                        .and(
                            Expr::col((
                                ResourceVersions::Table,
                                ResourceVersions::FirstSeenSeq,
                            ))
                            .lte(seq),
                        )
                        .and(
                            Expr::col((
                                ResourceVersions::Table,
                                ResourceVersions::LastSeenSeq,
                            ))
                            .gte(seq),
                        ),
                )
                .and_where(
                    Expr::col((
                        WorkloadMembers::Table,
                        WorkloadMembers::ScanId
                    ))
                    .eq(scan_id),
                )
                .and_where(
                    Expr::col((
                        MigrationStatus::Table,
                        MigrationStatus::Status
                    ))
                    .eq("migrated"),
                )
                // Every resource still placed in the pair counts, except
                // an instance that has been terminated and only lingers.
                .and_where(
                    Expr::col((
                        ResourceVersions::Table,
                        ResourceVersions::Kind
                    ))
                    .ne("ec2_instance")
                    .or(state.if_null("").ne("terminated")),
                )
                .group_by_col((
                    WorkloadMembers::Table,
                    WorkloadMembers::Workload
                ))
                .group_by_col((
                    WorkloadMembers::Table,
                    WorkloadMembers::Environment,
                ))
                .order_by(
                    (WorkloadMembers::Table, WorkloadMembers::Workload),
                    Order::Asc,
                )
                .order_by(
                    (WorkloadMembers::Table, WorkloadMembers::Environment),
                    Order::Asc,
                )
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Verification {
                workload: r.get("workload"),
                environment: r.get("environment"),
                remaining: r.get::<i64, _>("remaining").try_into().unwrap_or(0),
            })
            .collect())
    }

    pub async fn metrics(&self, today: Date) -> Result<Metrics, DbError> {
        let pairs = self.schedule_inputs().await?;
        let versions = self.schedule_versions().await?;
        let rules = self.global_rules().await?;
        let mut m = Metrics {
            total: pairs.len(),
            target_end: rules.target_end,
            ..Default::default()
        };

        for p in &pairs {
            match p.status {
                Status::Migrated => m.migrated += 1,
                Status::Confirmed => m.confirmed += 1,
                Status::Proposed => m.proposed += 1,
                Status::Missed => m.missed += 1,
                Status::RolledBack => m.rolled_back += 1,
            }
        }
        let outstanding = m.total - m.migrated;
        m.confirmation_rate = if outstanding == 0 {
            1.0
        } else {
            m.confirmed as f64 / outstanding as f64
        };

        // Everything below comes from the event log: a pair counts as
        // scheduled once a date was confirmed, and as rescheduled once a
        // confirmed date was missed or replaced by another.
        let mut never_missed = 0usize;
        let mut slips: Vec<i64> = Vec::new();
        let mut recent = 0usize;
        let mut ever_migrated = 0usize;
        let mut ever_rolled_back = 0usize;
        let mut reasons: Vec<(String, usize)> = Vec::new();
        let window_start =
            today.checked_sub(jiff::Span::new().days(30)).unwrap_or(today);
        for p in &pairs {
            let events =
                self.migration_events(&p.workload, &p.environment).await?;
            let first_confirmed = events
                .iter()
                .find(|e| e.event == "confirmed")
                .and_then(|e| e.date);
            if first_confirmed.is_some() {
                m.scheduled += 1;
            }
            let moved = events
                .iter()
                .any(|e| e.event == "missed" || e.event == "reconfirmed");
            if moved {
                m.rescheduled += 1;
            }
            for e in events
                .iter()
                .filter(|e| e.event == "missed" || e.event == "reconfirmed")
            {
                let why = e
                    .note
                    .clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "no reason given".into());
                match reasons.iter_mut().find(|(r, _)| *r == why) {
                    Some((_, n)) => *n += 1,
                    None => reasons.push((why, 1)),
                }
            }
            let rolled = events.iter().any(|e| e.event == "rolled_back");
            if events.iter().any(|e| e.event == "migrated") {
                ever_migrated += 1;
            }
            if rolled {
                ever_rolled_back += 1;
            }
            if p.status != Status::Migrated {
                continue;
            }
            if !moved && !rolled {
                never_missed += 1;
            }
            let migrated_on = events
                .iter()
                .filter(|e| e.event == "migrated")
                .filter_map(|e| e.date)
                .next_back();
            if let Some(done) = migrated_on {
                if done >= window_start && done <= today {
                    recent += 1;
                }
                if let Some(first) = first_confirmed {
                    slips.push((done - first).get_days().into());
                }
            }
        }
        reasons.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        m.reschedule_reasons = reasons;
        m.reschedule_rate = if m.scheduled == 0 {
            0.0
        } else {
            m.rescheduled as f64 / m.scheduled as f64
        };
        m.rollback_rate = if ever_migrated == 0 {
            0.0
        } else {
            ever_rolled_back as f64 / ever_migrated as f64
        };
        m.first_time_success_rate = if m.migrated == 0 {
            1.0
        } else {
            never_missed as f64 / m.migrated as f64
        };
        m.average_slip_days = if slips.is_empty() {
            0.0
        } else {
            slips.iter().sum::<i64>() as f64 / slips.len() as f64
        };
        m.migrated_last_30_days = recent;
        m.capacity_last_30_days = Ord::min(
            Ord::min(rules.per_month, rules.per_week * 4),
            rules.per_day * 30,
        );

        for v in &versions {
            if let Some(ends) = v.ends {
                match m.end_history.last_mut() {
                    Some((day, e)) if *day == v.as_of => *e = ends,
                    _ => m.end_history.push((v.as_of, ends)),
                }
            }
        }
        // Forecast accuracy: the first date each migrated pair was ever
        // given against the day it really moved.
        let mut errors: Vec<i64> = Vec::new();
        if let Some(first_version) = versions.first() {
            let rows = bound!(
                Query::select()
                    .columns([
                        ScheduleSlots::Workload,
                        ScheduleSlots::Environment,
                        ScheduleSlots::Date,
                    ])
                    .from(ScheduleSlots::Table)
                    .and_where(
                        Expr::col(ScheduleSlots::VersionId)
                            .eq(first_version.id)
                    )
            )
            .fetch_all(&self.pool)
            .await?;
            for r in rows {
                let (w, e): (String, String) =
                    (r.get("workload"), r.get("environment"));
                let planned = parse_date(&r.get::<String, _>("date"))?;
                let done = self.pair_status(&w, &e).await?;
                if let (Status::Migrated, Some(actual)) =
                    (done.status, done.migrated_date)
                {
                    errors.push((actual - planned).get_days().into());
                }
            }
        }
        if !errors.is_empty() {
            m.forecast_error_days =
                errors.iter().sum::<i64>() as f64 / errors.len() as f64;
            m.forecast_within_week_rate =
                errors.iter().filter(|d| d.abs() <= 7).count() as f64
                    / errors.len() as f64;
        }
        m.projected_end = versions.last().and_then(|v| v.ends);
        m.first_projected_end = versions.first().and_then(|v| v.ends);
        m.end_drift_days = match (m.first_projected_end, m.projected_end) {
            (Some(a), Some(b)) => (b - a).get_days().into(),
            _ => 0,
        };

        // Per wave: counts from the pairs, the projected end from the
        // latest version's slots.
        let mut wave_ends: std::collections::BTreeMap<u32, Date> =
            std::collections::BTreeMap::new();
        if let Some(latest) = versions.last() {
            let rows = bound!(
                Query::select()
                    .column(ScheduleSlots::Wave)
                    .expr_as(
                        Expr::col(ScheduleSlots::EndDate).max(),
                        Alias::new("ends"),
                    )
                    .from(ScheduleSlots::Table)
                    .and_where(
                        Expr::col(ScheduleSlots::VersionId).eq(latest.id)
                    )
                    .group_by_col(ScheduleSlots::Wave)
            )
            .fetch_all(&self.pool)
            .await?;
            for r in rows {
                let ends: Option<String> = r.get("ends");
                if let Some(e) = ends {
                    wave_ends.insert(
                        r.get::<i64, _>("wave").try_into().unwrap_or(1),
                        parse_date(&e)?,
                    );
                }
            }
        }
        for w in self.waves().await? {
            let in_wave = pairs.iter().filter(|p| p.wave == w.number);
            let total = in_wave.clone().count();
            let migrated =
                in_wave.filter(|p| p.status == Status::Migrated).count();
            if total == 0 && w.name.is_empty() {
                continue;
            }
            m.waves.push(WaveMetrics {
                number: w.number,
                name: w.name,
                target_end: w.target_end,
                total,
                migrated,
                projected_end: wave_ends.get(&w.number).copied(),
            });
        }
        Ok(m)
    }
}

const WEEKDAYS: [Weekday; 5] = [
    Weekday::Monday,
    Weekday::Tuesday,
    Weekday::Wednesday,
    Weekday::Thursday,
    Weekday::Friday,
];
const WEEKENDS: [Weekday; 2] = [Weekday::Saturday, Weekday::Sunday];

/// The preset name for a weekday rule, if it matches one.
pub fn weekday_preset(days: &[Weekday]) -> Option<&'static str> {
    if days.is_empty() {
        Some("any day")
    } else if days == WEEKDAYS {
        Some("weekdays")
    } else if days == WEEKENDS {
        Some("weekends")
    } else {
        None
    }
}

/// Parse a weekday list with presets: `any`, `weekdays`, `weekends`, or a
/// comma list such as `sat,sun`.
pub fn weekdays_from_spec(spec: &str) -> Vec<Weekday> {
    match spec.trim().to_lowercase().as_str() {
        "any" | "all" | "" => Vec::new(),
        "weekdays" => vec![
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
        ],
        "weekends" | "weekend" => vec![Weekday::Saturday, Weekday::Sunday],
        other => parse_weekdays(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekday_specs_and_presets() {
        assert!(weekdays_from_spec("any").is_empty());
        assert_eq!(
            weekdays_from_spec("weekends"),
            vec![Weekday::Saturday, Weekday::Sunday]
        );
        assert_eq!(weekdays_from_spec("weekdays").len(), 5);
        assert_eq!(
            weekdays_from_spec("Tue, thu"),
            vec![Weekday::Tuesday, Weekday::Thursday]
        );
    }
}
