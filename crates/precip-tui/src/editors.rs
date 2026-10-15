//! The rules editor and the pair editor, with the pickers they open.

use crate::{App, TuiError, parse_freeze, parse_optional_date};
use precip_db::Trigger;
use ratatui::widgets::ListState;

/// Picker for the global weekday rule: three presets, then each day of
/// the week as a toggle.
pub struct WeekdayPicker {
    pub days: precip_engine::schedule::Weekdays,
    pub state: ListState,
    /// The editor the answer goes back to.
    pub target: WeekdayTarget,
}

impl WeekdayPicker {
    fn new(
        days: precip_engine::schedule::Weekdays,
        target: WeekdayTarget,
    ) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self { days, state, target }
    }

    /// Apply the highlighted row: a preset replaces the set, a day
    /// toggles in or out.
    pub(crate) fn toggle_selected(&mut self) {
        use precip_engine::schedule::{WeekdayPreset, Weekdays};
        let Some(i) = self.state.selected() else { return };
        match i {
            0 => self.days = WeekdayPreset::Any.days(),
            1 => self.days = WeekdayPreset::Weekdays.days(),
            2 => self.days = WeekdayPreset::Weekends.days(),
            _ => {
                if let Some(day) = Weekdays::EVERY_DAY.get(i - 3) {
                    self.days.toggle(*day);
                }
            }
        }
    }
}

/// Rows of the weekday picker, presets first.
pub const WEEKDAY_ROWS: [&str; 10] = [
    "any day", "weekdays", "weekends", "mon", "tue", "wed", "thu", "fri",
    "sat", "sun",
];

/// A small form inside an editor row: one box per field, Tab between
/// them, so the shape of what to type is visible before typing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fields {
    pub labels: Vec<&'static str>,
    pub values: Vec<String>,
    pub active: usize,
}

impl Fields {
    pub fn new(labels: &[&'static str], values: &[&str]) -> Self {
        let mut values: Vec<String> =
            values.iter().map(|v| (*v).to_string()).collect();
        values.resize(labels.len(), String::new());
        Self { labels: labels.to_vec(), values, active: 0 }
    }

    pub fn get(&self, i: usize) -> &str {
        self.values.get(i).map(String::as_str).unwrap_or_default().trim()
    }

    pub fn next(&mut self) {
        self.active = (self.active + 1) % self.labels.len();
    }

    pub fn prev(&mut self) {
        self.active = (self.active + self.labels.len() - 1) % self.labels.len();
    }

    pub fn push(&mut self, c: char) {
        if let Some(v) = self.values.get_mut(self.active) {
            v.push(c);
        }
    }

    pub fn pop(&mut self) {
        if let Some(v) = self.values.get_mut(self.active) {
            v.pop();
        }
    }
}

/// Rows of the rules editor, rebuilt after every change. Headers are
/// skipped when moving the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleRow {
    Header(&'static str),
    PerDay,
    PerWeek,
    PerMonth,
    Weekdays,
    TargetEnd,
    NonprodFirst,
    /// A freeze, by index into `blackouts`.
    Blackout(usize),
    AddBlackout,
    Holidays,
    Wave(usize),
    AddWave,
}

impl RuleRow {
    pub fn is_header(self) -> bool {
        matches!(self, RuleRow::Header(_))
    }
}

/// The global rules, freezes and waves, editable in words.
pub struct RulesEditor {
    pub rules: precip_engine::schedule::GlobalRules,
    pub blackouts: Vec<precip_engine::schedule::Blackout>,
    pub waves: Vec<precip_engine::schedule::Wave>,
    pub rows: Vec<RuleRow>,
    pub today: jiff::civil::Date,
    pub state: ListState,
    /// The form being filled in for the selected row.
    pub input: Option<Fields>,
    /// A row waiting for a second Enter to delete it.
    pub confirming_delete: bool,
}

impl RulesEditor {
    pub fn rebuild_rows(&mut self) {
        let mut rows = vec![
            RuleRow::Header("PACE"),
            RuleRow::PerDay,
            RuleRow::PerWeek,
            RuleRow::PerMonth,
            RuleRow::Weekdays,
            RuleRow::TargetEnd,
            RuleRow::NonprodFirst,
            RuleRow::Header("FREEZES"),
        ];
        // Freezes that already passed are over; nothing to decide.
        rows.extend(
            self.blackouts
                .iter()
                .enumerate()
                .filter(|(_, b)| b.end >= self.today)
                .map(|(i, _)| RuleRow::Blackout(i)),
        );
        rows.push(RuleRow::AddBlackout);
        rows.push(RuleRow::Holidays);
        rows.push(RuleRow::Header("WAVES"));
        rows.extend((0..self.waves.len()).map(RuleRow::Wave));
        rows.push(RuleRow::AddWave);
        self.rows = rows;
        let i = self.state.selected().unwrap_or(1).min(self.rows.len() - 1);
        self.state.select(Some(if self.rows[i].is_header() {
            i + 1
        } else {
            i
        }));
    }

    pub fn row(&self) -> RuleRow {
        self.rows[self.state.selected().unwrap_or(1)]
    }

    pub fn step(&mut self, delta: isize) {
        let i = self.state.selected().unwrap_or(1);
        self.state.select(Some(step_over_headers(
            i,
            delta,
            self.rows.len(),
            |j| self.rows[j].is_header(),
        )));
    }
}

/// Move a selection by one, skipping header rows and staying in range.
pub(crate) fn step_over_headers(
    from: usize,
    delta: isize,
    len: usize,
    is_header: impl Fn(usize) -> bool,
) -> usize {
    let mut i = from;
    loop {
        let Some(next) = i.checked_add_signed(delta).filter(|n| *n < len)
        else {
            return from;
        };
        i = next;
        if !is_header(i) {
            return i;
        }
    }
}

/// Rows of the per-pair editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRow {
    Header(&'static str),
    Wave,
    Weekdays,
    Earliest,
    Duration,
    Weight,
    Priority,
    Dependency(usize),
    AddDependency,
    Freeze(usize),
    AddFreeze,
}

impl PairRow {
    pub fn is_header(self) -> bool {
        matches!(self, PairRow::Header(_))
    }
}

/// One pair's migration settings, editable in words.
pub struct PairEditor {
    pub pair: precip_engine::schedule::PairInput,
    /// Freezes that apply only to this pair.
    pub freezes: Vec<precip_engine::schedule::Blackout>,
    pub rows: Vec<PairRow>,
    pub state: ListState,
    pub input: Option<Fields>,
    pub confirming_delete: bool,
}

impl PairEditor {
    pub fn rebuild_rows(&mut self) {
        let mut rows = vec![
            PairRow::Header("WHEN"),
            PairRow::Wave,
            PairRow::Weekdays,
            PairRow::Earliest,
            PairRow::Header("HOW BIG"),
            PairRow::Duration,
            PairRow::Weight,
            PairRow::Priority,
            PairRow::Header("WAITS FOR"),
        ];
        rows.extend((0..self.pair.depends_on.len()).map(PairRow::Dependency));
        rows.push(PairRow::AddDependency);
        rows.push(PairRow::Header("FREEZES FOR THIS PAIR"));
        rows.extend((0..self.freezes.len()).map(PairRow::Freeze));
        rows.push(PairRow::AddFreeze);
        self.rows = rows;
        let i = self.state.selected().unwrap_or(1).min(self.rows.len() - 1);
        self.state.select(Some(if self.rows[i].is_header() {
            i + 1
        } else {
            i
        }));
    }

    pub fn row(&self) -> PairRow {
        self.rows[self.state.selected().unwrap_or(1)]
    }

    pub fn step(&mut self, delta: isize) {
        let i = self.state.selected().unwrap_or(1);
        self.state.select(Some(step_over_headers(
            i,
            delta,
            self.rows.len(),
            |j| self.rows[j].is_header(),
        )));
    }
}

/// Which editor the weekday picker reports back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeekdayTarget {
    Global,
    Pair,
}

/// A year's holidays with a tick each; only the ticked ones become
/// freezes, since not every company honours every one.
pub struct HolidayPicker {
    pub year: i16,
    pub holidays: Vec<precip_engine::schedule::holidays::Holiday>,
    pub chosen: Vec<bool>,
    pub state: ListState,
}

/// Pick another pair for the open pair editor to depend on.
pub struct DependsPicker {
    pub candidates: Vec<precip_types::PairKey>,
    pub state: ListState,
}

impl App {
    // ---- the rules editor -------------------------------------------

    pub(crate) fn open_rules_editor(&mut self) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        let rules = rt.block_on(store.global_rules())?;
        let blackouts = rt.block_on(store.blackouts())?;
        let waves = rt.block_on(store.waves())?;
        let mut state = ListState::default();
        state.select(Some(1));
        let mut ed = RulesEditor {
            rules,
            blackouts,
            waves,
            rows: Vec::new(),
            today: jiff::Zoned::now().date(),
            state,
            input: None,
            confirming_delete: false,
        };
        ed.rebuild_rows();
        self.popups.open(ed);
        Ok(())
    }

    pub(crate) fn save_rules(&mut self) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get::<RulesEditor>() else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let rules = ed.rules.clone();
        tokio::runtime::Handle::current()
            .block_on(store.set_global_rules(&rules))?;
        self.recompute_schedule(Trigger::RuleChange)
    }

    /// Left and right on a rules row: nudge a number or flip a switch.
    pub(crate) fn rules_adjust(&mut self, delta: i64) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get_mut::<RulesEditor>() else {
            return Ok(());
        };
        if ed.input.is_some() {
            return Ok(());
        }
        let bump = |n: u32| -> u32 {
            u32::try_from((i64::from(n) + delta).clamp(1, 1000)).unwrap_or(1)
        };
        match ed.row() {
            RuleRow::PerDay => ed.rules.per_day = bump(ed.rules.per_day),
            RuleRow::PerWeek => ed.rules.per_week = bump(ed.rules.per_week),
            RuleRow::PerMonth => ed.rules.per_month = bump(ed.rules.per_month),
            RuleRow::TargetEnd => {
                // The target end moves a week at a time.
                if let Some(t) = ed.rules.target_end {
                    ed.rules.target_end =
                        t.checked_add(jiff::Span::new().weeks(delta)).ok();
                }
            }
            RuleRow::NonprodFirst => {
                ed.rules.nonprod_before_prod = !ed.rules.nonprod_before_prod;
            }
            RuleRow::Header(_)
            | RuleRow::Weekdays
            | RuleRow::Blackout(_)
            | RuleRow::AddBlackout
            | RuleRow::Wave(_)
            | RuleRow::AddWave
            | RuleRow::Holidays => return Ok(()),
        }
        self.save_rules()
    }

    /// Enter on a rules row: open the day picker, start typing, flip the
    /// switch, or delete a freeze (asked twice).
    pub(crate) fn rules_enter(&mut self) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get_mut::<RulesEditor>() else {
            return Ok(());
        };
        if let Some(fields) = ed.input.take() {
            return self.rules_apply_input(&fields);
        }
        match ed.row() {
            RuleRow::Weekdays => {
                let days = ed.rules.weekdays.clone();
                self.popups
                    .open(WeekdayPicker::new(days, WeekdayTarget::Global));
            }
            RuleRow::TargetEnd => {
                let current = ed
                    .rules
                    .target_end
                    .map(|d| d.to_string())
                    .unwrap_or_default();
                ed.input = Some(Fields::new(&["date"], &[&current]));
            }
            RuleRow::NonprodFirst => return self.rules_adjust(1),
            RuleRow::AddBlackout => {
                ed.input = Some(Fields::new(&["name", "from", "to"], &[]));
            }
            RuleRow::AddWave => {
                ed.input = Some(Fields::new(&["number", "name", "due"], &[]));
            }
            RuleRow::Holidays => ed.input = Some(Fields::new(&["year"], &[])),
            RuleRow::Wave(i) => {
                let w = &ed.waves[i];
                let due =
                    w.target_end.map(|d| d.to_string()).unwrap_or_default();
                ed.input =
                    Some(Fields::new(&["name", "due"], &[&w.name, &due]));
            }
            RuleRow::Blackout(i) => {
                if !ed.confirming_delete {
                    ed.confirming_delete = true;
                    return Ok(());
                }
                let b = ed.blackouts.remove(i);
                ed.confirming_delete = false;
                ed.rebuild_rows();
                if let Some(store) = &self.store {
                    tokio::runtime::Handle::current()
                        .block_on(store.remove_blackout(&b))?;
                }
                self.note(format!("removed the {} freeze", b.name));
                return self.recompute_schedule(Trigger::RuleChange);
            }
            RuleRow::Header(_)
            | RuleRow::PerDay
            | RuleRow::PerWeek
            | RuleRow::PerMonth => {}
        }
        Ok(())
    }

    /// Backspace on a wave row: ask once, then remove it.
    pub(crate) fn rules_remove_wave(&mut self) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get_mut::<RulesEditor>() else {
            return Ok(());
        };
        let RuleRow::Wave(i) = ed.row() else { return Ok(()) };
        if !ed.confirming_delete {
            ed.confirming_delete = true;
            return Ok(());
        }
        ed.confirming_delete = false;
        let number = ed.waves[i].number;
        let Some(store) = &self.store else { return Ok(()) };
        match tokio::runtime::Handle::current()
            .block_on(store.remove_wave(number))
        {
            Ok(moved) => {
                self.reload_rules_editor()?;
                self.note(format!(
                    "wave {number} removed; {moved} pair(s) back in wave 1"
                ));
                self.recompute_schedule(Trigger::RuleChange)
            }
            Err(precip_db::DbError::Invalid(msg)) => {
                self.note(msg);
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Re-read freezes and waves after a change the editor cannot apply
    /// to its own copy.
    pub(crate) fn reload_rules_editor(&mut self) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let Some(ed) = self.popups.get_mut::<RulesEditor>() else {
            return Ok(());
        };
        let rt = tokio::runtime::Handle::current();
        ed.blackouts = rt.block_on(store.blackouts())?;
        ed.waves = rt.block_on(store.waves())?;
        ed.rebuild_rows();
        Ok(())
    }

    /// Typed text for the target end ("2026-12-15" or empty to clear), a
    /// new freeze ("name 2026-12-20 2027-01-05"), a wave ("3 finance
    /// 2026-12-01" to add, "finance 2026-12-01" on a wave row), or a year
    /// of US holidays.
    pub(crate) fn rules_apply_input(
        &mut self,
        f: &Fields,
    ) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get_mut::<RulesEditor>() else {
            return Ok(());
        };
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        match ed.row() {
            RuleRow::TargetEnd => match parse_optional_date(f.get(0)) {
                Ok(d) => {
                    ed.rules.target_end = d;
                    self.save_rules()
                }
                Err(msg) => {
                    self.note(msg);
                    Ok(())
                }
            },
            RuleRow::AddBlackout => {
                let b = match parse_freeze(f.get(0), f.get(1), f.get(2), None) {
                    Ok(b) => b,
                    Err(msg) => {
                        self.note(msg);
                        return Ok(());
                    }
                };
                ed.blackouts.push(b.clone());
                ed.blackouts.sort_by_key(|b| (b.start, b.name.clone()));
                ed.rebuild_rows();
                rt.block_on(store.add_blackout(&b))?;
                self.note(format!(
                    "no migrations from {} to {} ({})",
                    b.start, b.end, b.name
                ));
                self.recompute_schedule(Trigger::RuleChange)
            }
            RuleRow::Holidays => {
                let Ok(year) = f.get(0).parse::<i16>() else {
                    self.note("the year should look like 2027");
                    return Ok(());
                };
                let holidays =
                    precip_engine::schedule::holidays::us_federal(year);
                let chosen = vec![true; holidays.len()];
                let mut state = ListState::default();
                state.select(Some(0));
                self.popups.open(HolidayPicker {
                    year,
                    holidays,
                    chosen,
                    state,
                });
                Ok(())
            }
            RuleRow::Wave(i) => {
                let number = ed.waves[i].number;
                let name = f.get(0).to_string();
                let due = match parse_optional_date(f.get(1)) {
                    Ok(d) => d,
                    Err(msg) => {
                        self.note(msg);
                        return Ok(());
                    }
                };
                rt.block_on(store.set_wave(number, &name, due))?;
                ed.waves = rt.block_on(store.waves())?;
                ed.rebuild_rows();
                self.note(format!("wave {number} is now {name}"));
                self.recompute_schedule(Trigger::RuleChange)
            }
            RuleRow::AddWave => {
                let Ok(number) = f.get(0).parse::<u32>() else {
                    self.note(
                        "the wave number should be a whole number like 3",
                    );
                    return Ok(());
                };
                let name = f.get(1).to_string();
                let due = match parse_optional_date(f.get(2)) {
                    Ok(d) => d,
                    Err(msg) => {
                        self.note(msg);
                        return Ok(());
                    }
                };
                match rt.block_on(store.set_wave(number, &name, due)) {
                    Ok(()) => {}
                    Err(precip_db::DbError::Invalid(msg)) => {
                        self.note(msg);
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                }
                ed.waves = rt.block_on(store.waves())?;
                ed.rebuild_rows();
                self.note(format!("wave {number} added"));
                self.recompute_schedule(Trigger::RuleChange)
            }
            RuleRow::Header(_)
            | RuleRow::PerDay
            | RuleRow::PerWeek
            | RuleRow::PerMonth
            | RuleRow::Weekdays
            | RuleRow::NonprodFirst
            | RuleRow::Blackout(_) => Ok(()),
        }
    }

    // ---- the per-pair editor ----------------------------------------

    pub(crate) fn open_pair_editor(
        &mut self,
        workload: &str,
        environment: &str,
    ) -> Result<(), TuiError> {
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        let Some(pair) = rt
            .block_on(store.schedule_inputs())?
            .into_iter()
            .find(|p| p.key.is(workload, environment))
        else {
            self.note(format!(
                "{workload} / {environment} is not migrating (retire or retain), so it has no migration settings"
            ));
            return Ok(());
        };
        let freezes = rt
            .block_on(store.blackouts())?
            .into_iter()
            .filter(|b| {
                b.pair.as_ref().is_some_and(|p| p.is(workload, environment))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(1));
        let mut ed = PairEditor {
            pair,
            freezes,
            rows: Vec::new(),
            state,
            input: None,
            confirming_delete: false,
        };
        ed.rebuild_rows();
        self.popups.open(ed);
        Ok(())
    }

    pub(crate) fn save_pair(&mut self) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get::<PairEditor>() else { return Ok(()) };
        let Some(store) = &self.store else { return Ok(()) };
        let p = ed.pair.clone();
        let rt = tokio::runtime::Handle::current();
        let (w, e) = (p.key.workload.as_str(), p.key.environment.as_str());
        rt.block_on(store.set_pair_rule(
            w,
            e,
            p.weekdays.as_ref(),
            p.earliest,
            p.priority,
        ))?;
        rt.block_on(store.set_pair_plan(
            w,
            e,
            p.wave,
            p.duration.get(),
            p.weight.get(),
        ))?;
        self.recompute_schedule(Trigger::RuleChange)
    }

    pub(crate) fn pair_adjust(&mut self, delta: i64) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get_mut::<PairEditor>() else {
            return Ok(());
        };
        if ed.input.is_some() {
            return Ok(());
        }
        let bump = |n: u32| -> std::num::NonZeroU32 {
            u32::try_from((i64::from(n) + delta).clamp(1, 1000))
                .ok()
                .and_then(std::num::NonZeroU32::new)
                .unwrap_or(std::num::NonZeroU32::MIN)
        };
        match ed.row() {
            PairRow::Wave => ed.pair.wave = bump(ed.pair.wave).get(),
            PairRow::Duration => {
                ed.pair.duration = bump(ed.pair.duration.get());
            }
            PairRow::Weight => ed.pair.weight = bump(ed.pair.weight.get()),
            PairRow::Priority => {
                ed.pair.priority = i32::try_from(
                    (i64::from(ed.pair.priority) + delta).clamp(-100, 100),
                )
                .unwrap_or(0);
            }
            PairRow::Earliest => {
                if let Some(t) = ed.pair.earliest {
                    ed.pair.earliest =
                        t.checked_add(jiff::Span::new().weeks(delta)).ok();
                }
            }
            PairRow::Header(_)
            | PairRow::Weekdays
            | PairRow::Dependency(_)
            | PairRow::AddDependency
            | PairRow::Freeze(_)
            | PairRow::AddFreeze => return Ok(()),
        }
        self.save_pair()
    }

    pub(crate) fn pair_enter(&mut self) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get_mut::<PairEditor>() else {
            return Ok(());
        };
        if let Some(fields) = ed.input.take() {
            return self.pair_apply_input(&fields);
        }
        let Some(store) = &self.store else { return Ok(()) };
        let rt = tokio::runtime::Handle::current();
        match ed.row() {
            PairRow::Weekdays => {
                // The picker shows "any day" for a pair that follows the
                // global rule.
                let days = ed.pair.weekdays.clone().unwrap_or_default();
                self.popups.open(WeekdayPicker::new(days, WeekdayTarget::Pair));
            }
            PairRow::Earliest => {
                let current =
                    ed.pair.earliest.map(|d| d.to_string()).unwrap_or_default();
                ed.input = Some(Fields::new(&["date"], &[&current]));
            }
            PairRow::AddFreeze => {
                ed.input = Some(Fields::new(&["name", "from", "to"], &[]));
            }
            PairRow::AddDependency => {
                let me = ed.pair.key.clone();
                let have = ed.pair.depends_on.clone();
                let candidates: Vec<precip_types::PairKey> = rt
                    .block_on(store.schedule_inputs())?
                    .into_iter()
                    .map(|p| p.key)
                    .filter(|k| *k != me && !have.contains(k))
                    .collect();
                if candidates.is_empty() {
                    self.note("no other migrating pair to depend on");
                    return Ok(());
                }
                let mut state = ListState::default();
                state.select(Some(0));
                self.popups.open(DependsPicker { candidates, state });
            }
            PairRow::Dependency(i) => {
                if !ed.confirming_delete {
                    ed.confirming_delete = true;
                    return Ok(());
                }
                let dep = ed.pair.depends_on.remove(i);
                ed.confirming_delete = false;
                ed.rebuild_rows();
                let me = ed.pair.key.clone();
                rt.block_on(store.remove_dependency(
                    me.workload.as_str(),
                    me.environment.as_str(),
                    dep.workload.as_str(),
                    dep.environment.as_str(),
                ))?;
                self.note(format!("{me} no longer waits for {dep}"));
                return self.recompute_schedule(Trigger::RuleChange);
            }
            PairRow::Freeze(i) => {
                if !ed.confirming_delete {
                    ed.confirming_delete = true;
                    return Ok(());
                }
                let b = ed.freezes.remove(i);
                ed.confirming_delete = false;
                ed.rebuild_rows();
                rt.block_on(store.remove_blackout(&b))?;
                self.note(format!("removed the {} freeze", b.name));
                return self.recompute_schedule(Trigger::RuleChange);
            }
            PairRow::Header(_)
            | PairRow::Wave
            | PairRow::Duration
            | PairRow::Weight
            | PairRow::Priority => {}
        }
        Ok(())
    }

    pub(crate) fn pair_apply_input(
        &mut self,
        f: &Fields,
    ) -> Result<(), TuiError> {
        let Some(ed) = self.popups.get_mut::<PairEditor>() else {
            return Ok(());
        };
        let Some(store) = &self.store else { return Ok(()) };
        match ed.row() {
            PairRow::Earliest => match parse_optional_date(f.get(0)) {
                Ok(d) => {
                    ed.pair.earliest = d;
                    self.save_pair()
                }
                Err(msg) => {
                    self.note(msg);
                    Ok(())
                }
            },
            PairRow::AddFreeze => {
                let me = ed.pair.key.clone();
                let b = match parse_freeze(
                    f.get(0),
                    f.get(1),
                    f.get(2),
                    Some(me.clone()),
                ) {
                    Ok(b) => b,
                    Err(msg) => {
                        self.note(msg);
                        return Ok(());
                    }
                };
                ed.freezes.push(b.clone());
                ed.rebuild_rows();
                tokio::runtime::Handle::current()
                    .block_on(store.add_blackout(&b))?;
                self.note(format!(
                    "{me}: no migration from {} to {} ({})",
                    b.start, b.end, b.name
                ));
                self.recompute_schedule(Trigger::RuleChange)
            }
            PairRow::Header(_)
            | PairRow::Wave
            | PairRow::Duration
            | PairRow::Weight
            | PairRow::Priority
            | PairRow::Weekdays
            | PairRow::Dependency(_)
            | PairRow::AddDependency
            | PairRow::Freeze(_) => Ok(()),
        }
    }

    pub(crate) fn apply_holidays(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.popups.close::<HolidayPicker>() else {
            return Ok(());
        };
        let Some(store) = &self.store else { return Ok(()) };
        let chosen: Vec<_> = p
            .holidays
            .into_iter()
            .zip(p.chosen)
            .filter(|(_, on)| *on)
            .map(|(h, _)| h)
            .collect();
        tokio::runtime::Handle::current()
            .block_on(store.add_holidays(&chosen))?;
        self.reload_rules_editor()?;
        self.note(format!(
            "added {} of the US federal holidays for {}",
            chosen.len(),
            p.year
        ));
        self.recompute_schedule(Trigger::RuleChange)
    }

    pub(crate) fn apply_dependency(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.popups.close::<DependsPicker>() else {
            return Ok(());
        };
        let Some(chosen) = p.state.selected().and_then(|i| p.candidates.get(i))
        else {
            return Ok(());
        };
        let Some(ed) = self.popups.get_mut::<PairEditor>() else {
            return Ok(());
        };
        let Some(store) = &self.store else { return Ok(()) };
        let me = ed.pair.key.clone();
        ed.pair.depends_on.push(chosen.clone());
        ed.rebuild_rows();
        tokio::runtime::Handle::current().block_on(store.add_dependency(
            me.workload.as_str(),
            me.environment.as_str(),
            chosen.workload.as_str(),
            chosen.environment.as_str(),
        ))?;
        self.note(format!("{me} now waits for {chosen}"));
        self.recompute_schedule(Trigger::RuleChange)
    }

    /// Save the weekday picker's selection where it came from: the global
    /// rule, or the open pair's rule.
    pub(crate) fn close_weekday_picker(&mut self) -> Result<(), TuiError> {
        let Some(p) = self.popups.close::<WeekdayPicker>() else {
            return Ok(());
        };
        match p.target {
            WeekdayTarget::Pair => {
                let Some(ed) = self.popups.get_mut::<PairEditor>() else {
                    return Ok(());
                };
                // Any day here means the pair follows the global rule.
                let days = (!p.days.is_any()).then_some(p.days);
                if ed.pair.weekdays == days {
                    return Ok(());
                }
                ed.pair.weekdays = days;
                self.save_pair()
            }
            WeekdayTarget::Global => {
                let Some(store) = &self.store else { return Ok(()) };
                let Some(v) = &self.schedule else { return Ok(()) };
                if p.days == v.rules.weekdays {
                    return Ok(());
                }
                let mut rules = v.rules.clone();
                rules.weekdays = p.days;
                tokio::runtime::Handle::current()
                    .block_on(store.set_global_rules(&rules))?;
                self.note(format!("migrations may run on {}", rules.weekdays));
                if let Some(ed) = self.popups.get_mut::<RulesEditor>() {
                    ed.rules.weekdays = rules.weekdays.clone();
                }
                self.recompute_schedule(Trigger::RuleChange)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_boxes_tab_around_and_edit_the_active_one() {
        let mut f = Fields::new(&["name", "from", "to"], &["x"]);
        f.push('y');
        assert_eq!(f.get(0), "xy");
        f.next();
        f.push('2');
        f.pop();
        assert_eq!(f.get(1), "");
        f.prev();
        f.prev();
        assert_eq!(f.active, 2, "wraps around");
    }
}
