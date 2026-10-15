//! Headline numbers for the migration, and pairs that migrated but are
//! still in AWS.

use crate::kinds::MigrationEventKind;
use crate::query::{Bound, RowExt};
use crate::schema::{
    MigrationStatus, ResourceVersions, ScheduleSlots, WorkloadMembers,
};
use crate::{DbError, Store};
use jiff::civil::Date;
use precip_engine::schedule::{PairState, Status};
use precip_types::ResourceKind;
use sea_query::{Alias, Expr, ExprTrait, Func, Order, Query};
use sqlx::Row;

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
                    .ne(ResourceKind::Instance.as_str())
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
