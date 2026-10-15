//! The days of the week migrations may run on.

use jiff::civil::Weekday;
use std::fmt;
use std::str::FromStr;

const ALL: [Weekday; 7] = [
    Weekday::Monday,
    Weekday::Tuesday,
    Weekday::Wednesday,
    Weekday::Thursday,
    Weekday::Friday,
    Weekday::Saturday,
    Weekday::Sunday,
];
const WORKDAYS: [Weekday; 5] = [
    Weekday::Monday,
    Weekday::Tuesday,
    Weekday::Wednesday,
    Weekday::Thursday,
    Weekday::Friday,
];
const WEEKENDS: [Weekday; 2] = [Weekday::Saturday, Weekday::Sunday];

/// A set of weekdays, Monday first. Empty means any day.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Weekdays(Vec<Weekday>);

/// The named sets a user can pick instead of listing days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeekdayPreset {
    Any,
    Weekdays,
    Weekends,
}

impl WeekdayPreset {
    pub const ALL: [Self; 3] = [Self::Any, Self::Weekdays, Self::Weekends];

    pub fn label(self) -> &'static str {
        match self {
            Self::Any => "any day",
            Self::Weekdays => "weekdays",
            Self::Weekends => "weekends",
        }
    }

    pub fn days(self) -> Weekdays {
        match self {
            Self::Any => Weekdays::any(),
            Self::Weekdays => Weekdays::only(WORKDAYS),
            Self::Weekends => Weekdays::only(WEEKENDS),
        }
    }
}

/// A token in a weekday list that names no day or preset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a day; use mon..sun, weekdays, weekends or any")]
pub struct UnknownWeekday(String);

impl Weekdays {
    /// Every day of the week.
    pub const EVERY_DAY: [Weekday; 7] = ALL;

    pub(crate) fn any() -> Self {
        Self(Vec::new())
    }

    /// The given days, sorted and without repeats. All seven is the same
    /// as any day.
    pub fn only(days: impl IntoIterator<Item = Weekday>) -> Self {
        let mut out = Self(Vec::new());
        for d in days {
            out.insert(d);
        }
        out
    }

    pub fn is_any(&self) -> bool {
        self.0.is_empty()
    }

    pub fn days(&self) -> &[Weekday] {
        &self.0
    }

    pub(crate) fn allows(&self, day: Weekday) -> bool {
        self.0.is_empty() || self.0.contains(&day)
    }

    /// Whether the day is listed. Any-day sets list no day.
    pub fn contains(&self, day: Weekday) -> bool {
        self.0.contains(&day)
    }

    /// Add the day if missing, remove it if present.
    pub fn toggle(&mut self, day: Weekday) {
        if self.0.contains(&day) {
            self.0.retain(|d| *d != day);
        } else {
            self.insert(day);
        }
    }

    fn insert(&mut self, day: Weekday) {
        if !self.0.contains(&day) {
            self.0.push(day);
            self.0.sort_by_key(|d| d.to_monday_zero_offset());
        }
        if self.0.len() == ALL.len() {
            self.0.clear();
        }
    }

    pub fn preset(&self) -> Option<WeekdayPreset> {
        WeekdayPreset::ALL.into_iter().find(|p| p.days() == *self)
    }

    /// The stored form: a comma list of short names, empty for any day.
    pub fn to_spec(&self) -> String {
        self.0.iter().map(|d| day_name(*d)).collect::<Vec<_>>().join(",")
    }
}

/// A day's short name: `mon` to `sun`.
pub fn day_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "mon",
        Weekday::Tuesday => "tue",
        Weekday::Wednesday => "wed",
        Weekday::Thursday => "thu",
        Weekday::Friday => "fri",
        Weekday::Saturday => "sat",
        Weekday::Sunday => "sun",
    }
}

fn parse_day(token: &str) -> Option<Weekday> {
    ALL.into_iter().find(|d| {
        let short = day_name(*d);
        token == short || token == format!("{d:?}").to_lowercase()
    })
}

/// A preset (`any`, `weekdays`, `weekends`) or a comma list of days such
/// as `sat,sun`. Empty is any day.
impl FromStr for Weekdays {
    type Err = UnknownWeekday;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let spec = s.trim().to_lowercase();
        match spec.as_str() {
            "" | "any" | "all" => return Ok(Self::any()),
            "weekdays" => return Ok(WeekdayPreset::Weekdays.days()),
            "weekends" | "weekend" => return Ok(WeekdayPreset::Weekends.days()),
            _ => {}
        }
        let mut days = Vec::new();
        for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            days.push(
                parse_day(token)
                    .ok_or_else(|| UnknownWeekday(token.to_string()))?,
            );
        }
        Ok(Self::only(days))
    }
}

/// The label a user reads: a preset's name, or the day list.
impl fmt::Display for Weekdays {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.preset() {
            Some(p) => f.pad(p.label()),
            None => f.pad(&self.to_spec()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_lists_and_errors() {
        assert!("any".parse::<Weekdays>().unwrap().is_any());
        assert!("".parse::<Weekdays>().unwrap().is_any());
        let w: Weekdays = "Sat, sun".parse().unwrap();
        assert_eq!(w.preset(), Some(WeekdayPreset::Weekends));
        assert_eq!(w.to_spec(), "sat,sun");
        assert_eq!(w.to_string(), "weekends");
        let w: Weekdays = "tuesday,thu".parse().unwrap();
        assert_eq!(w.days(), &[Weekday::Tuesday, Weekday::Thursday]);
        assert_eq!(w.to_string(), "tue,thu");
        assert_eq!(
            "mon,funday".parse::<Weekdays>(),
            Err(UnknownWeekday("funday".into())),
            "unknown days are an error, not dropped"
        );
    }

    #[test]
    fn toggling_all_seven_is_any_day() {
        let mut w = Weekdays::only([Weekday::Monday]);
        for d in [
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
            Weekday::Saturday,
            Weekday::Sunday,
        ] {
            w.toggle(d);
        }
        assert!(w.is_any());
        assert!(w.allows(Weekday::Sunday));
        let mut w = Weekdays::only([Weekday::Monday, Weekday::Friday]);
        w.toggle(Weekday::Monday);
        assert_eq!(w.days(), &[Weekday::Friday]);
    }
}
