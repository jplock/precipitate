//! Schedule persistence: rules, blackouts, status, events and versions.

use crate::kinds::{DispositionSource, EventKind, MigrationEventKind, Trigger};
use crate::query::RowExt;
use crate::{DbError, Store};
use jiff::civil::Date;
use precip_engine::schedule::{
    Blackout, GlobalRules, PairState, Status, Weekdays,
};
use precip_types::PairKey;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

mod events;
mod metrics;
mod plan;
mod projections;
mod settings;

pub use metrics::{Metrics, RESCHEDULE_REASONS, Verification, WaveMetrics};

/// A pair's stored weekday rule: empty follows the global rule.
pub(crate) fn pair_weekdays(spec: &str) -> Result<Option<Weekdays>, DbError> {
    if spec.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(spec.parse()?))
}

/// A pair from two of a row's columns.
pub(crate) fn pair_of(
    r: &SqliteRow,
    workload: &str,
    environment: &str,
) -> Result<PairKey, DbError> {
    Ok(PairKey::new(
        r.try_get::<String, _>(workload)?,
        r.try_get::<String, _>(environment)?,
    ))
}

pub(crate) fn parse_date(s: &str) -> Result<Date, DbError> {
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

    pub(crate) fn kind(&self) -> EventKind {
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
pub(crate) struct PairStatus {
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
    pub(crate) fn state(&self) -> PairState {
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
pub(crate) struct MigrationEvent {
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
pub(crate) struct ScheduleVersion {
    pub id: i64,
    pub computed_at: jiff::Timestamp,
    pub as_of: Date,
    pub target_end: Option<Date>,
    pub ends: Option<Date>,
    pub late: i64,
    pub trigger: Trigger,
}

impl Store {
    pub(crate) fn today(&self) -> Date {
        self.today_override
            .lock()
            .ok()
            .and_then(|t| *t)
            .unwrap_or_else(|| jiff::Zoned::now().date())
    }

    #[cfg(test)]
    /// Pin "today" so schedules computed by `apply` are reproducible.
    pub(crate) fn freeze_today(&self, day: Date) {
        if let Ok(mut t) = self.today_override.lock() {
            *t = Some(day);
        }
    }
}
