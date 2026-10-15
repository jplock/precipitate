//! Migration scheduling. Pure over the pairs to migrate, the rules and
//! the calendar, so it is testable and can be recomputed every day.
//!
//! Agreed dates are fixed points: they are placed first, consume capacity,
//! and never move. Everything else is placed wave by wave, in priority
//! order, into the first span of days that satisfies the pair's weekday
//! rule, the freezes, and the weighted per-day, per-week and per-month
//! capacity, and is offered two further starts that would also work so the
//! owners can pick one and agree it.
//!
//! A pair occupies `duration` consecutive calendar days, every one of
//! which must be allowed, and takes `weight` units of capacity on each.
//! Waves are strict: nothing in a later wave starts before the last day of
//! an earlier one.

pub mod gantt;
pub mod holidays;
mod weekdays;

pub use weekdays::{UnknownWeekday, WeekdayPreset, Weekdays, day_name};

use jiff::ToSpan;
use jiff::civil::Date;
use precip_types::PairKey;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::num::NonZeroU32;

/// The plan's limits and calendar rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalRules {
    /// Capacity in weight units. A pair of weight 1 and duration 1 is one
    /// unit on one day.
    pub per_day: u32,
    pub per_week: u32,
    pub per_month: u32,
    /// Weekdays migrations may run on when a pair has no rule of its own.
    pub weekdays: Weekdays,
    /// Everything should be migrated by this date, when set.
    pub target_end: Option<Date>,
    /// Every production pair depends on the other environments of its
    /// workload, so dev and staging move first.
    pub nonprod_before_prod: bool,
}

impl Default for GlobalRules {
    fn default() -> Self {
        Self {
            per_day: 1,
            per_week: 3,
            per_month: 10,
            weekdays: Weekdays::any(),
            target_end: None,
            nonprod_before_prod: true,
        }
    }
}

/// A date range no migration may land in. `pair` limits it to one
/// workload and environment; `None` applies to everyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blackout {
    pub name: String,
    pub start: Date,
    pub end: Date,
    pub pair: Option<PairKey>,
}

impl Blackout {
    /// Whether this freeze stops `pair` from migrating on `day`.
    pub(crate) fn covers(&self, day: Date, pair: &PairKey) -> bool {
        day >= self.start
            && day <= self.end
            && self.pair.as_ref().is_none_or(|p| p == pair)
    }
}

precip_types::named_enum! {
    /// A pair's status as stored.
    pub enum Status {
        /// Proposed by the scheduler; moves when the schedule is recomputed.
        Proposed = "proposed",
        /// Date agreed with the owners; the scheduler never moves it.
        Confirmed = "confirmed",
        /// Did not happen on its date; a new date is proposed.
        Missed = "missed",
        Migrated = "migrated",
        /// Migrated and then moved back; a new date is proposed.
        RolledBack = "rolled_back",
    }
}

impl Status {
    /// Statuses that need a date proposed.
    pub(crate) fn is_open(self) -> bool {
        match self {
            Status::Proposed | Status::Missed | Status::RolledBack => true,
            Status::Confirmed | Status::Migrated => false,
        }
    }
}

/// Where a pair stands, with the agreed date when there is one, so an
/// agreed status without a date cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    Proposed,
    /// Agreed with the owners to start on this date.
    Confirmed(Date),
    Missed,
    Migrated,
    RolledBack,
}

impl PairState {
    pub fn status(self) -> Status {
        match self {
            Self::Proposed => Status::Proposed,
            Self::Confirmed(_) => Status::Confirmed,
            Self::Missed => Status::Missed,
            Self::Migrated => Status::Migrated,
            Self::RolledBack => Status::RolledBack,
        }
    }

    /// The agreed start date, for confirmed pairs.
    pub(crate) fn agreed(self) -> Option<Date> {
        match self {
            Self::Confirmed(d) => Some(d),
            Self::Proposed
            | Self::Missed
            | Self::Migrated
            | Self::RolledBack => None,
        }
    }
}

/// A named group of pairs that migrate together, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wave {
    pub number: u32,
    pub name: String,
    pub target_end: Option<Date>,
}

/// One workload and environment to place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairInput {
    pub key: PairKey,
    /// Waves are placed in order; nothing in a later wave starts before
    /// the earlier wave's last day.
    pub wave: u32,
    /// Lower runs first. Ties break on environment (non-production first)
    /// then name.
    pub priority: i32,
    /// Consecutive calendar days the migration occupies.
    pub duration: NonZeroU32,
    /// Capacity units taken on each of those days.
    pub weight: NonZeroU32,
    /// Weekdays this pair may migrate on; `None` follows the global rule.
    pub weekdays: Option<Weekdays>,
    pub earliest: Option<Date>,
    pub state: PairState,
    /// Pairs that must migrate before this one.
    pub depends_on: Vec<PairKey>,
}

impl PairInput {
    /// A proposed pair in wave 1 with no rules of its own.
    pub fn new(key: PairKey) -> Self {
        Self {
            key,
            wave: 1,
            priority: 0,
            duration: NonZeroU32::MIN,
            weight: NonZeroU32::MIN,
            weekdays: None,
            earliest: None,
            state: PairState::Proposed,
            depends_on: Vec::new(),
        }
    }

    /// The last day of a migration that starts on `start`.
    pub(crate) fn span_end(&self, start: Date) -> Date {
        let extra = i64::from(self.duration.get()) - 1;
        start.checked_add(extra.days()).unwrap_or(start)
    }

    /// Every day of a migration that starts on `start`.
    fn span(&self, start: Date) -> impl Iterator<Item = Date> + use<> {
        days(start, self.span_end(start))
    }

    fn allows(&self, day: Date, rules: &GlobalRules) -> bool {
        self.weekdays.as_ref().unwrap_or(&rules.weekdays).allows(day.weekday())
    }

    fn sort_key(&self) -> (i32, bool, &PairKey) {
        (self.priority, self.key.environment.is_production(), &self.key)
    }
}

/// Every date from `start` to `end`, inclusive.
pub(crate) fn days(start: Date, end: Date) -> impl Iterator<Item = Date> {
    start.series(1.day()).take_while(move |d| *d <= end)
}

/// Why a date cannot be used for a migration.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error,
)]
#[serde(tag = "problem", rename_all = "snake_case")]
pub enum DateProblem {
    #[error("{date} is a {}, which the rules exclude", date.strftime("%A"))]
    ExcludedWeekday { date: Date },
    #[error("{date} is inside the {name} freeze")]
    Freeze { date: Date, name: String },
    #[error("{date} already has {agreed} agreed; the limit is {limit} per day")]
    OverDay { date: Date, agreed: u32, limit: u32 },
    #[error(
        "the week of {date} already has {agreed} agreed; the limit is {limit} per week"
    )]
    OverWeek { date: Date, agreed: u32, limit: u32 },
    #[error(
        "{} already has {agreed} agreed; the limit is {limit} per month",
        date.strftime("%B %Y")
    )]
    OverMonth { date: Date, agreed: u32, limit: u32 },
}

/// Why a pair could not be placed at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Unplaced {
    DependencyCycle,
    DependsOnLaterWave { on: PairKey },
    NoValidSpan,
}

impl fmt::Display for Unplaced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DependencyCycle => f.write_str("dependency cycle"),
            Self::DependsOnLaterWave { on } => {
                write!(f, "depends on {on} in a later wave")
            }
            Self::NoValidSpan => f.write_str("no valid span within two years"),
        }
    }
}

/// Why an agreed date breaks the plan. Reported, never moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "conflict", rename_all = "snake_case")]
pub enum Conflict {
    /// A rule changed after the date was agreed.
    Rule {
        problem: DateProblem,
    },
    BeforeDependency {
        on: PairKey,
    },
    BeforeWave {
        wave: u32,
    },
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rule { problem } => problem.fmt(f),
            Self::BeforeDependency { on } => {
                write!(f, "starts before {on} ends")
            }
            Self::BeforeWave { wave } => {
                write!(f, "starts before wave {wave} ends")
            }
        }
    }
}

/// Why a slot is on its date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "placement", rename_all = "snake_case")]
pub enum Placement {
    /// The owners agreed it.
    Agreed,
    /// Pushed past a freeze.
    AfterFreeze {
        name: String,
    },
    AfterDependencies,
    /// The first day after the previous wave ended.
    AfterWave {
        wave: u32,
    },
    FirstOpenDay,
    /// The first day the pair's own weekday rule allows.
    AllowedWeekday,
}

impl fmt::Display for Placement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agreed => f.write_str("confirmed"),
            Self::AfterFreeze { name } => write!(f, "after {name}"),
            Self::AfterDependencies => f.write_str("after its dependencies"),
            Self::AfterWave { wave } => write!(f, "after wave {wave}"),
            Self::FirstOpenDay => f.write_str("first open day"),
            Self::AllowedWeekday => f.write_str("allowed weekday"),
        }
    }
}

/// Why a slot is being placed again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reopened {
    /// Its agreed date passed or was marked missed.
    Missed,
    RolledBack,
}

/// One pair on the calendar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slot {
    pub pair: PairKey,
    pub wave: u32,
    /// First day of the migration.
    pub date: Date,
    /// Last day of the migration; the same as `date` for one-day moves.
    pub end: Date,
    pub weight: u32,
    /// The owners agreed this date; it never moves.
    pub agreed: bool,
    pub placement: Placement,
    /// Set when the pair was missed or rolled back and is placed again.
    pub reopened: Option<Reopened>,
    /// Further start dates that would also work, offered so the owners
    /// can pick one and agree it.
    pub alternatives: Vec<Date>,
}

impl Slot {
    /// Why the slot is on its date, in a sentence fragment.
    pub fn reason(&self) -> String {
        match self.reopened {
            Some(Reopened::Missed) => {
                format!("rescheduled, {}", self.placement)
            }
            Some(Reopened::RolledBack) => {
                format!("rolled back, {}", self.placement)
            }
            None => self.placement.to_string(),
        }
    }

    /// `agreed`, `rescheduled` or `proposed`.
    pub fn state_label(&self) -> &'static str {
        match (self.agreed, self.reopened) {
            (true, _) => "agreed",
            (false, Some(_)) => "rescheduled",
            (false, None) => "proposed",
        }
    }

    /// Calendar days the slot spans.
    pub fn days(&self) -> i32 {
        (self.end - self.date).get_days() + 1
    }
}

/// The computed calendar.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Schedule {
    pub slots: Vec<Slot>,
    /// Pairs scheduled after the target end date.
    pub late: Vec<PairKey>,
    /// Last scheduled day, when anything was scheduled.
    pub ends: Option<Date>,
    /// Last scheduled day of each wave that has anything scheduled.
    pub wave_ends: BTreeMap<u32, Date>,
    /// Agreed dates that passed without the pair being marked migrated.
    /// They were scheduled again as missed.
    pub expired: Vec<PairKey>,
    /// Pairs that could not be placed, with why.
    pub unplaced: Vec<(PairKey, Unplaced)>,
    /// Agreed dates that break the plan.
    pub conflicts: Vec<(PairKey, Conflict)>,
}

/// Capacity used so far, per day, ISO week and month.
struct Capacity<'a> {
    rules: &'a GlobalRules,
    per_day: HashMap<Date, u32>,
    per_week: HashMap<(i16, i8), u32>,
    per_month: HashMap<(i16, i8), u32>,
}

fn week_of(d: Date) -> (i16, i8) {
    let w = d.iso_week_date();
    (w.year(), w.week())
}

impl<'a> Capacity<'a> {
    fn new(rules: &'a GlobalRules) -> Self {
        Self {
            rules,
            per_day: HashMap::new(),
            per_week: HashMap::new(),
            per_month: HashMap::new(),
        }
    }

    /// Why `weight` more on `date` breaks a limit, if it does.
    fn check(&self, date: Date, weight: u32) -> Result<(), DateProblem> {
        let used = |n: Option<&u32>| n.copied().unwrap_or(0);
        let r = self.rules;
        let day = used(self.per_day.get(&date));
        if day + weight > r.per_day {
            return Err(DateProblem::OverDay {
                date,
                agreed: day,
                limit: r.per_day,
            });
        }
        let week = used(self.per_week.get(&week_of(date)));
        if week + weight > r.per_week {
            return Err(DateProblem::OverWeek {
                date,
                agreed: week,
                limit: r.per_week,
            });
        }
        let month = used(self.per_month.get(&(date.year(), date.month())));
        if month + weight > r.per_month {
            return Err(DateProblem::OverMonth {
                date,
                agreed: month,
                limit: r.per_month,
            });
        }
        Ok(())
    }

    fn take(&mut self, pair: &PairInput, start: Date) {
        let weight = pair.weight.get();
        for d in pair.span(start) {
            *self.per_day.entry(d).or_default() += weight;
            *self.per_week.entry(week_of(d)).or_default() += weight;
            *self.per_month.entry((d.year(), d.month())).or_default() += weight;
        }
    }
}

/// A valid start, and the last freeze it was pushed past if any.
struct Start {
    date: Date,
    skipped_freeze: Option<String>,
}

/// The rules, freezes and capacity a placement is searched in.
struct Calendar<'a> {
    rules: &'a GlobalRules,
    blackouts: &'a [Blackout],
    cap: Capacity<'a>,
    /// Two years is far past any plausible target; give up after that.
    limit: Date,
}

impl<'a> Calendar<'a> {
    fn new(
        rules: &'a GlobalRules,
        blackouts: &'a [Blackout],
        today: Date,
    ) -> Self {
        Self {
            rules,
            blackouts,
            cap: Capacity::new(rules),
            limit: today.checked_add(2.years()).unwrap_or(today),
        }
    }

    /// Why `pair` cannot use the span starting on `start`, if it cannot.
    fn check(&self, pair: &PairInput, start: Date) -> Result<(), DateProblem> {
        for date in pair.span(start) {
            if !pair.allows(date, self.rules) {
                return Err(DateProblem::ExcludedWeekday { date });
            }
            if let Some(b) =
                self.blackouts.iter().find(|b| b.covers(date, &pair.key))
            {
                return Err(DateProblem::Freeze { date, name: b.name.clone() });
            }
            self.cap.check(date, pair.weight.get())?;
        }
        Ok(())
    }

    /// The first start at or after `from` whose whole span is usable.
    fn next_start(&self, from: Date, pair: &PairInput) -> Option<Start> {
        let mut skipped_freeze = None;
        for date in days(from, self.limit) {
            match self.check(pair, date) {
                Ok(()) => return Some(Start { date, skipped_freeze }),
                Err(DateProblem::Freeze { name, .. }) => {
                    skipped_freeze = Some(name);
                }
                Err(
                    DateProblem::ExcludedWeekday { .. }
                    | DateProblem::OverDay { .. }
                    | DateProblem::OverWeek { .. }
                    | DateProblem::OverMonth { .. },
                ) => {}
            }
        }
        None
    }

    /// Up to `count` valid starts from `from`, skipping `skip`. Offered,
    /// not reserved, so they stay available to other pairs.
    fn alternatives(
        &self,
        from: Date,
        pair: &PairInput,
        skip: Option<Date>,
        count: usize,
    ) -> Vec<Date> {
        let mut out = Vec::new();
        let mut from = from;
        while out.len() < count {
            let Some(start) = self.next_start(from, pair) else { break };
            if Some(start.date) != skip {
                out.push(start.date);
            }
            let Ok(next) = start.date.tomorrow() else { break };
            from = next;
        }
        out
    }
}

/// Everything a schedule is computed from.
pub struct Plan<'a> {
    pub pairs: &'a [PairInput],
    pub rules: &'a GlobalRules,
    pub blackouts: &'a [Blackout],
}

impl Plan<'_> {
    /// What `pair` waits for: its explicit dependencies plus, by rule, the
    /// other environments of its workload when it is production.
    pub(crate) fn dependencies(&self, pair: &PairInput) -> Vec<PairKey> {
        let mut deps = pair.depends_on.clone();
        if self.rules.nonprod_before_prod
            && pair.key.environment.is_production()
        {
            deps.extend(
                self.pairs
                    .iter()
                    .filter(|q| {
                        q.key.workload == pair.key.workload
                            && !q.key.environment.is_production()
                    })
                    .map(|q| q.key.clone()),
            );
        }
        deps.sort();
        deps.dedup();
        deps
    }

    /// The pairs `pair` waits for that have not migrated yet.
    pub fn unfinished_dependencies(&self, pair: &PairInput) -> Vec<PairKey> {
        self.dependencies(pair)
            .into_iter()
            .filter(|d| {
                self.pairs
                    .iter()
                    .any(|p| p.key == *d && p.state != PairState::Migrated)
            })
            .collect()
    }

    /// Check a date before it is agreed: it must follow every rule given
    /// the dates already agreed for the other pairs.
    pub fn check_agreed_date(
        &self,
        pair: &PairInput,
        start: Date,
    ) -> Result<(), DateProblem> {
        let mut cal = Calendar::new(self.rules, self.blackouts, start);
        for q in self.pairs.iter().filter(|q| q.key != pair.key) {
            if let Some(d) = q.state.agreed() {
                cal.cap.take(q, d);
            }
        }
        cal.check(pair, start)
    }

    /// Place every pair. `today` is the first day anything may start.
    pub fn compute(&self, today: Date) -> Schedule {
        let mut out = Schedule::default();
        let mut cal = Calendar::new(self.rules, self.blackouts, today);
        // Last day of each settled pair (agreed, migrated or placed), for
        // dependents and wave ordering.
        let mut settled: HashMap<PairKey, Date> = HashMap::new();

        // An agreed migration whose last day passed without a migration
        // recorded did not happen; the pair goes back into the pool.
        let mut pairs: Vec<PairInput> = self.pairs.to_vec();
        for p in &mut pairs {
            if let Some(d) = p.state.agreed() {
                if p.span_end(d) < today {
                    p.state = PairState::Missed;
                    out.expired.push(p.key.clone());
                }
            }
        }
        let plan = Plan {
            pairs: &pairs,
            rules: self.rules,
            blackouts: self.blackouts,
        };

        // Agreed dates first, earliest first; they consume capacity and
        // never move. One that breaks a rule, because the rules changed
        // after it was agreed, is kept and reported.
        let mut agreed: Vec<(Date, &PairInput)> = pairs
            .iter()
            .filter_map(|p| p.state.agreed().map(|d| (d, p)))
            .collect();
        agreed.sort_by(|a, b| (a.0, &a.1.key).cmp(&(b.0, &b.1.key)));
        for (d, p) in &agreed {
            if let Err(problem) = cal.check(p, *d) {
                out.conflicts.push((p.key.clone(), Conflict::Rule { problem }));
            }
            cal.cap.take(p, *d);
            settled.insert(p.key.clone(), p.span_end(*d));
            out.slots.push(Slot {
                pair: p.key.clone(),
                wave: p.wave,
                date: *d,
                end: p.span_end(*d),
                weight: p.weight.get(),
                agreed: true,
                placement: Placement::Agreed,
                reopened: None,
                alternatives: Vec::new(),
            });
        }
        // Another future date may be agreed in place of an agreed one;
        // offer the next three open starts once every agreed date holds
        // capacity.
        for (slot, (_, p)) in out.slots.iter_mut().zip(&agreed) {
            slot.alternatives = cal.alternatives(today, p, Some(slot.date), 3);
        }
        for p in pairs.iter().filter(|p| p.state == PairState::Migrated) {
            // Already done: dependents may go any time.
            settled.insert(p.key.clone(), today.yesterday().unwrap_or(today));
        }

        let wave_of: HashMap<&PairKey, u32> =
            pairs.iter().map(|p| (&p.key, p.wave)).collect();
        let mut waves: Vec<u32> = pairs.iter().map(|p| p.wave).collect();
        waves.sort_unstable();
        waves.dedup();

        let mut wave_floor = today;
        let mut previous_wave = None;
        for wave in waves {
            let mut open: Vec<&PairInput> = pairs
                .iter()
                .filter(|p| p.wave == wave && p.state.status().is_open())
                .collect();
            open.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

            // Place in rounds: a pair is ready when every dependency has a
            // date. Unknown dependencies are ignored; a cycle leaves its
            // members unplaced.
            while !open.is_empty() {
                let mut progressed = false;
                let mut remaining = Vec::new();
                for p in open {
                    let deps = plan.dependencies(p);
                    if let Some(later) = deps
                        .iter()
                        .find(|d| wave_of.get(d).is_some_and(|w| *w > wave))
                    {
                        out.unplaced.push((
                            p.key.clone(),
                            Unplaced::DependsOnLaterWave { on: later.clone() },
                        ));
                        progressed = true;
                        continue;
                    }
                    if deps.iter().any(|d| {
                        wave_of.contains_key(d) && !settled.contains_key(d)
                    }) {
                        remaining.push(p);
                        continue;
                    }
                    progressed = true;
                    let after_deps = deps
                        .iter()
                        .filter_map(|d| settled.get(d))
                        .max()
                        .and_then(|d| d.tomorrow().ok());
                    let from =
                        [Some(today), Some(wave_floor), p.earliest, after_deps]
                            .into_iter()
                            .flatten()
                            .max()
                            .unwrap_or(today);
                    let Some(start) = cal.next_start(from, p) else {
                        out.unplaced
                            .push((p.key.clone(), Unplaced::NoValidSpan));
                        continue;
                    };
                    let date = start.date;
                    cal.cap.take(p, date);
                    settled.insert(p.key.clone(), p.span_end(date));

                    let alternatives = cal.alternatives(
                        date.tomorrow().unwrap_or(date),
                        p,
                        None,
                        2,
                    );
                    let placement = match (start.skipped_freeze, after_deps) {
                        (Some(name), _) => Placement::AfterFreeze { name },
                        (None, Some(_)) => Placement::AfterDependencies,
                        (None, None) => match previous_wave {
                            Some(prev)
                                if date == wave_floor && wave_floor > today =>
                            {
                                Placement::AfterWave { wave: prev }
                            }
                            Some(_) | None if p.weekdays.is_none() => {
                                Placement::FirstOpenDay
                            }
                            Some(_) | None => Placement::AllowedWeekday,
                        },
                    };
                    let reopened = match p.state {
                        PairState::Missed => Some(Reopened::Missed),
                        PairState::RolledBack => Some(Reopened::RolledBack),
                        PairState::Proposed
                        | PairState::Confirmed(_)
                        | PairState::Migrated => None,
                    };
                    out.slots.push(Slot {
                        pair: p.key.clone(),
                        wave,
                        date,
                        end: p.span_end(date),
                        weight: p.weight.get(),
                        agreed: false,
                        placement,
                        reopened,
                        alternatives,
                    });
                }
                if !progressed {
                    for p in remaining {
                        out.unplaced
                            .push((p.key.clone(), Unplaced::DependencyCycle));
                    }
                    break;
                }
                open = remaining;
            }

            // The next wave starts after everything in this one, agreed and
            // placed alike, has finished.
            let wave_end = pairs
                .iter()
                .filter(|p| p.wave == wave && p.state != PairState::Migrated)
                .filter_map(|p| settled.get(&p.key))
                .max()
                .copied();
            if let Some(end) = wave_end {
                out.wave_ends.insert(wave, end);
                wave_floor = wave_floor.max(end.tomorrow().unwrap_or(end));
                previous_wave = Some(wave);
            }
        }

        // Agreed dates that contradict the plan's order are reported, not
        // moved.
        for (d, p) in &agreed {
            for dep in plan.dependencies(p) {
                if settled.get(&dep).is_some_and(|end| end >= d) {
                    out.conflicts.push((
                        p.key.clone(),
                        Conflict::BeforeDependency { on: dep },
                    ));
                }
            }
            let earlier = out
                .wave_ends
                .range(..p.wave)
                .max_by_key(|(_, end)| **end)
                .map(|(w, end)| (*w, *end));
            if let Some((wave, end)) = earlier {
                if end >= *d {
                    out.conflicts
                        .push((p.key.clone(), Conflict::BeforeWave { wave }));
                }
            }
        }

        out.slots.sort_by(|a, b| (a.date, &a.pair).cmp(&(b.date, &b.pair)));
        out.ends = out.slots.iter().map(|s| s.end).max();
        if let Some(end) = self.rules.target_end {
            out.late = out
                .slots
                .iter()
                .filter(|s| s.end > end)
                .map(|s| s.pair.clone())
                .collect();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::{Weekday, date};

    fn pair(w: &str, e: &str) -> PairInput {
        PairInput::new(PairKey::new(w, e))
    }

    fn agreed(w: &str, e: &str, d: Date) -> PairInput {
        let mut p = pair(w, e);
        p.state = PairState::Confirmed(d);
        p
    }

    fn weekends() -> Option<Weekdays> {
        Some(WeekdayPreset::Weekends.days())
    }

    fn n(v: u32) -> NonZeroU32 {
        NonZeroU32::new(v).unwrap_or(NonZeroU32::MIN)
    }

    fn compute(
        pairs: &[PairInput],
        rules: &GlobalRules,
        blackouts: &[Blackout],
        today: Date,
    ) -> Schedule {
        Plan { pairs, rules, blackouts }.compute(today)
    }

    fn by_env(s: &Schedule) -> HashMap<&str, &Slot> {
        s.slots.iter().map(|x| (x.pair.environment.as_str(), x)).collect()
    }

    fn by_workload(s: &Schedule) -> HashMap<&str, &Slot> {
        s.slots.iter().map(|x| (x.pair.workload.as_str(), x)).collect()
    }

    #[test]
    fn one_per_day_spreads_pairs_across_days() {
        let s = compute(
            &[pair("a", "dev"), pair("b", "dev"), pair("c", "dev")],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        let dates: Vec<Date> = s.slots.iter().map(|s| s.date).collect();
        assert_eq!(
            dates,
            vec![date(2026, 10, 14), date(2026, 10, 15), date(2026, 10, 16)]
        );
    }

    #[test]
    fn prod_waits_for_its_weekend_and_dev_goes_first() {
        let mut prod = pair("shop", "prod");
        prod.weekdays = weekends();
        let s = compute(
            &[prod, pair("shop", "dev")],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        let by = by_env(&s);
        assert_eq!(by["dev"].date, date(2026, 10, 14), "wednesday");
        assert_eq!(by["prod"].date, date(2026, 10, 17), "saturday");
    }

    #[test]
    fn blackouts_and_weekly_capacity_push_dates_out() {
        let rules = GlobalRules {
            per_day: 1,
            per_week: 2,
            per_month: 10,
            weekdays: Weekdays::any(),
            target_end: None,
            nonprod_before_prod: true,
        };
        let freeze = Blackout {
            name: "quarter close".into(),
            start: date(2026, 10, 15),
            end: date(2026, 10, 16),
            pair: None,
        };
        let s = compute(
            &[pair("a", "dev"), pair("b", "dev"), pair("c", "dev")],
            &rules,
            &[freeze],
            date(2026, 10, 14),
        );
        let dates: Vec<Date> = s.slots.iter().map(|s| s.date).collect();
        // Wed 14, then Sat 17 (15 and 16 frozen), then the week's capacity
        // of 2 is used, so Monday 19.
        assert_eq!(
            dates,
            vec![date(2026, 10, 14), date(2026, 10, 17), date(2026, 10, 19)]
        );
        assert_eq!(
            s.slots[1].placement,
            Placement::AfterFreeze { name: "quarter close".into() }
        );
        assert_eq!(s.slots[1].reason(), "after quarter close");
    }

    #[test]
    fn agreed_dates_hold_and_consume_capacity() {
        let s = compute(
            &[agreed("a", "prod", date(2026, 10, 14)), pair("b", "dev")],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(s.slots[0].date, date(2026, 10, 14));
        assert!(s.slots[0].agreed);
        assert_eq!(
            s.slots[0].alternatives,
            vec![date(2026, 10, 15), date(2026, 10, 16), date(2026, 10, 17)],
            "other future days it could be agreed to instead"
        );
        assert_eq!(
            s.slots[1].date,
            date(2026, 10, 15),
            "the day was taken by the agreed date"
        );
    }

    #[test]
    fn proposed_pairs_get_two_alternatives_that_respect_the_rules() {
        let mut prod = pair("shop", "prod");
        prod.weekdays = weekends();
        let s =
            compute(&[prod], &GlobalRules::default(), &[], date(2026, 10, 14));
        assert_eq!(s.slots[0].date, date(2026, 10, 17));
        assert_eq!(
            s.slots[0].alternatives,
            vec![date(2026, 10, 18), date(2026, 10, 24)]
        );
        assert_eq!(s.slots[0].placement, Placement::AllowedWeekday);
    }

    #[test]
    fn missed_pairs_are_rescheduled_and_late_ones_reported() {
        let mut missed = pair("a", "dev");
        missed.state = PairState::Missed;
        let rules = GlobalRules {
            target_end: Some(date(2026, 10, 14)),
            ..Default::default()
        };
        let s = compute(
            &[missed, pair("b", "dev")],
            &rules,
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(s.slots[0].reopened, Some(Reopened::Missed));
        assert!(s.slots[0].reason().starts_with("rescheduled"));
        assert_eq!(s.slots[0].state_label(), "rescheduled");
        assert_eq!(s.late, vec![PairKey::new("b", "dev")]);
    }

    #[test]
    fn an_agreed_date_that_passed_goes_back_into_the_pool() {
        let s = compute(
            &[agreed("a", "dev", date(2026, 10, 10))],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(s.expired, vec![PairKey::new("a", "dev")]);
        assert!(!s.slots[0].agreed);
        assert_eq!(s.slots[0].reopened, Some(Reopened::Missed));
        assert_eq!(s.slots[0].date, date(2026, 10, 14));
    }

    #[test]
    fn prod_follows_its_other_environments_by_rule() {
        let s = compute(
            &[
                pair("shop", "prod"),
                pair("shop", "dev"),
                pair("shop", "staging"),
            ],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        let by = by_env(&s);
        assert!(by["prod"].date > by["dev"].date);
        assert!(by["prod"].date > by["staging"].date);
        assert_eq!(by["prod"].placement, Placement::AfterDependencies);
    }

    #[test]
    fn explicit_dependencies_and_cycles() {
        let mut api = pair("api", "prod");
        api.depends_on = vec![PairKey::new("db", "prod")];
        let mut db = pair("db", "prod");
        db.state = PairState::Migrated;
        let rules =
            GlobalRules { nonprod_before_prod: false, ..Default::default() };
        let s = compute(&[api, db], &rules, &[], date(2026, 10, 14));
        assert_eq!(s.slots.len(), 1, "db is done, api goes today");
        assert_eq!(s.slots[0].date, date(2026, 10, 14));

        let mut x = pair("x", "dev");
        x.depends_on = vec![PairKey::new("y", "dev")];
        let mut y = pair("y", "dev");
        y.depends_on = vec![PairKey::new("x", "dev")];
        let s = compute(&[x, y], &rules, &[], date(2026, 10, 14));
        assert!(s.slots.is_empty());
        assert_eq!(s.unplaced.len(), 2);
        assert_eq!(s.unplaced[0].1, Unplaced::DependencyCycle);
    }

    #[test]
    fn migrated_pairs_are_not_scheduled() {
        let mut done = pair("a", "dev");
        done.state = PairState::Migrated;
        let s =
            compute(&[done], &GlobalRules::default(), &[], date(2026, 10, 14));
        assert!(s.slots.is_empty());
    }

    #[test]
    fn a_span_takes_consecutive_allowed_days_and_weight_fills_capacity() {
        let mut big = pair("erp", "prod");
        big.duration = n(3);
        big.weight = n(2);
        let rules = GlobalRules {
            per_day: 2,
            per_week: 10,
            per_month: 30,
            nonprod_before_prod: false,
            ..Default::default()
        };
        let freeze = Blackout {
            name: "patch night".into(),
            start: date(2026, 10, 16),
            end: date(2026, 10, 16),
            pair: None,
        };
        let s = compute(
            &[big, pair("tiny", "dev")],
            &rules,
            &[freeze],
            date(2026, 10, 14),
        );
        let by = by_workload(&s);
        // 14-16 is broken by the freeze on the 16th, so erp runs 17-19.
        assert_eq!(by["erp"].date, date(2026, 10, 17));
        assert_eq!(by["erp"].end, date(2026, 10, 19));
        assert_eq!(by["erp"].days(), 3);
        assert_eq!(
            by["erp"].placement,
            Placement::AfterFreeze { name: "patch night".into() }
        );
        // tiny goes first on the 14th; erp's weight of 2 fills each of its
        // days completely.
        assert_eq!(by["tiny"].date, date(2026, 10, 14));
        assert_eq!(s.ends, Some(date(2026, 10, 19)));
    }

    #[test]
    fn waves_run_strictly_in_order() {
        let mut second = pair("later", "dev");
        second.wave = 2;
        second.priority = -10;
        let mut done = pair("done", "dev");
        done.state = PairState::Migrated;
        let s = compute(
            &[second, pair("first", "dev"), done],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        let by = by_workload(&s);
        assert_eq!(by["first"].date, date(2026, 10, 14));
        assert_eq!(
            by["later"].date,
            date(2026, 10, 15),
            "priority does not beat the wave order"
        );
        assert_eq!(by["later"].placement, Placement::AfterWave { wave: 1 });
        assert_eq!(s.wave_ends[&1], date(2026, 10, 14));
        assert_eq!(s.wave_ends[&2], date(2026, 10, 15));
    }

    #[test]
    fn wave_messages_name_the_wave_that_actually_came_before() {
        let mut fifth = pair("later", "dev");
        fifth.wave = 5;
        let mut second = pair("first", "dev");
        second.wave = 2;
        let s = compute(
            &[fifth, second],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(
            by_workload(&s)["later"].placement,
            Placement::AfterWave { wave: 2 },
            "not wave 4, which has nothing in it"
        );
    }

    #[test]
    fn an_agreed_date_inside_an_earlier_wave_is_a_conflict_not_a_move() {
        let mut jumped = agreed("jumped", "dev", date(2026, 10, 14));
        jumped.wave = 2;
        let s = compute(
            &[jumped, pair("first", "dev")],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert!(s.slots[0].agreed);
        assert_eq!(s.slots[0].date, date(2026, 10, 14));
        assert_eq!(
            s.conflicts,
            vec![(
                PairKey::new("jumped", "dev"),
                Conflict::BeforeWave { wave: 1 }
            )]
        );
        assert_eq!(s.conflicts[0].1.to_string(), "starts before wave 1 ends");
    }

    #[test]
    fn a_dependency_in_a_later_wave_cannot_be_placed() {
        let mut early = pair("early", "dev");
        early.depends_on = vec![PairKey::new("late", "dev")];
        let mut late = pair("late", "dev");
        late.wave = 2;
        let s = compute(
            &[early, late],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(
            s.unplaced,
            vec![(
                PairKey::new("early", "dev"),
                Unplaced::DependsOnLaterWave {
                    on: PairKey::new("late", "dev")
                }
            )]
        );
    }

    #[test]
    fn rolled_back_pairs_are_scheduled_again() {
        let mut back = pair("a", "dev");
        back.state = PairState::RolledBack;
        let s =
            compute(&[back], &GlobalRules::default(), &[], date(2026, 10, 14));
        assert_eq!(s.slots.len(), 1);
        assert!(s.slots[0].reason().starts_with("rolled back"));
    }

    #[test]
    fn an_agreed_span_expires_only_after_its_last_day() {
        let mut running = agreed("a", "dev", date(2026, 10, 12));
        running.duration = n(3);
        let s = compute(
            &[running],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert!(s.expired.is_empty(), "still inside its 12-14 span");
        assert!(s.slots[0].agreed);
    }

    #[test]
    fn agreed_dates_must_follow_the_rules() {
        let a = agreed("a", "dev", date(2026, 10, 20));
        let b = pair("b", "dev");
        let rules = GlobalRules {
            weekdays: Weekdays::only([Weekday::Tuesday, Weekday::Wednesday]),
            ..GlobalRules::default()
        };
        let freeze = Blackout {
            name: "close".into(),
            start: date(2026, 10, 27),
            end: date(2026, 10, 27),
            pair: None,
        };
        let pairs = [a.clone(), b.clone()];
        let blackouts = [freeze];
        let plan = Plan { pairs: &pairs, rules: &rules, blackouts: &blackouts };
        assert_eq!(
            plan.check_agreed_date(&b, date(2026, 10, 20)),
            Err(DateProblem::OverDay {
                date: date(2026, 10, 20),
                agreed: 1,
                limit: 1
            })
        );
        let thursday = plan.check_agreed_date(&b, date(2026, 10, 22));
        assert!(thursday.unwrap_err().to_string().contains("Thursday"));
        let frozen = plan.check_agreed_date(&b, date(2026, 10, 27));
        assert!(frozen.unwrap_err().to_string().contains("close freeze"));
        assert_eq!(plan.check_agreed_date(&b, date(2026, 10, 21)), Ok(()));
        // Agreeing a pair's own date again does not count against itself.
        assert_eq!(plan.check_agreed_date(&a, date(2026, 10, 20)), Ok(()));
    }

    #[test]
    fn agreed_dates_that_break_a_later_rule_are_conflicts() {
        let s = compute(
            &[
                agreed("a", "dev", date(2026, 10, 20)),
                agreed("b", "dev", date(2026, 10, 20)),
            ],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(s.slots.len(), 2, "both kept");
        assert_eq!(s.conflicts.len(), 1);
        assert_eq!(s.conflicts[0].0, PairKey::new("b", "dev"));
        assert!(s.conflicts[0].1.to_string().contains("limit is 1 per day"));
    }

    #[test]
    fn unfinished_dependencies_skip_what_has_migrated() {
        let mut dev = pair("shop", "dev");
        dev.state = PairState::Migrated;
        let pairs = [pair("shop", "prod"), pair("shop", "staging"), dev];
        let rules = GlobalRules::default();
        let plan = Plan { pairs: &pairs, rules: &rules, blackouts: &[] };
        assert_eq!(
            plan.unfinished_dependencies(&pairs[0]),
            vec![PairKey::new("shop", "staging")]
        );
    }

    #[test]
    fn days_are_inclusive() {
        let d: Vec<Date> =
            days(date(2026, 10, 30), date(2026, 11, 1)).collect();
        assert_eq!(
            d,
            vec![date(2026, 10, 30), date(2026, 10, 31), date(2026, 11, 1)]
        );
    }
}
