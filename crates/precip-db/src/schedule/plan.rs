//! Reading the plan and computing schedule versions from it.

use super::{
    MigrationEvent, PairStatus, PlanEvent, ScheduleVersion, pair_of,
    pair_weekdays,
};
use crate::kinds::Trigger;
use crate::query::{Bound, RowExt};
use crate::schema::{
    MigrationEvents, MigrationStatus, PlanEvents, ScheduleBlackouts,
    ScheduleDependencies, ScheduleRules, ScheduleSlots, ScheduleVersions,
    ScheduleWaves,
};
use crate::{DbError, Store};
use jiff::civil::Date;
use precip_engine::schedule::{
    Blackout, PairInput, PairState, Plan, Schedule, Status, Wave,
};
use precip_types::PairKey;
use sea_query::{Alias, Expr, ExprTrait, Order, Query, UnionType};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;

impl Store {
    pub async fn blackouts(&self) -> Result<Vec<Blackout>, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::blackouts_in(&mut conn).await
    }

    pub(crate) async fn blackouts_in(
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

    pub async fn pair_status(
        &self,
        workload: &str,
        environment: &str,
    ) -> Result<PairStatus, DbError> {
        let mut conn = self.pool.acquire().await?;
        Self::pair_status_in(&mut conn, workload, environment).await
    }

    pub(crate) async fn pair_status_in(
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
    pub(crate) async fn pair_statuses_in(
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
    pub(crate) async fn migration_events_by_pair(
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

    pub(crate) async fn unfinished_dependencies_in(
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

    pub(crate) async fn schedule_inputs_in(
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

    pub(crate) async fn compute_version(
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
