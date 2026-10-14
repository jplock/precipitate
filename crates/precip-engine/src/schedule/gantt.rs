//! A Gantt layout of the schedule: one row per migration, one cell per
//! day, week or month depending on the width available. Pure data, so
//! the terminal interface paints it and the command line prints it from
//! the same layout.

use super::{Schedule, days};
use jiff::civil::Date;
use precip_types::PairKey;
use std::fmt;

/// How much time one cell covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellUnit {
    Day,
    Week,
    Month,
}

impl CellUnit {
    pub fn days(self) -> u32 {
        match self {
            Self::Day => 1,
            Self::Week => 7,
            Self::Month => 30,
        }
    }

    /// The smallest unit that fits `span_days` into `columns`.
    fn fitting(span_days: u32, columns: usize) -> Self {
        let fits = |unit: Self| {
            usize::try_from(span_days.div_ceil(unit.days()))
                .is_ok_and(|n| n <= columns)
        };
        if fits(Self::Day) {
            Self::Day
        } else if fits(Self::Week) {
            Self::Week
        } else {
            Self::Month
        }
    }
}

impl fmt::Display for CellUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Day => "a day",
            Self::Week => "a week",
            Self::Month => "a month",
        })
    }
}

/// One migration's row: which cells its span covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub pair: PairKey,
    pub wave: u32,
    pub agreed: bool,
    pub cells: Vec<bool>,
}

/// A month name over the column where the month starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonthLabel {
    pub column: usize,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gantt {
    /// The date the first column starts on.
    pub start: Date,
    pub unit: CellUnit,
    pub columns: usize,
    pub months: Vec<MonthLabel>,
    /// Column holding today, when it is in range.
    pub today: Option<usize>,
    pub rows: Vec<Row>,
}

impl Gantt {
    /// Lay the schedule out in at most `max_columns` cells, choosing days,
    /// weeks or months so everything fits.
    pub fn new(schedule: &Schedule, today: Date, max_columns: usize) -> Self {
        let max_columns = max_columns.max(1);
        let first = schedule
            .slots
            .iter()
            .map(|s| s.date)
            .min()
            .unwrap_or(today)
            .min(today);
        let last = schedule
            .slots
            .iter()
            .map(|s| s.end)
            .max()
            .unwrap_or(today)
            .max(today);
        let span_days =
            u32::try_from((last - first).get_days()).unwrap_or(0) + 1;
        let unit = CellUnit::fitting(span_days, max_columns);
        let columns = usize::try_from(span_days.div_ceil(unit.days()))
            .unwrap_or(max_columns)
            .min(max_columns);
        let mut gantt = Self {
            start: first,
            unit,
            columns,
            months: Vec::new(),
            today: None,
            rows: Vec::new(),
        };
        gantt.today = gantt.column_of(today);

        // A label where each month starts inside the range, and the first.
        for d in days(first, last) {
            if d != first && d.day() != 1 {
                continue;
            }
            let Some(column) = gantt.column_of(d) else { continue };
            if gantt.months.last().is_some_and(|m| m.column == column) {
                continue;
            }
            let label = if d.year() == today.year() {
                d.strftime("%b").to_string()
            } else {
                d.strftime("%b %Y").to_string()
            };
            gantt.months.push(MonthLabel { column, label });
        }

        for s in &schedule.slots {
            let mut cells = vec![false; columns];
            for d in days(s.date, s.end) {
                if let Some(c) = gantt.column_of(d) {
                    cells[c] = true;
                }
            }
            gantt.rows.push(Row {
                pair: s.pair.clone(),
                wave: s.wave,
                agreed: s.agreed,
                cells,
            });
        }
        gantt
    }

    fn column_of(&self, d: Date) -> Option<usize> {
        let offset = usize::try_from((d - self.start).get_days()).ok()?;
        let unit = usize::try_from(self.unit.days()).ok()?;
        let column = offset / unit;
        (column < self.columns).then_some(column)
    }

    /// The month labels laid out over the columns.
    pub fn month_line(&self) -> String {
        let mut line = vec![' '; self.columns];
        for m in &self.months {
            for (i, c) in m.label.chars().enumerate() {
                if let Some(cell) = line.get_mut(m.column + i) {
                    *cell = c;
                }
            }
        }
        line.into_iter().collect()
    }

    /// A marker under today's column.
    pub fn today_line(&self) -> String {
        let mut line = vec![' '; self.columns];
        if let Some(c) = self.today {
            line[c] = '▼';
        }
        line.into_iter().collect()
    }

    /// Plain text, for the command line: a month line, a today marker,
    /// then one line per row with `█` for agreed spans and `░` for
    /// proposed ones.
    pub fn render(&self, label_width: usize) -> Vec<String> {
        let mut lines = vec![
            format!(
                "{:<label_width$} {}",
                format!("each cell is {}", self.unit),
                self.month_line()
            ),
            format!("{:<label_width$} {}", "today", self.today_line()),
        ];
        for r in &self.rows {
            let mark = if r.agreed { '█' } else { '░' };
            let bar: String =
                r.cells.iter().map(|on| if *on { mark } else { ' ' }).collect();
            let label: String = format!("{} {}", r.wave, r.pair)
                .chars()
                .take(label_width)
                .collect();
            lines.push(format!("{label:<label_width$} {bar}"));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Placement, Slot};
    use jiff::civil::date;

    fn slot(w: &str, start: Date, end: Date, agreed: bool) -> Slot {
        Slot {
            pair: PairKey::new(w, "prod"),
            wave: 1,
            date: start,
            end,
            weight: 1,
            agreed,
            placement: Placement::FirstOpenDay,
            reopened: None,
            alternatives: Vec::new(),
        }
    }

    #[test]
    fn one_cell_per_day_when_it_fits() {
        let s = Schedule {
            slots: vec![
                slot("a", date(2026, 10, 14), date(2026, 10, 14), false),
                slot("b", date(2026, 10, 20), date(2026, 10, 22), true),
            ],
            ..Default::default()
        };
        let g = Gantt::new(&s, date(2026, 10, 14), 80);
        assert_eq!((g.unit, g.columns), (CellUnit::Day, 9));
        assert_eq!(g.today, Some(0));
        assert_eq!(g.rows[1].cells[6..=8], [true; 3]);
        assert!(!g.rows[1].cells[5]);
        assert_eq!(
            g.months,
            vec![MonthLabel { column: 0, label: "Oct".into() }]
        );
        let text = g.render(12);
        assert!(text[3].ends_with("███"));
        assert!(text[2].contains('░'));
    }

    #[test]
    fn weeks_when_days_would_not_fit_and_months_are_labelled() {
        let s = Schedule {
            slots: vec![slot(
                "a",
                date(2026, 10, 14),
                date(2026, 12, 20),
                false,
            )],
            ..Default::default()
        };
        let g = Gantt::new(&s, date(2026, 10, 14), 20);
        assert_eq!(g.unit, CellUnit::Week);
        assert!(g.columns <= 20);
        assert_eq!(g.months.len(), 3, "Oct, Nov, Dec");
        assert!(g.rows[0].cells.iter().all(|c| *c));
    }
}
