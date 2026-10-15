//! Applying plan events to the tables built from them.

use super::{PlanEvent, pair_weekdays};
use crate::kinds::{DispositionSource, Setting};
use crate::query::Bound;
use crate::schema::{
    DispositionHistory, MigrationEvents, MigrationStatus, ScheduleBlackouts,
    ScheduleDependencies, ScheduleRules, ScheduleWaves, Settings,
    WorkloadAliases, WorkloadEnvironments,
};
use crate::{DbError, Store};
use jiff::civil::Date;
use precip_engine::schedule::{Blackout, GlobalRules, Plan, Status, Weekdays};
use precip_types::PairKey;
use sea_query::{Expr, ExprTrait, OnConflict, Query};
use sqlx::SqliteConnection;

impl Store {
    pub(crate) async fn project_rules(
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

    pub(crate) async fn project_blackout_added(
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

    pub(crate) async fn project_blackout_removed(
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

    pub(crate) async fn project_pair_rule(
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
    pub(crate) async fn project_pair_plan(
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

    pub(crate) async fn project_wave(
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

    pub(crate) async fn project_wave_removed(
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

    pub(crate) async fn project_dependency_added(
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

    pub(crate) async fn project_dependency_removed(
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

    /// Record a status change and its event. Confirming requires a date
    /// that is today or later.
    #[expect(clippy::too_many_arguments, reason = "projection of one event")]
    pub(crate) async fn project_status(
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

    pub(crate) async fn project(
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

    /// Every pair back to the built-in default, user and bulk history
    /// cleared, aliases cleared; only the log can set them again.
    pub(crate) async fn reset_dispositions(
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
}
