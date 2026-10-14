//! Holidays computed from their rules, so no data source is needed. Only
//! the United States federal set is built in; others are entered as
//! blackouts by hand.

use jiff::civil::{Date, Weekday, date};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holiday {
    pub name: &'static str,
    pub date: Date,
}

/// The `n`th `weekday` of a month, or the last when `n` is 0.
fn nth_weekday(year: i16, month: i8, weekday: Weekday, n: u8) -> Date {
    if n == 0 {
        let mut d = date(year, month, 1).last_of_month();
        while d.weekday() != weekday {
            d = d.yesterday().unwrap_or(d);
        }
        return d;
    }
    let mut d = date(year, month, 1);
    while d.weekday() != weekday {
        d = d.tomorrow().unwrap_or(d);
    }
    d.checked_add(jiff::Span::new().weeks(i64::from(n) - 1)).unwrap_or(d)
}

/// Fixed-date holidays are observed on the nearest weekday when they fall
/// on a weekend, as the federal government does.
fn observed(d: Date) -> Date {
    match d.weekday() {
        Weekday::Saturday => d.yesterday().unwrap_or(d),
        Weekday::Sunday => d.tomorrow().unwrap_or(d),
        _ => d,
    }
}

pub fn us_federal(year: i16) -> Vec<Holiday> {
    vec![
        Holiday { name: "New Year's Day", date: observed(date(year, 1, 1)) },
        Holiday {
            name: "Martin Luther King Jr. Day",
            date: nth_weekday(year, 1, Weekday::Monday, 3),
        },
        Holiday {
            name: "Presidents' Day",
            date: nth_weekday(year, 2, Weekday::Monday, 3),
        },
        Holiday {
            name: "Memorial Day",
            date: nth_weekday(year, 5, Weekday::Monday, 0),
        },
        Holiday { name: "Juneteenth", date: observed(date(year, 6, 19)) },
        Holiday { name: "Independence Day", date: observed(date(year, 7, 4)) },
        Holiday {
            name: "Labor Day",
            date: nth_weekday(year, 9, Weekday::Monday, 1),
        },
        Holiday {
            name: "Columbus Day",
            date: nth_weekday(year, 10, Weekday::Monday, 2),
        },
        Holiday { name: "Veterans Day", date: observed(date(year, 11, 11)) },
        Holiday {
            name: "Thanksgiving",
            date: nth_weekday(year, 11, Weekday::Thursday, 4),
        },
        Holiday { name: "Christmas Day", date: observed(date(year, 12, 25)) },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_2026_dates() {
        let h = us_federal(2026);
        let find =
            |n: &str| h.iter().find(|x| x.name == n).map(|x| x.date).unwrap();
        assert_eq!(find("Thanksgiving"), date(2026, 11, 26));
        assert_eq!(find("Memorial Day"), date(2026, 5, 25));
        assert_eq!(find("Labor Day"), date(2026, 9, 7));
        assert_eq!(
            find("Independence Day"),
            date(2026, 7, 3),
            "4 July 2026 is a Saturday, observed Friday"
        );
        assert_eq!(find("Martin Luther King Jr. Day"), date(2026, 1, 19));
    }
}
