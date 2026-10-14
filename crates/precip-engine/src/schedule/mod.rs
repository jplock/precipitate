//! Migration scheduling. Pure over the pairs to migrate, the rules and
//! the calendar, so it is testable and can be recomputed every day.
//!
//! Confirmed dates are fixed points: they are placed first, consume
//! capacity, and never move. Everything else is placed wave by wave, in
//! priority order, into the first span of days that satisfies the pair's
//! weekday rule, the blackouts, and the weighted per-day, per-week and
//! per-month capacity, and is offered two further starts that would also
//! work so the owners can pick one and confirm.
//!
//! A pair occupies `duration_days` consecutive calendar days, every one of
//! which must be allowed, and takes `weight` units of capacity on each.
//! Waves are strict: nothing in wave 2 starts before the last day of
//! wave 1.

pub mod gantt;
pub mod holidays;

use jiff::civil::{Date, Weekday};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalRules {
    /// Capacity in weight units. A pair of weight 1 and duration 1 is one
    /// unit on one day.
    pub per_day: u32,
    pub per_week: u32,
    pub per_month: u32,
    /// Weekdays migrations may happen on when a pair has no rule of its
    /// own. Empty means every day.
    pub weekdays: Vec<Weekday>,
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
            weekdays: Vec::new(),
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
    pub pair: Option<(String, String)>,
}

precip_types::named_enum! {
    pub enum Status {
        /// Proposed by the scheduler; moves when the schedule is recomputed.
        Proposed = "proposed",
        /// Date confirmed with the owners; the scheduler never moves it.
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
    pub fn is_open(self) -> bool {
        match self {
            Status::Proposed | Status::Missed | Status::RolledBack => true,
            Status::Confirmed | Status::Migrated => false,
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
    pub workload: String,
    pub environment: String,
    /// Waves are placed in order; nothing in a later wave starts before
    /// the earlier wave's last day.
    pub wave: u32,
    /// Lower runs first. Ties break on environment (non-production first)
    /// then name.
    pub priority: i32,
    /// Consecutive calendar days the migration occupies.
    pub duration_days: u32,
    /// Capacity units taken on each of those days.
    pub weight: u32,
    /// Weekdays this pair may migrate on, overriding the global rule.
    pub weekdays: Vec<Weekday>,
    pub earliest: Option<Date>,
    pub status: Status,
    /// The confirmed start date when status is `Confirmed`.
    pub locked: Option<Date>,
    /// Pairs that must migrate before this one.
    pub depends_on: Vec<(String, String)>,
}

impl PairInput {
    fn span_end(&self, start: Date) -> Date {
        start
            .checked_add(
                jiff::Span::new()
                    .days(i64::from(self.duration_days.max(1)) - 1),
            )
            .unwrap_or(start)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Slot {
    pub workload: String,
    pub environment: String,
    pub wave: u32,
    /// First day of the migration.
    pub date: Date,
    /// Last day of the migration; the same as `date` for one-day moves.
    pub end: Date,
    pub weight: u32,
    pub locked: bool,
    /// Why this date: the first rule it satisfied or the lock.
    pub reason: String,
    /// Further start dates that would also work, offered so the owners
    /// can pick one and confirm.
    pub alternatives: Vec<Date>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Schedule {
    pub slots: Vec<Slot>,
    /// Pairs scheduled after the target end date.
    pub late: Vec<(String, String)>,
    /// Last scheduled day, when anything was scheduled.
    pub ends: Option<Date>,
    /// Last scheduled day of each wave that has anything scheduled.
    pub wave_ends: BTreeMap<u32, Date>,
    /// Confirmed dates that passed without the pair being marked migrated.
    /// They were scheduled again as missed.
    pub expired: Vec<(String, String)>,
    /// Pairs that could not be placed, with why (a dependency cycle, a
    /// dependency in a later wave, or nothing valid within two years).
    pub unplaced: Vec<((String, String), String)>,
    /// Confirmed dates that break the plan: over a capacity limit, on a
    /// day the rules exclude, inside a freeze, before an earlier wave ends,
    /// or before a dependency. Reported, never moved.
    pub conflicts: Vec<((String, String), String)>,
}

struct Capacity<'a> {
    rules: &'a GlobalRules,
    per_day: HashMap<Date, u32>,
    per_week: HashMap<(i16, i8), u32>,
    per_month: HashMap<(i16, i8), u32>,
}

/// Week key: the year and week number of the Monday starting the week.
fn week_key(d: Date) -> (i16, i8) {
    let back = i64::from(d.weekday().to_monday_zero_offset());
    let monday = d.checked_sub(jiff::Span::new().days(back)).unwrap_or(d);
    (monday.year(), i8::try_from(monday.day_of_year() / 7).unwrap_or(0))
}

impl Capacity<'_> {
    fn has_room(&self, d: Date, weight: u32) -> bool {
        let used = |n: Option<&u32>| n.copied().unwrap_or(0);
        used(self.per_day.get(&d)) + weight <= self.rules.per_day
            && used(self.per_week.get(&week_key(d))) + weight
                <= self.rules.per_week
            && used(self.per_month.get(&(d.year(), d.month()))) + weight
                <= self.rules.per_month
    }

    fn take(&mut self, d: Date, weight: u32) {
        *self.per_day.entry(d).or_default() += weight;
        *self.per_week.entry(week_key(d)).or_default() += weight;
        *self.per_month.entry((d.year(), d.month())).or_default() += weight;
    }

    fn take_span(&mut self, pair: &PairInput, start: Date) {
        let mut d = start;
        let end = pair.span_end(start);
        while d <= end {
            self.take(d, pair.weight.max(1));
            let Ok(next) = d.tomorrow() else { break };
            d = next;
        }
    }
}

impl Capacity<'_> {
    /// Why `weight` more on day `d` breaks a limit, if it does.
    fn over_limit(&self, d: Date, weight: u32) -> Option<String> {
        let used = |n: Option<&u32>| n.copied().unwrap_or(0);
        let r = self.rules;
        let day = used(self.per_day.get(&d));
        if day + weight > r.per_day {
            return Some(format!(
                "{d} already has {day} agreed; the limit is {} per day",
                r.per_day
            ));
        }
        let week = used(self.per_week.get(&week_key(d)));
        if week + weight > r.per_week {
            return Some(format!(
                "the week of {d} already has {week} agreed; the limit is {} per week",
                r.per_week
            ));
        }
        let month = used(self.per_month.get(&(d.year(), d.month())));
        if month + weight > r.per_month {
            return Some(format!(
                "{} already has {month} agreed; the limit is {} per month",
                d.strftime("%B %Y"),
                r.per_month
            ));
        }
        None
    }
}

/// Why an agreed date for `pair` starting on `start` breaks the rules,
/// given the capacity other agreed dates already hold: a weekday the
/// rules exclude, a freeze, or a limit. `None` when it is allowed.
fn agreed_date_problem(
    start: Date,
    pair: &PairInput,
    rules: &GlobalRules,
    blackouts: &[Blackout],
    cap: &Capacity<'_>,
) -> Option<String> {
    let mut d = start;
    let end = pair.span_end(start);
    while d <= end {
        if !weekday_allowed(d, pair, rules) {
            return Some(format!(
                "{d} is a {}, which the rules exclude",
                d.strftime("%A")
            ));
        }
        if let Some(name) = blacked_out(d, pair, blackouts) {
            return Some(format!("{d} is inside the {name} freeze"));
        }
        if let Some(why) = cap.over_limit(d, pair.weight.max(1)) {
            return Some(why);
        }
        let Ok(next) = d.tomorrow() else { break };
        d = next;
    }
    None
}

/// Check a date before it is agreed: it must follow every rule given the
/// dates already agreed for the other pairs. Returns the reason when not.
pub fn check_agreed_date(
    pair: &PairInput,
    start: Date,
    pairs: &[PairInput],
    rules: &GlobalRules,
    blackouts: &[Blackout],
) -> Result<(), String> {
    let mut cap = Capacity {
        rules,
        per_day: HashMap::new(),
        per_week: HashMap::new(),
        per_month: HashMap::new(),
    };
    for q in pairs.iter().filter(|q| {
        q.status == Status::Confirmed
            && (q.workload != pair.workload
                || q.environment != pair.environment)
    }) {
        if let Some(d) = q.locked {
            cap.take_span(q, d);
        }
    }
    match agreed_date_problem(start, pair, rules, blackouts, &cap) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

fn blacked_out<'a>(
    d: Date,
    pair: &PairInput,
    blackouts: &'a [Blackout],
) -> Option<&'a str> {
    blackouts
        .iter()
        .find(|b| {
            d >= b.start
                && d <= b.end
                && b.pair.as_ref().is_none_or(|(w, e)| {
                    *w == pair.workload && *e == pair.environment
                })
        })
        .map(|b| b.name.as_str())
}

fn weekday_allowed(d: Date, pair: &PairInput, rules: &GlobalRules) -> bool {
    let allowed =
        if pair.weekdays.is_empty() { &rules.weekdays } else { &pair.weekdays };
    allowed.is_empty() || allowed.contains(&d.weekday())
}

/// Whether every day of the pair's span starting at `start` is allowed,
/// outside blackouts and has room. Returns the blocking blackout, if one.
fn span_fits<'a>(
    start: Date,
    pair: &PairInput,
    rules: &GlobalRules,
    blackouts: &'a [Blackout],
    cap: &Capacity<'_>,
) -> Result<(), Option<&'a str>> {
    let mut d = start;
    let end = pair.span_end(start);
    while d <= end {
        if !weekday_allowed(d, pair, rules) {
            return Err(None);
        }
        if let Some(name) = blacked_out(d, pair, blackouts) {
            return Err(Some(name));
        }
        if !cap.has_room(d, pair.weight.max(1)) {
            return Err(None);
        }
        let Ok(next) = d.tomorrow() else { return Err(None) };
        d = next;
    }
    Ok(())
}

/// The first start at or after `from` whose whole span the pair may use,
/// with the name of the last blackout skipped on the way.
fn next_valid<'a>(
    from: Date,
    limit: Date,
    pair: &PairInput,
    rules: &GlobalRules,
    blackouts: &'a [Blackout],
    cap: &Capacity<'_>,
) -> Option<(Date, Option<&'a str>)> {
    let mut d = from;
    let mut skipped = None;
    while d <= limit {
        match span_fits(d, pair, rules, blackouts, cap) {
            Ok(()) => return Some((d, skipped)),
            Err(Some(name)) => skipped = Some(name),
            Err(None) => {}
        }
        d = d.tomorrow().ok()?;
    }
    None
}

/// The calendar a placement is searched in: rules, blackouts, the
/// capacity taken so far, and how far ahead to look.
struct Calendar<'a> {
    rules: &'a GlobalRules,
    blackouts: &'a [Blackout],
    limit: Date,
}

impl Calendar<'_> {
    /// Up to `count` valid starts from `from`, skipping `skip`. Offered,
    /// not reserved, so they stay available to other pairs.
    fn alternatives(
        &self,
        from: Date,
        pair: &PairInput,
        cap: &Capacity<'_>,
        skip: Option<Date>,
        count: usize,
    ) -> Vec<Date> {
        let mut out = Vec::new();
        let mut from = from;
        while out.len() < count {
            let Some((alt, _)) = next_valid(
                from,
                self.limit,
                pair,
                self.rules,
                self.blackouts,
                cap,
            ) else {
                break;
            };
            if Some(alt) != skip {
                out.push(alt);
            }
            let Ok(next) = alt.tomorrow() else { break };
            from = next;
        }
        out
    }
}

fn key(p: &PairInput) -> (String, String) {
    (p.workload.clone(), p.environment.clone())
}

/// Place every pair. `today` is the first day anything may be scheduled.
pub fn compute(
    pairs: &[PairInput],
    rules: &GlobalRules,
    blackouts: &[Blackout],
    today: Date,
) -> Schedule {
    let mut cap = Capacity {
        rules,
        per_day: HashMap::new(),
        per_week: HashMap::new(),
        per_month: HashMap::new(),
    };
    let mut out = Schedule::default();
    // Last day of each settled pair (confirmed, migrated or placed), for
    // dependents and wave ordering.
    let mut settled: HashMap<(String, String), Date> = HashMap::new();

    // A confirmed migration whose last day passed without a migration
    // recorded did not happen; the pair goes back into the pool.
    let mut effective: Vec<PairInput> = pairs.to_vec();
    for p in effective.iter_mut() {
        if p.status == Status::Confirmed
            && p.locked.is_some_and(|d| p.span_end(d) < today)
        {
            p.status = Status::Missed;
            p.locked = None;
            out.expired.push(key(p));
        }
    }

    // Two years is far past any plausible target; give up after that.
    let limit = today.checked_add(jiff::Span::new().years(2)).unwrap_or(today);
    let cal = Calendar { rules, blackouts, limit };

    // Confirmed dates first; they consume capacity and never move. One
    // that breaks a rule, because the rules changed after it was agreed,
    // is kept and reported.
    let mut locks: Vec<&PairInput> = effective
        .iter()
        .filter(|p| p.status == Status::Confirmed && p.locked.is_some())
        .collect();
    locks.sort_by(|a, b| {
        (a.locked, &a.workload, &a.environment).cmp(&(
            b.locked,
            &b.workload,
            &b.environment,
        ))
    });
    for p in locks {
        let Some(d) = p.locked else { continue };
        if let Some(why) = agreed_date_problem(d, p, rules, blackouts, &cap) {
            out.conflicts.push((key(p), why));
        }
        cap.take_span(p, d);
        settled.insert(key(p), p.span_end(d));
        out.slots.push(Slot {
            workload: p.workload.clone(),
            environment: p.environment.clone(),
            wave: p.wave,
            date: d,
            end: p.span_end(d),
            weight: p.weight.max(1),
            locked: true,
            reason: "confirmed".to_string(),
            alternatives: Vec::new(),
        });
    }
    // Another future date may be confirmed in place of a confirmed one;
    // offer the next three open starts once every lock holds capacity.
    for s in out.slots.iter_mut() {
        let Some(p) = effective.iter().find(|p| {
            p.workload == s.workload && p.environment == s.environment
        }) else {
            continue;
        };
        s.alternatives = cal.alternatives(today, p, &cap, Some(s.date), 3);
    }
    for p in effective.iter().filter(|p| p.status == Status::Migrated) {
        // Already done: dependents may go any time.
        settled.insert(key(p), today.yesterday().unwrap_or(today));
    }

    let deps_of = |p: &PairInput| dependencies_of(p, &effective, rules);
    let wave_of: HashMap<(String, String), u32> =
        effective.iter().map(|p| (key(p), p.wave)).collect();

    let mut waves: Vec<u32> = effective.iter().map(|p| p.wave).collect();
    waves.sort_unstable();
    waves.dedup();

    let mut wave_floor = today;
    for wave in waves {
        let mut open: Vec<&PairInput> = effective
            .iter()
            .filter(|p| p.wave == wave && p.status.is_open())
            .collect();
        open.sort_by(|a, b| {
            let prod = |e: &str| e == "prod";
            (a.priority, prod(&a.environment), &a.workload, &a.environment).cmp(
                &(
                    b.priority,
                    prod(&b.environment),
                    &b.workload,
                    &b.environment,
                ),
            )
        });

        // Place in rounds: a pair is ready when every dependency has a
        // date. Unknown dependencies are ignored; a cycle leaves its
        // members unplaced.
        while !open.is_empty() {
            let mut progressed = false;
            let mut remaining = Vec::new();
            for p in open {
                let deps = deps_of(p);
                if let Some(later) = deps
                    .iter()
                    .find(|d| wave_of.get(*d).is_some_and(|w| *w > wave))
                {
                    out.unplaced.push((
                        key(p),
                        format!(
                            "depends on {} / {} in a later wave",
                            later.0, later.1
                        ),
                    ));
                    progressed = true;
                    continue;
                }
                let pending = deps.iter().any(|d| {
                    wave_of.contains_key(d) && !settled.contains_key(d)
                });
                if pending {
                    remaining.push(p);
                    continue;
                }
                let after_deps = deps
                    .iter()
                    .filter_map(|d| settled.get(d))
                    .max()
                    .and_then(|d| d.tomorrow().ok());
                let start =
                    [Some(today), Some(wave_floor), p.earliest, after_deps]
                        .into_iter()
                        .flatten()
                        .max()
                        .unwrap_or(today);
                let Some((date, skipped)) =
                    next_valid(start, limit, p, rules, blackouts, &cap)
                else {
                    out.unplaced.push((
                        key(p),
                        "no valid span within two years".to_string(),
                    ));
                    progressed = true;
                    continue;
                };
                cap.take_span(p, date);
                settled.insert(key(p), p.span_end(date));
                progressed = true;

                // Alternatives are the next valid starts with room,
                // offered but not reserved, so they stay available to
                // other pairs.
                let alts = cal.alternatives(
                    date.tomorrow().unwrap_or(date),
                    p,
                    &cap,
                    None,
                    2,
                );
                let mut reason = match (skipped, after_deps) {
                    (Some(name), _) => format!("after {name}"),
                    (None, Some(_)) => "after its dependencies".to_string(),
                    (None, None)
                        if wave_floor > today && date == wave_floor =>
                    {
                        format!("after wave {}", wave - 1)
                    }
                    (None, None) if p.weekdays.is_empty() => {
                        "first open day".to_string()
                    }
                    (None, None) => "allowed weekday".to_string(),
                };
                match p.status {
                    Status::Missed => reason = format!("rescheduled, {reason}"),
                    Status::RolledBack => {
                        reason = format!("rolled back, {reason}");
                    }
                    Status::Proposed | Status::Confirmed | Status::Migrated => {
                    }
                }
                out.slots.push(Slot {
                    workload: p.workload.clone(),
                    environment: p.environment.clone(),
                    wave,
                    date,
                    end: p.span_end(date),
                    weight: p.weight.max(1),
                    locked: false,
                    reason,
                    alternatives: alts,
                });
            }
            if !progressed {
                for p in remaining {
                    out.unplaced.push((key(p), "dependency cycle".to_string()));
                }
                break;
            }
            open = remaining;
        }

        // The next wave starts after everything in this one, confirmed and
        // placed alike, has finished.
        let wave_end = effective
            .iter()
            .filter(|p| p.wave == wave && p.status != Status::Migrated)
            .filter_map(|p| settled.get(&key(p)))
            .max()
            .copied();
        if let Some(end) = wave_end {
            out.wave_ends.insert(wave, end);
            wave_floor = wave_floor.max(end.tomorrow().unwrap_or(end));
        }
    }

    // Confirmed dates that contradict the plan are reported, not moved.
    for p in effective.iter().filter(|p| p.status == Status::Confirmed) {
        let Some(d) = p.locked else { continue };
        for dep in deps_of(p) {
            if let Some(end) = settled.get(&dep) {
                if *end >= d {
                    out.conflicts.push((
                        key(p),
                        format!("starts before {} / {} ends", dep.0, dep.1),
                    ));
                }
            }
        }
        let earlier_wave_end =
            out.wave_ends.range(..p.wave).map(|(_, end)| *end).max();
        if earlier_wave_end.is_some_and(|end| end >= d) {
            out.conflicts.push((
                key(p),
                format!("starts before wave {} ends", p.wave - 1),
            ));
        }
    }

    out.slots.sort_by(|a, b| {
        (a.date, &a.workload, &a.environment).cmp(&(
            b.date,
            &b.workload,
            &b.environment,
        ))
    });
    out.ends = out.slots.iter().map(|s| s.end).max();
    if let Some(end) = rules.target_end {
        out.late = out
            .slots
            .iter()
            .filter(|s| s.end > end)
            .map(|s| (s.workload.clone(), s.environment.clone()))
            .collect();
    }
    out
}

/// What a pair waits for: its explicit dependencies plus, by rule, the
/// other environments of its workload when it is production.
pub fn dependencies_of(
    p: &PairInput,
    pairs: &[PairInput],
    rules: &GlobalRules,
) -> Vec<(String, String)> {
    let mut deps = p.depends_on.clone();
    if rules.nonprod_before_prod && p.environment == "prod" {
        deps.extend(
            pairs
                .iter()
                .filter(|q| q.workload == p.workload && q.environment != "prod")
                .map(key),
        );
    }
    deps.sort();
    deps.dedup();
    deps
}

/// Slots grouped by start date, for a calendar view.
pub fn by_date(schedule: &Schedule) -> BTreeMap<Date, Vec<&Slot>> {
    let mut map: BTreeMap<Date, Vec<&Slot>> = BTreeMap::new();
    for s in &schedule.slots {
        map.entry(s.date).or_default().push(s);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn pair(w: &str, e: &str) -> PairInput {
        PairInput {
            workload: w.into(),
            environment: e.into(),
            wave: 1,
            priority: 0,
            duration_days: 1,
            weight: 1,
            weekdays: Vec::new(),
            earliest: None,
            status: Status::Proposed,
            locked: None,
            depends_on: Vec::new(),
        }
    }

    fn weekends() -> Vec<Weekday> {
        vec![Weekday::Saturday, Weekday::Sunday]
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
        let by: HashMap<&str, Date> =
            s.slots.iter().map(|x| (x.environment.as_str(), x.date)).collect();
        assert_eq!(by["dev"], date(2026, 10, 14), "wednesday");
        assert_eq!(by["prod"], date(2026, 10, 17), "saturday");
    }

    #[test]
    fn blackouts_and_weekly_capacity_push_dates_out() {
        let rules = GlobalRules {
            per_day: 1,
            per_week: 2,
            per_month: 10,
            weekdays: Vec::new(),
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
        assert!(s.slots[1].reason.contains("quarter close"));
    }

    #[test]
    fn confirmed_dates_hold_and_consume_capacity() {
        let mut locked = pair("a", "prod");
        locked.status = Status::Confirmed;
        locked.locked = Some(date(2026, 10, 14));
        let s = compute(
            &[locked, pair("b", "dev")],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(s.slots[0].date, date(2026, 10, 14));
        assert!(s.slots[0].locked);
        assert_eq!(
            s.slots[0].alternatives,
            vec![date(2026, 10, 15), date(2026, 10, 16), date(2026, 10, 17)],
            "other future days it could be re-confirmed to"
        );
        assert_eq!(
            s.slots[1].date,
            date(2026, 10, 15),
            "the day was taken by the lock"
        );
    }

    #[test]
    fn unconfirmed_pairs_get_two_alternatives_that_respect_the_rules() {
        let mut prod = pair("shop", "prod");
        prod.weekdays = weekends();
        let s =
            compute(&[prod], &GlobalRules::default(), &[], date(2026, 10, 14));
        assert_eq!(s.slots[0].date, date(2026, 10, 17));
        assert_eq!(
            s.slots[0].alternatives,
            vec![date(2026, 10, 18), date(2026, 10, 24)]
        );
    }

    #[test]
    fn missed_pairs_are_rescheduled_and_late_ones_reported() {
        let mut missed = pair("a", "dev");
        missed.status = Status::Missed;
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
        assert!(s.slots[0].reason.starts_with("rescheduled"));
        assert_eq!(s.late, vec![("b".to_string(), "dev".to_string())]);
    }

    #[test]
    fn a_confirmed_date_that_passed_goes_back_into_the_pool() {
        let mut stale = pair("a", "dev");
        stale.status = Status::Confirmed;
        stale.locked = Some(date(2026, 10, 10));
        let s =
            compute(&[stale], &GlobalRules::default(), &[], date(2026, 10, 14));
        assert_eq!(s.expired, vec![("a".to_string(), "dev".to_string())]);
        assert!(!s.slots[0].locked);
        assert!(s.slots[0].reason.starts_with("rescheduled"));
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
        let by: HashMap<&str, &Slot> =
            s.slots.iter().map(|x| (x.environment.as_str(), x)).collect();
        assert!(by["prod"].date > by["dev"].date);
        assert!(by["prod"].date > by["staging"].date);
        assert_eq!(by["prod"].reason, "after its dependencies");
    }

    #[test]
    fn explicit_dependencies_and_cycles() {
        let mut api = pair("api", "prod");
        api.depends_on = vec![("db".into(), "prod".into())];
        let mut db = pair("db", "prod");
        db.status = Status::Migrated;
        let rules =
            GlobalRules { nonprod_before_prod: false, ..Default::default() };
        let s = compute(&[api, db], &rules, &[], date(2026, 10, 14));
        assert_eq!(s.slots.len(), 1, "db is done, api goes today");
        assert_eq!(s.slots[0].date, date(2026, 10, 14));

        let mut x = pair("x", "dev");
        x.depends_on = vec![("y".into(), "dev".into())];
        let mut y = pair("y", "dev");
        y.depends_on = vec![("x".into(), "dev".into())];
        let s = compute(&[x, y], &rules, &[], date(2026, 10, 14));
        assert!(s.slots.is_empty());
        assert_eq!(s.unplaced.len(), 2);
        assert_eq!(s.unplaced[0].1, "dependency cycle");
    }

    #[test]
    fn migrated_pairs_are_not_scheduled() {
        let mut done = pair("a", "dev");
        done.status = Status::Migrated;
        let s =
            compute(&[done], &GlobalRules::default(), &[], date(2026, 10, 14));
        assert!(s.slots.is_empty());
    }

    #[test]
    fn a_span_takes_consecutive_allowed_days_and_weight_fills_capacity() {
        let mut big = pair("erp", "prod");
        big.duration_days = 3;
        big.weight = 2;
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
        let by: HashMap<&str, &Slot> =
            s.slots.iter().map(|x| (x.workload.as_str(), x)).collect();
        // 14-16 is broken by the freeze on the 16th, so erp runs 17-19.
        assert_eq!(by["erp"].date, date(2026, 10, 17));
        assert_eq!(by["erp"].end, date(2026, 10, 19));
        assert!(by["erp"].reason.contains("patch night"));
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
        done.wave = 1;
        done.status = Status::Migrated;
        let s = compute(
            &[second, pair("first", "dev"), done],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        let by: HashMap<&str, &Slot> =
            s.slots.iter().map(|x| (x.workload.as_str(), x)).collect();
        assert_eq!(by["first"].date, date(2026, 10, 14));
        assert_eq!(
            by["later"].date,
            date(2026, 10, 15),
            "priority does not beat the wave order"
        );
        assert_eq!(by["later"].reason, "after wave 1");
        assert_eq!(s.wave_ends[&1], date(2026, 10, 14));
        assert_eq!(s.wave_ends[&2], date(2026, 10, 15));
    }

    #[test]
    fn a_confirmed_date_inside_an_earlier_wave_is_a_conflict_not_a_move() {
        let mut jumped = pair("jumped", "dev");
        jumped.wave = 2;
        jumped.status = Status::Confirmed;
        jumped.locked = Some(date(2026, 10, 14));
        let s = compute(
            &[jumped, pair("first", "dev")],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert!(s.slots[0].locked);
        assert_eq!(s.slots[0].date, date(2026, 10, 14));
        assert_eq!(s.conflicts.len(), 1);
        assert!(s.conflicts[0].1.contains("wave 1"));
    }

    #[test]
    fn a_dependency_in_a_later_wave_cannot_be_placed() {
        let mut early = pair("early", "dev");
        early.depends_on = vec![("late".into(), "dev".into())];
        let mut late = pair("late", "dev");
        late.wave = 2;
        let s = compute(
            &[early, late],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(s.unplaced.len(), 1);
        assert!(s.unplaced[0].1.contains("later wave"));
    }

    #[test]
    fn rolled_back_pairs_are_scheduled_again() {
        let mut back = pair("a", "dev");
        back.status = Status::RolledBack;
        let s =
            compute(&[back], &GlobalRules::default(), &[], date(2026, 10, 14));
        assert_eq!(s.slots.len(), 1);
        assert!(s.slots[0].reason.starts_with("rolled back"));
    }

    #[test]
    fn a_confirmed_span_expires_only_after_its_last_day() {
        let mut running = pair("a", "dev");
        running.status = Status::Confirmed;
        running.locked = Some(date(2026, 10, 12));
        running.duration_days = 3;
        let s = compute(
            &[running],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert!(s.expired.is_empty(), "still inside its 12-14 span");
        assert!(s.slots[0].locked);
    }

    #[test]
    fn agreed_dates_must_follow_the_rules() {
        let mut a = pair("a", "dev");
        a.status = Status::Confirmed;
        a.locked = Some(date(2026, 10, 20));
        let b = pair("b", "dev");
        let rules = GlobalRules {
            weekdays: vec![Weekday::Tuesday, Weekday::Wednesday],
            ..GlobalRules::default()
        };
        let freeze = Blackout {
            name: "close".into(),
            start: date(2026, 10, 27),
            end: date(2026, 10, 27),
            pair: None,
        };
        let pairs = [a.clone(), b.clone()];
        let check = |d| {
            check_agreed_date(
                &b,
                d,
                &pairs,
                &rules,
                std::slice::from_ref(&freeze),
            )
        };
        assert!(
            check(date(2026, 10, 20))
                .unwrap_err()
                .contains("limit is 1 per day")
        );
        assert!(check(date(2026, 10, 22)).unwrap_err().contains("Thursday"));
        assert!(
            check(date(2026, 10, 27)).unwrap_err().contains("close freeze")
        );
        assert!(check(date(2026, 10, 21)).is_ok());
        // Re-agreeing a pair's own date does not count against itself.
        assert!(
            check_agreed_date(&a, date(2026, 10, 20), &pairs, &rules, &[])
                .is_ok()
        );
    }

    #[test]
    fn agreed_dates_that_break_a_later_rule_are_conflicts() {
        let lock = |w: &str| {
            let mut p = pair(w, "dev");
            p.status = Status::Confirmed;
            p.locked = Some(date(2026, 10, 20));
            p
        };
        let s = compute(
            &[lock("a"), lock("b")],
            &GlobalRules::default(),
            &[],
            date(2026, 10, 14),
        );
        assert_eq!(s.slots.len(), 2, "both kept");
        assert_eq!(s.conflicts.len(), 1);
        assert_eq!(s.conflicts[0].0, ("b".to_string(), "dev".to_string()));
        assert!(s.conflicts[0].1.contains("limit is 1 per day"));
    }
}
