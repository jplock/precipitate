//! Schedule persistence: rules, blackouts, status, events and versions.

use crate::kinds::{
    DispositionSource, EventKind, MigrationEventKind, Setting, Trigger,
};
use crate::query::{Bound, RowExt};
use crate::schema::{
    DispositionHistory, MigrationEvents, MigrationStatus, PlanEvents,
    ResourceVersions, ScheduleBlackouts, ScheduleDependencies, ScheduleRules,
    ScheduleSlots, ScheduleVersions, ScheduleWaves, Settings, WorkloadAliases,
    WorkloadEnvironments, WorkloadMembers,
};
use crate::{DbError, Store};
use jiff::civil::Date;
use precip_engine::schedule::{
    Blackout, GlobalRules, PairInput, PairState, Plan, Schedule, Status, Wave,
    Weekdays,
};
use precip_types::PairKey;
use sea_query::{
    Alias, Expr, ExprTrait, Func, OnConflict, Order, Query, UnionType,
};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;

/// A pair's stored weekday rule: empty follows the global rule.
fn pair_weekdays(spec: &str) -> Result<Option<Weekdays>, DbError> {
    if spec.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(spec.parse()?))
}

/// A pair from two of a row's columns.
fn pair_of(
    r: &SqliteRow,
    workload: &str,
    environment: &str,
) -> Result<PairKey, DbError> {
    Ok(PairKey::new(
        r.try_get::<String, _>(workload)?,
        r.try_get::<String, _>(environment)?,
    ))
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
        source: DispositionSource,
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
        PlanEvent::RulesSet {
            per_day: rules.per_day,
            per_week: rules.per_week,
            per_month: rules.per_month,
            weekdays: rules.weekdays.to_spec(),
            target_end: rules.target_end,
            nonprod_before_prod: rules.nonprod_before_prod,
        }
    }

    fn blackout(b: &Blackout, added: bool) -> Self {
        let (workload, environment) = match &b.pair {
            Some(p) => {
                (Some(p.workload.to_string()), Some(p.environment.to_string()))
            }
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

    pub fn kind(&self) -> EventKind {
        match self {
            PlanEvent::RulesSet { .. } => EventKind::RulesSet,
            PlanEvent::BlackoutAdded { .. } => EventKind::BlackoutAdded,
            PlanEvent::BlackoutRemoved { .. } => EventKind::BlackoutRemoved,
            PlanEvent::PairRuleSet { .. } => EventKind::PairRuleSet,
            PlanEvent::PairPlanSet { .. } => EventKind::PairPlanSet,
            PlanEvent::WaveSet { .. } => EventKind::WaveSet,
            PlanEvent::WaveRemoved { .. } => EventKind::WaveRemoved,
            PlanEvent::DependencyAdded { .. } => EventKind::DependencyAdded,
            PlanEvent::DependencyRemoved { .. } => EventKind::DependencyRemoved,
            PlanEvent::StatusChanged { .. } => EventKind::StatusChanged,
            PlanEvent::DispositionSet { .. } => EventKind::DispositionSet,
            PlanEvent::DefaultDispositionSet { .. } => {
                EventKind::DefaultDispositionSet
            }
            PlanEvent::WorkloadMerged { .. } => EventKind::WorkloadMerged,
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
                    disposition.as_str(),
                    if *source == DispositionSource::Bulk {
                        " (default applied)"
                    } else {
                        ""
                    },
                    note.as_ref().map(|n| format!(", {n}")).unwrap_or_default()
                )
            }
            PlanEvent::DefaultDispositionSet {
                disposition,
                apply_to_existing,
            } => format!(
                "new pairs start as {}{}",
                disposition.as_str(),
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

impl PairStatus {
    /// A pair with no stored status.
    const PROPOSED: Self = Self {
        status: Status::Proposed,
        confirmed_date: None,
        migrated_date: None,
    };

    fn from_row(r: &SqliteRow) -> Result<Self, DbError> {
        Ok(Self {
            status: r.parsed("status")?,
            confirmed_date: r.parsed_opt("confirmed_date")?,
            migrated_date: r.parsed_opt("migrated_date")?,
        })
    }

    /// The scheduler's view. An agreed status is always written with its
    /// date; one read without a date has nothing to hold and is proposed.
    pub fn state(&self) -> PairState {
        match (self.status, self.confirmed_date) {
            (Status::Confirmed, Some(d)) => PairState::Confirmed(d),
            (Status::Confirmed | Status::Proposed, _) => PairState::Proposed,
            (Status::Missed, _) => PairState::Missed,
            (Status::Migrated, _) => PairState::Migrated,
            (Status::RolledBack, _) => PairState::RolledBack,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationEvent {
    pub event: MigrationEventKind,
    pub date: Option<Date>,
    pub at: jiff::Timestamp,
    pub note: Option<String>,
}

impl MigrationEvent {
    fn from_row(r: &sqlx::sqlite::SqliteRow) -> Result<Self, DbError> {
        Ok(Self {
            event: r.parsed("event")?,
            date: r.parsed_opt("date")?,
            at: r.parsed("at")?,
            note: r.try_get("note")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleVersion {
    pub id: i64,
    pub computed_at: jiff::Timestamp,
    pub as_of: Date,
    pub target_end: Option<Date>,
    pub ends: Option<Date>,
    pub late: i64,
    pub trigger: Trigger,
}

impl Store {
    async fn setting(&self, key: Setting) -> Result<Option<String>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::setting_in(&mut conn, key).await
    }

    async fn setting_in(
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

    async fn set_setting(
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

    async fn global_rules_in(
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

    async fn project_rules(
        conn: &mut SqliteConnection,
        rules: &GlobalRules,
    ) -> Result<(), DbError> {
        Self::set_setting(conn, Setting::PerDay, &rules.per_day.to_string())
            .await?;
        Self::set_setting(conn, Setting::PerWeek, &rules.per_week.to_string())
            .await?;
        Self::set_setting(
            conn,
            Setting::PerMonth,
            &rules.per_month.to_string(),
        )
        .await?;
        Self::set_setting(conn, Setting::Weekdays, &rules.weekdays.to_spec())
            .await?;
        Self::set_setting(
            conn,
            Setting::TargetEnd,
            &rules.target_end.map(|d| d.to_string()).unwrap_or_default(),
        )
        .await?;
        Self::set_setting(
            conn,
            Setting::NonprodBeforeProd,
            &rules.nonprod_before_prod.to_string(),
        )
        .await?;
        Ok(())
    }

    async fn project_blackout_added(
        conn: &mut SqliteConnection,
        b: &Blackout,
    ) -> Result<(), DbError> {
        (Query::insert()
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
                b.pair.as_ref().map(|p| p.workload.to_string()).into(),
                b.pair.as_ref().map(|p| p.environment.to_string()).into(),
            ]))
        .bound()
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
            Some(PairKey { workload: w, environment: e }) => {
                q.and_where(
                    Expr::col(ScheduleBlackouts::Workload).eq(w.as_str()),
                )
                .and_where(
                    Expr::col(ScheduleBlackouts::Environment).eq(e.as_str()),
                );
            }
            None => {
                q.and_where(Expr::col(ScheduleBlackouts::Workload).is_null())
                    .and_where(
                        Expr::col(ScheduleBlackouts::Environment).is_null(),
                    );
            }
        }
        (q).bound().execute(&mut *conn).await?;
        Ok(())
    }

    pub async fn blackouts(&self) -> Result<Vec<Blackout>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::blackouts_in(&mut conn).await
    }

    async fn blackouts_in(
        conn: &mut SqliteConnection,
    ) -> Result<Vec<Blackout>, DbError> {
        let rows = (Query::select()
            .columns([
                ScheduleBlackouts::Name,
                ScheduleBlackouts::StartDate,
                ScheduleBlackouts::EndDate,
                ScheduleBlackouts::Workload,
                ScheduleBlackouts::Environment,
            ])
            .from(ScheduleBlackouts::Table)
            .order_by(ScheduleBlackouts::StartDate, Order::Asc)
            .order_by(ScheduleBlackouts::Name, Order::Asc))
        .bound()
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .map(|r| {
                let w: Option<String> = r.get("workload");
                let e: Option<String> = r.get("environment");
                Ok(Blackout {
                    name: r.get("name"),
                    start: r.parsed::<Date>("start_date")?,
                    end: r.parsed::<Date>("end_date")?,
                    pair: w.zip(e).map(|(w, e)| PairKey::new(w, e)),
                })
            })
            .collect()
    }

    async fn project_pair_rule(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
        weekdays: Option<&Weekdays>,
        earliest: Option<Date>,
        priority: i32,
    ) -> Result<(), DbError> {
        // An empty list stored means the pair follows the global rule.
        let days = weekdays.map(Weekdays::to_spec).unwrap_or_default();
        (Query::insert()
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
                days.into(),
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
            ))
        .bound()
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
        (Query::insert()
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
            ))
        .bound()
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
        (Query::insert()
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
            ))
        .bound()
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn project_wave_removed(
        conn: &mut SqliteConnection,
        number: u32,
    ) -> Result<(), DbError> {
        (Query::update()
            .table(ScheduleRules::Table)
            .value(ScheduleRules::Wave, 1)
            .and_where(Expr::col(ScheduleRules::Wave).eq(number)))
        .bound()
        .execute(&mut *conn)
        .await?;
        (Query::delete()
            .from_table(ScheduleWaves::Table)
            .and_where(Expr::col(ScheduleWaves::Wave).eq(number)))
        .bound()
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
        let rows = (Query::select()
            .column((in_use.clone(), ScheduleWaves::Wave))
            .column((ScheduleWaves::Table, ScheduleWaves::Name))
            .column((ScheduleWaves::Table, ScheduleWaves::TargetEnd))
            .from_subquery(numbers, in_use.clone())
            .left_join(
                ScheduleWaves::Table,
                Expr::col((ScheduleWaves::Table, ScheduleWaves::Wave))
                    .equals((in_use.clone(), ScheduleWaves::Wave)),
            )
            .order_by((in_use.clone(), ScheduleWaves::Wave), Order::Asc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let name: Option<String> = r.get("name");
                Ok(Wave {
                    number: r.get::<i64, _>("wave").try_into().unwrap_or(1),
                    name: name.unwrap_or_default(),
                    target_end: r.parsed_opt("target_end")?,
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
        (Query::insert()
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
            ))
        .bound()
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
        (Query::delete()
            .from_table(ScheduleDependencies::Table)
            .and_where(Expr::col(ScheduleDependencies::Workload).eq(workload))
            .and_where(
                Expr::col(ScheduleDependencies::Environment).eq(environment),
            )
            .and_where(
                Expr::col(ScheduleDependencies::OnWorkload).eq(on_workload),
            )
            .and_where(
                Expr::col(ScheduleDependencies::OnEnvironment)
                    .eq(on_environment),
            ))
        .bound()
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
        let row = (Query::select()
            .columns([
                MigrationStatus::Status,
                MigrationStatus::ConfirmedDate,
                MigrationStatus::MigratedDate,
            ])
            .from(MigrationStatus::Table)
            .and_where(Expr::col(MigrationStatus::Workload).eq(workload))
            .and_where(
                Expr::col(MigrationStatus::Environment).eq(environment),
            ))
        .bound()
        .fetch_optional(&mut *conn)
        .await?;
        row.as_ref().map_or(Ok(PairStatus::PROPOSED), PairStatus::from_row)
    }

    /// Every pair with a stored status, in one query.
    async fn pair_statuses_in(
        conn: &mut SqliteConnection,
    ) -> Result<HashMap<PairKey, PairStatus>, DbError> {
        (Query::select()
            .columns([
                MigrationStatus::Workload,
                MigrationStatus::Environment,
                MigrationStatus::Status,
                MigrationStatus::ConfirmedDate,
                MigrationStatus::MigratedDate,
            ])
            .from(MigrationStatus::Table))
        .bound()
        .fetch_all(&mut *conn)
        .await?
        .iter()
        .map(|r| {
            Ok((
                pair_of(r, "workload", "environment")?,
                PairStatus::from_row(r)?,
            ))
        })
        .collect()
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
            if let Some(dep) =
                Self::unfinished_dependencies_in(conn, workload, environment)
                    .await?
                    .first()
            {
                return refuse(&format!(
                    "it waits for {dep}, which has not migrated; agree that date first"
                ));
            }
            let pairs = Self::schedule_inputs_in(conn).await?;
            let rules = Self::global_rules_in(conn).await?;
            let blackouts = Self::blackouts_in(conn).await?;
            let plan =
                Plan { pairs: &pairs, rules: &rules, blackouts: &blackouts };
            if let (Some(me), Some(start)) =
                (pairs.iter().find(|p| p.key.is(workload, environment)), date)
            {
                if let Err(why) = plan.check_agreed_date(me, start) {
                    return refuse(&why.to_string());
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
        let name = status.as_str();
        let (confirmed, migrated) = match status {
            Status::Confirmed => (date, None),
            Status::Migrated => (None, date.or(Some(today))),
            Status::Proposed | Status::Missed | Status::RolledBack => {
                (None, None)
            }
        };
        (Query::insert()
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
                name.into(),
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
            ))
        .bound()
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
        (Query::insert()
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
                confirmed.or(migrated).or(date).map(|d| d.to_string()).into(),
                at.to_string().into(),
                note.into(),
            ]))
        .bound()
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn migration_events(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<MigrationEvent>, DbError> {
        let rows = (Query::select()
            .columns([
                MigrationEvents::Event,
                MigrationEvents::Date,
                MigrationEvents::At,
                MigrationEvents::Note,
            ])
            .from(MigrationEvents::Table)
            .and_where(Expr::col(MigrationEvents::Workload).eq(workload))
            .and_where(Expr::col(MigrationEvents::Environment).eq(environment))
            .order_by(MigrationEvents::Id, Order::Asc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(MigrationEvent::from_row).collect()
    }

    /// Every pair's migration history in one query, oldest first.
    async fn migration_events_by_pair(
        &self,
    ) -> Result<BTreeMap<PairKey, Vec<MigrationEvent>>, DbError> {
        let rows = (Query::select()
            .columns([
                MigrationEvents::Workload,
                MigrationEvents::Environment,
                MigrationEvents::Event,
                MigrationEvents::Date,
                MigrationEvents::At,
                MigrationEvents::Note,
            ])
            .from(MigrationEvents::Table)
            .order_by(MigrationEvents::Id, Order::Asc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        let mut out: BTreeMap<PairKey, Vec<MigrationEvent>> = BTreeMap::new();
        for r in rows {
            out.entry(PairKey::new(
                r.try_get::<String, _>("workload")?,
                r.try_get::<String, _>("environment")?,
            ))
            .or_default()
            .push(MigrationEvent::from_row(&r)?);
        }
        Ok(out)
    }

    /// The pairs this one waits for that have not migrated yet.
    pub async fn unfinished_dependencies(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<PairKey>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::unfinished_dependencies_in(&mut conn, workload, environment).await
    }

    async fn unfinished_dependencies_in(
        conn: &mut SqliteConnection,
        workload: &str,
        environment: &str,
    ) -> Result<Vec<PairKey>, DbError> {
        let pairs = Self::schedule_inputs_in(conn).await?;
        let rules = Self::global_rules_in(conn).await?;
        let Some(me) = pairs.iter().find(|p| p.key.is(workload, environment))
        else {
            return Ok(Vec::new());
        };
        let plan = Plan { pairs: &pairs, rules: &rules, blackouts: &[] };
        Ok(plan.unfinished_dependencies(me))
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
        let rules: HashMap<PairKey, SqliteRow> = (Query::select()
            .columns([
                ScheduleRules::Workload,
                ScheduleRules::Environment,
                ScheduleRules::Weekdays,
                ScheduleRules::Earliest,
                ScheduleRules::Priority,
                ScheduleRules::Wave,
                ScheduleRules::DurationDays,
                ScheduleRules::Weight,
            ])
            .from(ScheduleRules::Table))
        .bound()
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|r| Ok((pair_of(&r, "workload", "environment")?, r)))
        .collect::<Result<_, DbError>>()?;
        let mut deps: HashMap<PairKey, Vec<PairKey>> = HashMap::new();
        for r in (Query::select()
            .columns([
                ScheduleDependencies::Workload,
                ScheduleDependencies::Environment,
                ScheduleDependencies::OnWorkload,
                ScheduleDependencies::OnEnvironment,
            ])
            .from(ScheduleDependencies::Table))
        .bound()
        .fetch_all(&mut *conn)
        .await?
        {
            deps.entry(pair_of(&r, "workload", "environment")?)
                .or_default()
                .push(pair_of(&r, "on_workload", "on_environment")?);
        }
        let mut statuses = Self::pair_statuses_in(conn).await?;

        let mut out = Vec::new();
        for (pair, d) in dispositions {
            if !d.is_migrating() {
                continue;
            }
            let mut input = PairInput::new(pair);
            if let Some(r) = rules.get(&input.key) {
                // Stored as at least 1 by set_pair_plan; anything else
                // reads as 1.
                let count = |col: &str| -> Result<NonZeroU32, DbError> {
                    Ok(u32::try_from(r.try_get::<i64, _>(col)?)
                        .ok()
                        .and_then(NonZeroU32::new)
                        .unwrap_or(NonZeroU32::MIN))
                };
                input.weekdays =
                    pair_weekdays(&r.try_get::<String, _>("weekdays")?)?;
                input.earliest = r.parsed_opt("earliest")?;
                input.priority =
                    i32::try_from(r.try_get::<i64, _>("priority")?)
                        .unwrap_or(0);
                input.wave = count("wave")?.get();
                input.duration = count("duration_days")?;
                input.weight = count("weight")?;
            }
            input.depends_on = deps.remove(&input.key).unwrap_or_default();
            input.state = statuses
                .remove(&input.key)
                .map_or(PairState::Proposed, |s| s.state());
            out.push(input);
        }
        Ok(out)
    }

    /// Compute the schedule from the stored rules and status, record it as
    /// a version, and write back any expired confirmations as missed.
    pub async fn compute_schedule(
        &self,
        today: Date,
        trigger: Trigger,
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
        trigger: Trigger,
        event_id: Option<i64>,
    ) -> Result<(i64, Schedule), DbError> {
        let pairs = Self::schedule_inputs_in(conn).await?;
        let rules = Self::global_rules_in(conn).await?;
        let blackouts = Self::blackouts_in(conn).await?;
        let schedule =
            Plan { pairs: &pairs, rules: &rules, blackouts: &blackouts }
                .compute(today);

        // A confirmed date that passed is itself a change to the plan,
        // recorded like any other, without computing again.
        for pair in &schedule.expired {
            let prior = Self::pair_status_in(
                conn,
                pair.workload.as_str(),
                pair.environment.as_str(),
            )
            .await?;
            Self::record(
                conn,
                &PlanEvent::StatusChanged {
                    workload: pair.workload.to_string(),
                    environment: pair.environment.to_string(),
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

        let id: i64 = (Query::insert()
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
                i64::try_from(schedule.late.len()).unwrap_or(i64::MAX).into(),
                trigger.as_str().into(),
                serde_json::to_string(&schedule)?.into(),
                event_id.into(),
            ])
            .returning_col(ScheduleVersions::Id))
        .bound()
        .fetch_one(&mut *conn)
        .await?
        .get("id");
        for s in &schedule.slots {
            let alts: Vec<String> =
                s.alternatives.iter().map(|d| d.to_string()).collect();
            (Query::insert()
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
                    s.pair.workload.as_str().into(),
                    s.pair.environment.as_str().into(),
                    s.date.to_string().into(),
                    s.end.to_string().into(),
                    s.wave.into(),
                    i32::from(s.agreed).into(),
                    s.reason().into(),
                    alts.join(",").into(),
                ]))
            .bound()
            .execute(&mut *conn)
            .await?;
        }
        if let Some(event_id) = event_id {
            (Query::update()
                .table(PlanEvents::Table)
                .value(PlanEvents::VersionId, id)
                .and_where(Expr::col(PlanEvents::Id).eq(event_id)))
            .bound()
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
        let row = (Query::select()
            .columns([
                ScheduleVersions::Id,
                ScheduleVersions::AsOf,
                ScheduleVersions::Schedule,
            ])
            .from(ScheduleVersions::Table)
            .order_by(ScheduleVersions::Id, Order::Desc)
            .limit(1))
        .bound()
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let json: Option<String> = r.get("schedule");
        let Some(json) = json else { return Ok(None) };
        // A version stored before the schedule's shape last changed does
        // not read back; the caller computes a fresh one instead.
        let Ok(schedule) = serde_json::from_str(&json) else { return Ok(None) };
        Ok(Some((r.get("id"), r.parsed::<Date>("as_of")?, schedule)))
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
        let id: i64 = (Query::insert()
            .into_table(PlanEvents::Table)
            .columns([PlanEvents::At, PlanEvents::Kind, PlanEvents::Body])
            .values_panic([
                at.to_string().into(),
                event.kind().as_str().into(),
                serde_json::to_string(event)?.into(),
            ])
            .returning_col(PlanEvents::Id))
        .bound()
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
            .compute_version(
                &mut tx,
                today,
                Trigger::Event(event.kind()),
                Some(id),
            )
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
                        weekdays: weekdays.parse()?,
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
                        pair: workload
                            .clone()
                            .zip(environment.clone())
                            .map(|(w, e)| PairKey::new(w, e)),
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
                        pair: workload
                            .clone()
                            .zip(environment.clone())
                            .map(|(w, e)| PairKey::new(w, e)),
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
                    pair_weekdays(weekdays)?.as_ref(),
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
                    *source,
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
        let rows = (Query::select()
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
            .order_by((PlanEvents::Table, PlanEvents::Id), Order::Asc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(PlanEventRecord {
                    id: r.try_get("id")?,
                    at: r.parsed("at")?,
                    event: serde_json::from_str(
                        &r.try_get::<String, _>("body")?,
                    )?,
                    version_id: r.try_get("version_id")?,
                    ends: r.parsed_opt("ends")?,
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
            (stmt).bound().execute(&mut *tx).await?;
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
        self.compute_schedule(self.today(), Trigger::Replay).await
    }

    /// Every pair back to the built-in default, user and bulk history
    /// cleared, aliases cleared; only the log can set them again.
    async fn reset_dispositions(
        conn: &mut SqliteConnection,
    ) -> Result<(), DbError> {
        let builtin = precip_types::Disposition::Rehost.as_str();
        (Query::update()
            .table(Settings::Table)
            .value(Settings::Value, builtin)
            .and_where(
                Expr::col(Settings::Key)
                    .eq(Setting::DefaultDisposition.as_str()),
            ))
        .bound()
        .execute(&mut *conn)
        .await?;
        (Query::update()
            .table(WorkloadEnvironments::Table)
            .value(WorkloadEnvironments::Disposition, builtin))
        .bound()
        .execute(&mut *conn)
        .await?;
        (Query::delete().from_table(DispositionHistory::Table).and_where(
            Expr::col(DispositionHistory::Source)
                .ne(DispositionSource::Default.as_str()),
        ))
        .bound()
        .execute(&mut *conn)
        .await?;
        (Query::delete().from_table(WorkloadAliases::Table))
            .bound()
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
            DispositionSource::User,
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
        source: DispositionSource,
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
            source,
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
        weekdays: Option<&Weekdays>,
        earliest: Option<Date>,
        priority: i32,
    ) -> Result<(), DbError> {
        self.apply(PlanEvent::PairRuleSet {
            workload: workload.into(),
            environment: environment.into(),
            weekdays: weekdays.map(Weekdays::to_spec).unwrap_or_default(),
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
        let rows = (Query::select()
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
            .order_by(ScheduleVersions::Id, Order::Asc))
        .bound()
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ScheduleVersion {
                    id: r.try_get("id")?,
                    computed_at: r.parsed("computed_at")?,
                    as_of: r.parsed("as_of")?,
                    target_end: r.parsed_opt("target_end")?,
                    ends: r.parsed_opt("ends")?,
                    late: r.try_get("late")?,
                    trigger: r.parsed("trigger")?,
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
        let rows = (Query::select()
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
                Expr::col((MigrationStatus::Table, MigrationStatus::Workload))
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
                Expr::col((WorkloadMembers::Table, WorkloadMembers::ScanId))
                    .eq(scan_id),
            )
            .and_where(
                Expr::col((MigrationStatus::Table, MigrationStatus::Status))
                    .eq(Status::Migrated.as_str()),
            )
            // Every resource still placed in the pair counts, except
            // an instance that has been terminated and only lingers.
            .and_where(
                Expr::col((ResourceVersions::Table, ResourceVersions::Kind))
                    .ne("ec2_instance")
                    .or(state.if_null("").ne("terminated")),
            )
            .group_by_col((WorkloadMembers::Table, WorkloadMembers::Workload))
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
            ))
        .bound()
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
            match p.state.status() {
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
        let all_events = self.migration_events_by_pair().await?;
        for p in &pairs {
            let events =
                all_events.get(&p.key).map(Vec::as_slice).unwrap_or_default();
            let first_confirmed = events
                .iter()
                .find(|e| e.event == MigrationEventKind::Confirmed)
                .and_then(|e| e.date);
            if first_confirmed.is_some() {
                m.scheduled += 1;
            }
            let moved = events.iter().any(|e| e.event.moved_a_date());
            if moved {
                m.rescheduled += 1;
            }
            for e in events.iter().filter(|e| e.event.moved_a_date()) {
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
            let rolled = events
                .iter()
                .any(|e| e.event == MigrationEventKind::RolledBack);
            if events.iter().any(|e| e.event == MigrationEventKind::Migrated) {
                ever_migrated += 1;
            }
            if rolled {
                ever_rolled_back += 1;
            }
            if p.state != PairState::Migrated {
                continue;
            }
            if !moved && !rolled {
                never_missed += 1;
            }
            let migrated_on = events
                .iter()
                .filter(|e| e.event == MigrationEventKind::Migrated)
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
            let rows = (Query::select()
                .columns([
                    ScheduleSlots::Workload,
                    ScheduleSlots::Environment,
                    ScheduleSlots::Date,
                ])
                .from(ScheduleSlots::Table)
                .and_where(
                    Expr::col(ScheduleSlots::VersionId).eq(first_version.id),
                ))
            .bound()
            .fetch_all(&self.pool)
            .await?;
            for r in rows {
                let (w, e): (String, String) =
                    (r.get("workload"), r.get("environment"));
                let planned = r.parsed::<Date>("date")?;
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
            let rows = (Query::select()
                .column(ScheduleSlots::Wave)
                .expr_as(
                    Expr::col(ScheduleSlots::EndDate).max(),
                    Alias::new("ends"),
                )
                .from(ScheduleSlots::Table)
                .and_where(Expr::col(ScheduleSlots::VersionId).eq(latest.id))
                .group_by_col(ScheduleSlots::Wave))
            .bound()
            .fetch_all(&self.pool)
            .await?;
            for r in rows {
                if let Some(e) = r.parsed_opt::<Date>("ends")? {
                    wave_ends.insert(
                        r.try_get::<i64, _>("wave")?.try_into().unwrap_or(1),
                        e,
                    );
                }
            }
        }
        for w in self.waves().await? {
            let in_wave = pairs.iter().filter(|p| p.wave == w.number);
            let total = in_wave.clone().count();
            let migrated =
                in_wave.filter(|p| p.state == PairState::Migrated).count();
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
