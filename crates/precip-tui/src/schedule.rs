//! The Schedule tab: the calendar, agreeing dates, and why dates moved.

use crate::{Action, App, TuiError};
use precip_db::Trigger;
use precip_engine::schedule::{Blackout, GlobalRules, Schedule, Slot, Status};
use ratatui::widgets::ListState;

/// The computed migration calendar and the controls over it.
pub(crate) struct ScheduleView {
    pub rules: GlobalRules,
    pub schedule: Schedule,
    pub metrics: precip_db::Metrics,
    /// Migrated pairs whose compute the latest scan still sees.
    pub findings: Vec<precip_db::Verification>,
    pub version: i64,
    pub selected: ratatui::widgets::TableState,
    pub today: jiff::civil::Date,
    /// Show the Gantt chart instead of the table.
    pub gantt: bool,
}

/// What a reason is being asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pending {
    Missed { workload: String, environment: String },
    Reconfirm { workload: String, environment: String, date: jiff::civil::Date },
}

/// Why an agreed date moved: the usual reasons, or typed.
pub(crate) struct ReasonPicker {
    pub pending: Pending,
    pub state: ListState,
    /// Text being typed for the "other" row.
    pub custom: Option<String>,
}

impl ReasonPicker {
    pub(crate) fn rows() -> usize {
        precip_db::RESCHEDULE_REASONS.len() + 1
    }
}

/// Every computed schedule, oldest first.
pub(crate) struct HistoryView {
    pub events: Vec<precip_db::PlanEventRecord>,
    pub state: ListState,
}

/// Picker for confirming a date: the proposed date and its alternatives.
pub(crate) struct DatePicker {
    pub pair: precip_types::PairKey,
    pub dates: Vec<jiff::civil::Date>,
    /// True when a confirmed date is being replaced by another.
    pub replacing: bool,
    pub state: ListState,
}

/// The one sentence that says what to do now on the Schedule tab.
pub(crate) fn next_step(v: &ScheduleView) -> String {
    let s = &v.schedule;
    let m = &v.metrics;
    if !v.findings.is_empty() {
        return format!(
            "{} pair(s) marked done still have resources in the cloud. Select one below and press enter.",
            v.findings.len()
        );
    }
    if !s.conflicts.is_empty() {
        return "An agreed date breaks the order of the plan. See the activity feed and agree a different date.".into();
    }
    if m.total == 0 {
        return "Nothing to schedule yet. Give workloads a disposition on the Workloads tab.".into();
    }
    if s.slots.is_empty() {
        return "Everything is migrated.".into();
    }
    let today_runs: Vec<&Slot> = s
        .slots
        .iter()
        .filter(|x| x.agreed && x.date <= v.today && x.end >= v.today)
        .collect();
    if let Some(x) = today_runs.first() {
        return format!(
            "{} runs today. When it finishes, select it, press enter and choose It's done.",
            x.pair
        );
    }
    let open = s.slots.iter().filter(|x| !x.agreed).count();
    if open > 0 {
        return format!(
            "{open} proposed date(s) need agreement. Select a row, press enter and choose Agree a date.",
        );
    }
    "All dates are agreed. Come back on each date to mark it done.".into()
}

/// A freeze from its three boxes. An empty "to" means the one day.
pub(crate) fn parse_freeze(
    name: &str,
    from: &str,
    to: &str,
    pair: Option<precip_types::PairKey>,
) -> Result<Blackout, String> {
    if name.is_empty() {
        return Err("the freeze needs a name".into());
    }
    let start: jiff::civil::Date = from
        .parse()
        .map_err(|_| format!("from: {from:?} is not a date like 2026-12-20"))?;
    let end = if to.is_empty() {
        start
    } else {
        to.parse()
            .map_err(|_| format!("to: {to:?} is not a date like 2027-01-05"))?
    };
    if start > end {
        return Err("the freeze ends before it starts".into());
    }
    Ok(Blackout { name: name.to_string(), start, end, pair })
}

/// A date box that may be left empty to mean none.
pub(crate) fn parse_optional_date(
    text: &str,
) -> Result<Option<jiff::civil::Date>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|_| format!("{text:?} is not a date like 2026-12-15"))
}

impl App {
    /// Recompute the calendar from today and keep the selection.
    pub(crate) fn recompute_schedule(
        &mut self,
        trigger: Trigger,
    ) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        let today = jiff::Zoned::now().date();
        let rules = rt.block_on(store.global_rules())?;
        // Every change already produced a version through the event log;
        // only opening the workspace (a new day) computes afresh.
        let latest = if trigger == Trigger::Open {
            None
        } else {
            rt.block_on(store.latest_schedule())?
                .filter(|(_, as_of, _)| *as_of == today)
        };
        let (version, schedule) = match latest {
            Some((id, _, schedule)) => (id, schedule),
            None => rt.block_on(store.compute_schedule(today, trigger))?,
        };
        let metrics = rt.block_on(store.metrics(today))?;
        let findings = match self.scans.first() {
            Some(latest) => rt.block_on(store.verify_migrated(&latest.id))?,
            None => Vec::new(),
        };
        let gantt = self.schedule.as_ref().is_some_and(|v| v.gantt);
        let mut selected =
            self.schedule.as_ref().map(|v| v.selected).unwrap_or_default();
        if selected.selected().is_none() && !schedule.slots.is_empty() {
            selected.select(Some(0));
        }
        let expired = schedule.expired.clone();
        self.schedule = Some(ScheduleView {
            rules,
            schedule,
            metrics,
            findings,
            version,
            selected,
            today,
            gantt,
        });
        for pair in expired {
            self.note(format!(
                "{pair}: confirmed date passed without a migration, rescheduled"
            ));
        }
        let conflicts =
            self.schedule.as_ref().map(|v| v.schedule.conflicts.clone());
        for (pair, why) in conflicts.unwrap_or_default() {
            self.note(format!("{pair}: confirmed date {why}"));
        }
        Ok(())
    }

    pub(crate) fn selected_slot(&self) -> Option<Slot> {
        let v = self.schedule.as_ref()?;
        v.schedule.slots.get(v.selected.selected()?).cloned()
    }

    pub(crate) fn change_status(
        &mut self,
        workload: &str,
        environment: &str,
        status: Status,
        date: Option<jiff::civil::Date>,
        note: Option<&str>,
    ) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let today = jiff::Zoned::now().date();
        let result =
            tokio::runtime::Handle::current().block_on(store.set_pair_status(
                workload,
                environment,
                status,
                date,
                today,
                note,
            ));
        match result {
            Ok(()) => {
                let what = match status {
                    Status::Proposed => "back to proposed",
                    Status::Confirmed => "date agreed",
                    Status::Missed => "didn't happen, a new date is proposed",
                    Status::Migrated => "done",
                    Status::RolledBack => "rolled back, a new date is proposed",
                };
                self.note(format!(
                    "{workload} / {environment}: {what}{}",
                    date.map(|d| format!(" ({d})")).unwrap_or_default()
                ));
                self.recompute_schedule(Trigger::StatusChange)?;
            }
            Err(precip_db::DbError::Invalid(msg)) => {
                self.note(format!("{workload} / {environment}: {msg}"));
            }
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    pub(crate) fn ask_reason(&mut self, pending: Pending) {
        let mut state = ListState::default();
        state.select(Some(0));
        self.popups.open(ReasonPicker { pending, state, custom: None });
    }

    /// Enter in the reason picker: a listed reason applies at once, the
    /// last row opens a box to type one.
    pub(crate) fn reason_enter(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.popups.get_mut::<ReasonPicker>() else {
            return Ok(());
        };
        let i = p.state.selected().unwrap_or(0);
        let reason = match p.custom.take() {
            Some(text) => text.trim().to_string(),
            None if i < precip_db::RESCHEDULE_REASONS.len() => {
                precip_db::RESCHEDULE_REASONS[i].to_string()
            }
            None => {
                p.custom = Some(String::new());
                return Ok(());
            }
        };
        if reason.is_empty() {
            self.note("a reason is needed so the reschedule rate can say why");
            return Ok(());
        }
        let Some(p) = self.popups.close::<ReasonPicker>() else {
            return Ok(());
        };
        match p.pending {
            Pending::Missed { workload, environment } => self.change_status(
                &workload,
                &environment,
                Status::Missed,
                None,
                Some(&reason),
            ),
            Pending::Reconfirm { workload, environment, date } => self
                .change_status(
                    &workload,
                    &environment,
                    Status::Confirmed,
                    Some(date),
                    Some(&reason),
                ),
        }
    }

    /// Open the menu for the selected schedule row: a slot, or a finding
    /// below the slots.
    pub(crate) fn open_action_menu(&mut self) {
        let Some(v) = &self.schedule else { return };
        let Some(i) = v.selected.selected() else { return };
        let today = v.today;
        let (workload, environment, actions) =
            if let Some(slot) = v.schedule.slots.get(i) {
                let mut actions = Vec::new();
                if slot.agreed {
                    actions.push(Action::ChangeDate);
                    actions.push(Action::Done);
                    if slot.date <= today {
                        actions.push(Action::DidNotHappen);
                    }
                } else {
                    actions.push(Action::AgreeDate);
                    actions.push(Action::Done);
                }
                actions.push(Action::PairSettings);
                (
                    slot.pair.workload.to_string(),
                    slot.pair.environment.to_string(),
                    actions,
                )
            } else if let Some(f) = v.findings.get(i - v.schedule.slots.len()) {
                (
                    f.workload.clone(),
                    f.environment.clone(),
                    vec![Action::RolledBack],
                )
            } else {
                return;
            };
        let title = format!("{workload} / {environment}");
        self.show_menu(title, workload, environment, actions);
    }

    pub(crate) fn open_history(&mut self) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let events =
            tokio::runtime::Handle::current().block_on(store.events())?;
        let mut state = ListState::default();
        if !events.is_empty() {
            state.select(Some(events.len() - 1));
        }
        self.popups.open(HistoryView { events, state });
        Ok(())
    }

    pub(crate) fn open_date_picker(&mut self) {
        let Some(slot) = self.selected_slot() else { return };
        // A confirmed date never moves, but another future date can be
        // confirmed in its place; the picker then offers only alternatives.
        let mut dates = if slot.agreed { Vec::new() } else { vec![slot.date] };
        dates.extend(slot.alternatives.iter().copied());
        if dates.is_empty() {
            self.note("no open dates to offer; adjust the rules or blackouts");
            return;
        }
        let mut state = ListState::default();
        state.select(Some(0));
        self.popups.open(DatePicker {
            pair: slot.pair,
            dates,
            replacing: slot.agreed,
            state,
        });
    }

    /// Agree the date picked, or ask why when it replaces an agreed one.
    pub(crate) fn date_enter(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.popups.close::<DatePicker>() else { return Ok(()) };
        let Some(d) = p.state.selected().and_then(|i| p.dates.get(i).copied())
        else {
            return Ok(());
        };
        let (w, e) =
            (p.pair.workload.to_string(), p.pair.environment.to_string());
        if p.replacing {
            self.ask_reason(Pending::Reconfirm {
                workload: w,
                environment: e,
                date: d,
            });
            Ok(())
        } else {
            self.change_status(&w, &e, Status::Confirmed, Some(d), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn a_freeze_with_no_end_is_one_day() {
        let b = parse_freeze("audit", "2026-11-03", "", None).unwrap();
        assert_eq!((b.start, b.end), (date(2026, 11, 3), date(2026, 11, 3)));
        let b =
            parse_freeze("close", "2026-12-20", "2027-01-05", None).unwrap();
        assert_eq!(b.end, date(2027, 1, 5));
        assert!(parse_freeze("", "2026-11-03", "", None).is_err());
        assert!(parse_freeze("x", "tomorrow", "", None).is_err());
        assert!(parse_freeze("x", "2026-11-03", "2026-11-01", None).is_err());
    }
}
