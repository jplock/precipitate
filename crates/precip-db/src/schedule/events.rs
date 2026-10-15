//! The event log, and one public method per kind of change.

use super::{PlanEvent, PlanEventRecord};
use crate::kinds::{DispositionSource, Trigger};
use crate::query::{Bound, RowExt};
use crate::schema::{
    MigrationEvents, MigrationStatus, PlanEvents, ScheduleBlackouts,
    ScheduleDependencies, ScheduleRules, ScheduleVersions, ScheduleWaves,
};
use crate::{DbError, Store};
use jiff::civil::Date;
use precip_engine::schedule::holidays::Holiday;
use precip_engine::schedule::{
    Blackout, GlobalRules, Schedule, Status, Weekdays,
};
use sea_query::{Expr, ExprTrait, Order, Query};
use sqlx::{Row, SqliteConnection};

impl Store {
    /// Append an event and project it, without computing a schedule.
    pub(crate) async fn record(
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
    pub(crate) async fn apply(
        &self,
        event: PlanEvent,
    ) -> Result<(i64, Schedule), DbError> {
        let today = self.today();
        self.apply_as_of(event, today).await
    }

    /// `apply` from a chosen day, for status changes that carry one.
    pub(crate) async fn apply_as_of(
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
    /// Record that `alias` is really `workload`. Folding a name into
    /// itself changes nothing and records nothing.
    pub async fn merge_workload(
        &self,
        alias: &str,
        workload: &str,
    ) -> Result<(), DbError> {
        if alias.trim() == workload.trim() {
            return Ok(());
        }
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
        holidays: &[Holiday],
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
}
