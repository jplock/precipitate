//! The `schedule` subcommands, and how the schedule and its metrics print.

use jiff::civil::Date;
use precip_db::{Store, Trigger};
use precip_engine::schedule::gantt::Gantt;
use precip_engine::schedule::{Status, day_name};
use precip_types::PairKey;

use crate::cli::{PairArgs, ScheduleCommand, TargetEnd};
use crate::print_line;

/// Print the schedule: the version the last change produced, or a fresh
/// computation when `trigger` is given (the `show` command, a new day).
pub(crate) async fn print_schedule(
    store: &Store,
    trigger: Option<Trigger>,
) -> anyhow::Result<()> {
    let rules = store.global_rules().await?;
    let latest = match trigger {
        Some(_) => None,
        None => store
            .latest_schedule()
            .await?
            .filter(|(_, as_of, _)| *as_of == today()),
    };
    let (version, schedule) = match latest {
        Some((id, _, schedule)) => (id, schedule),
        None => {
            store
                .compute_schedule(today(), trigger.unwrap_or(Trigger::Command))
                .await?
        }
    };
    out!(
        "schedule #{version}  {} per day, {} per week, {} per month, {}{}{}",
        rules.per_day,
        rules.per_week,
        rules.per_month,
        rules.weekdays,
        rules
            .target_end
            .map(|d| format!("  target end {d}"))
            .unwrap_or_default(),
        if rules.nonprod_before_prod { "  non-prod before prod" } else { "" }
    );
    out!("");
    out!(
        "{:<5} {:<12} {:<4} {:<6} {:<28} {:<10} {:<10} {}",
        "wave",
        "start",
        "day",
        "days",
        "workload",
        "env",
        "state",
        "alternatives / reason"
    );
    for s in &schedule.slots {
        let alts: Vec<String> =
            s.alternatives.iter().map(|d| d.to_string()).collect();
        let days = s.days();
        out!(
            "{:<5} {:<12} {:<4} {:<6} {:<28} {:<10} {:<10} {}  ({})",
            s.wave,
            s.date,
            day_name(s.date.weekday()),
            if s.weight > 1 {
                format!("{days}x{}", s.weight)
            } else {
                days.to_string()
            },
            s.pair.workload,
            s.pair.environment,
            s.state_label(),
            alts.join(" or "),
            s.reason()
        );
    }
    for (wave, end) in &schedule.wave_ends {
        out!("wave {wave} ends {end}");
    }
    for (pair, why) in &schedule.conflicts {
        out!("CONFLICT {pair}: {why}");
    }
    for (pair, why) in &schedule.unplaced {
        out!("UNPLACED {pair}: {why}");
    }
    if let Some(end) = schedule.ends {
        out!("");
        out!(
            "projected end {end}{}",
            match rules.target_end {
                Some(t) if end > t => format!(
                    "  LATE by {} days against {t}",
                    (end - t).get_days()
                ),
                Some(t) => format!("  within target {t}"),
                None => String::new(),
            }
        );
    }
    for pair in &schedule.expired {
        out!(
            "expired: {pair} confirmed date passed without a migration; rescheduled"
        );
    }
    Ok(())
}

pub(crate) async fn run(
    store: &Store,
    command: ScheduleCommand,
) -> anyhow::Result<()> {
    match command {
        ScheduleCommand::Show => {
            print_schedule(store, Some(Trigger::Command)).await?
        }
        ScheduleCommand::Set {
            per_day,
            per_week,
            per_month,
            weekdays,
            target_end,
            nonprod_before_prod,
        } => {
            let mut rules = store.global_rules().await?;
            if let Some(v) = per_day {
                rules.per_day = v;
            }
            if let Some(v) = per_week {
                rules.per_week = v;
            }
            if let Some(v) = per_month {
                rules.per_month = v;
            }
            if let Some(v) = weekdays {
                rules.weekdays = v;
            }
            if let Some(TargetEnd(v)) = target_end {
                rules.target_end = v;
            }
            if let Some(v) = nonprod_before_prod {
                rules.nonprod_before_prod = v;
            }
            store.set_global_rules(&rules).await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Blackout {
            name,
            start,
            end,
            workload,
            environment,
            remove,
        } => {
            let b = precip_engine::schedule::Blackout {
                name,
                start,
                end: end.unwrap_or(start),
                pair: workload
                    .zip(environment)
                    .map(|(w, e)| PairKey::new(w, e)),
            };
            if remove {
                store.remove_blackout(&b).await?;
            } else {
                store.add_blackout(&b).await?;
            }
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Blackouts => {
            let today = today();
            for b in
                store.blackouts().await?.into_iter().filter(|b| b.end >= today)
            {
                out!(
                    "{} to {}  {}{}",
                    b.start,
                    b.end,
                    b.name,
                    b.pair
                        .map(|pair| format!("  (only {pair})"))
                        .unwrap_or_default()
                );
            }
        }
        ScheduleCommand::Holidays { country, year, only, skip } => {
            anyhow::ensure!(
                country.eq_ignore_ascii_case("us"),
                "only US federal holidays are built in; add others with `schedule blackout`"
            );
            let names = |list: Option<String>| -> Vec<String> {
                list.map(|l| {
                    l.split(',').map(|n| n.trim().to_lowercase()).collect()
                })
                .unwrap_or_default()
            };
            let (only, skip) = (names(only), names(skip));
            let chosen: Vec<_> =
                precip_engine::schedule::holidays::us_federal(year)
                    .into_iter()
                    .filter(|h| {
                        let n = h.name.to_lowercase();
                        (only.is_empty() || only.contains(&n))
                            && !skip.contains(&n)
                    })
                    .collect();
            store.add_holidays(&chosen).await?;
            for h in &chosen {
                out!("{}  {}", h.date, h.name);
            }
            print_schedule(store, None).await?;
        }
        ScheduleCommand::HolidayNames { country, year } => {
            anyhow::ensure!(
                country.eq_ignore_ascii_case("us"),
                "only US federal holidays are built in"
            );
            for h in precip_engine::schedule::holidays::us_federal(year) {
                out!("{}  {}", h.date, h.name);
            }
        }
        ScheduleCommand::Rule { pair, weekdays, earliest, priority } => {
            let days = weekdays.filter(|w| !w.is_any());
            store
                .set_pair_rule(
                    &pair.workload,
                    &pair.environment,
                    days.as_ref(),
                    earliest,
                    priority,
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Plan { pair, wave, duration, weight } => {
            store
                .set_pair_plan(
                    &pair.workload,
                    &pair.environment,
                    wave,
                    duration,
                    weight,
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Wave { number, name, target_end, remove } => {
            if remove {
                let moved = store.remove_wave(number).await?;
                out!("wave {number} removed; {moved} pair(s) back in wave 1");
            } else {
                store.set_wave(number, &name, target_end).await?;
            }
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Depends { pair, on_workload, on_environment } => {
            store
                .add_dependency(
                    &pair.workload,
                    &pair.environment,
                    &on_workload,
                    &on_environment,
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Confirm { pair, date, reason } => {
            set_status(store, &pair, Status::Confirmed, Some(date), reason)
                .await?;
        }
        ScheduleCommand::Migrated { pair, date } => {
            set_status(store, &pair, Status::Migrated, date, None).await?;
        }
        ScheduleCommand::Missed { pair, reason } => {
            set_status(store, &pair, Status::Missed, None, reason).await?;
        }
        ScheduleCommand::Reasons => {
            for r in precip_db::RESCHEDULE_REASONS {
                out!("{r}");
            }
        }
        ScheduleCommand::RolledBack { pair, note } => {
            set_status(store, &pair, Status::RolledBack, None, note).await?;
        }
        ScheduleCommand::Verify => {
            let Some(latest) = store.list_scans().await?.into_iter().next()
            else {
                anyhow::bail!("no scans yet");
            };
            let findings = store.verify_migrated(&latest.id).await?;
            if findings.is_empty() {
                out!("every migrated pair is gone from scan #{}", latest.seq);
            }
            for f in findings {
                out!(
                    "{} / {}: marked migrated but {} resource(s) still in scan #{}",
                    f.workload,
                    f.environment,
                    f.remaining,
                    latest.seq
                );
            }
        }
        ScheduleCommand::Metrics => {
            let m = store.metrics(today()).await?;
            print_metrics(&m);
        }
        ScheduleCommand::History => {
            for e in store.events().await? {
                out!(
                    "{}  {}{}",
                    e.at.strftime("%Y-%m-%d %H:%M"),
                    e.event.describe(),
                    e.ends
                        .map(|d| format!("   -> ends {d}"))
                        .unwrap_or_default()
                );
            }
        }
        ScheduleCommand::Gantt { width } => {
            let (_, schedule) =
                store.compute_schedule(today(), Trigger::Command).await?;
            let gantt = Gantt::new(&schedule, today(), width);
            for line in gantt.render(32) {
                out!("{line}");
            }
        }
        ScheduleCommand::Replay => {
            let (version, schedule) = store.replay().await?;
            out!(
                "rebuilt the plan from {} events; schedule #{version} ends {}",
                store.events().await?.len(),
                schedule
                    .ends
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "-".into())
            );
        }
    }
    Ok(())
}

/// Record a pair's new status from today, then show the schedule.
async fn set_status(
    store: &Store,
    pair: &PairArgs,
    status: Status,
    date: Option<Date>,
    note: Option<String>,
) -> anyhow::Result<()> {
    store
        .set_pair_status(
            &pair.workload,
            &pair.environment,
            status,
            date,
            today(),
            note.as_deref(),
        )
        .await?;
    print_schedule(store, None).await
}

pub(crate) fn today() -> Date {
    jiff::Zoned::now().date()
}

fn print_metrics(m: &precip_db::Metrics) {
    let pct = |r: f64| format!("{:.0}%", r * 100.0);
    let day = |d: Option<jiff::civil::Date>| {
        d.map(|d| d.to_string()).unwrap_or_else(|| "-".into())
    };
    out!(
        "pairs              {}  ({} migrated, {} confirmed, {} proposed, {} missed, {} rolled back)",
        m.total,
        m.migrated,
        m.confirmed,
        m.proposed,
        m.missed,
        m.rolled_back
    );
    out!(
        "reschedule rate    {}  ({} of {} confirmed migrations missed or re-confirmed)",
        pct(m.reschedule_rate),
        m.rescheduled,
        m.scheduled
    );
    out!(
        "first-time success {}  (migrated on the first confirmed date)",
        pct(m.first_time_success_rate)
    );
    out!(
        "confirmation       {}  of outstanding pairs have a confirmed date",
        pct(m.confirmation_rate)
    );
    out!(
        "projected end      {}  first projection {}  drift {:+} days  target {}",
        day(m.projected_end),
        day(m.first_projected_end),
        m.end_drift_days,
        day(m.target_end)
    );
    out!(
        "average slip       {:.1} days from first confirmed date to migration",
        m.average_slip_days
    );
    out!(
        "last 30 days       {} migrated against a capacity of {}",
        m.migrated_last_30_days,
        m.capacity_last_30_days
    );
    out!(
        "rollbacks          {}  ({} pairs currently rolled back)",
        pct(m.rollback_rate),
        m.rolled_back
    );
    if m.forecast_within_week_rate > 0.0 || m.forecast_error_days != 0.0 {
        out!(
            "forecast          first plan was off by {:+.1} days on average; {} of migrations landed within a week of it",
            m.forecast_error_days,
            pct(m.forecast_within_week_rate)
        );
    }
    if m.end_history.len() > 1 {
        let trail: Vec<String> = m
            .end_history
            .iter()
            .map(|(day, ends)| format!("{day}: {ends}"))
            .collect();
        out!("end over time     {}", trail.join("  ->  "));
    }
    if !m.reschedule_reasons.is_empty() {
        let list: Vec<String> = m
            .reschedule_reasons
            .iter()
            .map(|(r, n)| format!("{r} {n}"))
            .collect();
        out!("why dates moved   {}", list.join(", "));
    }
    for w in &m.waves {
        out!(
            "wave {:<3} {:<16} {}/{} migrated  ends {}{}",
            w.number,
            w.name,
            w.migrated,
            w.total,
            day(w.projected_end),
            match (w.projected_end, w.target_end) {
                (Some(e), Some(t)) if e > t =>
                    format!("  LATE by {} days against {t}", (e - t).get_days()),
                (_, Some(t)) => format!("  target {t}"),
                _ => String::new(),
            }
        );
    }
}
