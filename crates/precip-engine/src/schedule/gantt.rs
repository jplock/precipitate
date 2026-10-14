//! A Gantt layout of the schedule: one row per migration, one cell per
//! day, week or month depending on the width available. Pure data, so
//! the terminal interface paints it and the command line prints it from
//! the same layout.

use super::Schedule;
use jiff::civil::Date;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell {
    Empty,
    /// Part of the migration's span.
    Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub workload: String,
    pub environment: String,
    pub wave: u32,
    pub locked: bool,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gantt {
    /// The date the first column starts on.
    pub start: Date,
    pub days_per_cell: u32,
    pub columns: usize,
    /// Column index and label where a month begins.
    pub months: Vec<(usize, String)>,
    /// Column holding today, when it is in range.
    pub today: Option<usize>,
    pub rows: Vec<Row>,
}

impl Gantt {
    fn column_of(&self, d: Date) -> Option<usize> {
        let days = (d - self.start).get_days();
        if days < 0 {
            return None;
        }
        let col = usize::try_from(days).ok()?
            / usize::try_from(self.days_per_cell).ok()?;
        (col < self.columns).then_some(col)
    }
}

/// Lay the schedule out in at most `max_columns` cells, choosing days,
/// weeks or months so everything fits.
pub fn build(schedule: &Schedule, today: Date, max_columns: usize) -> Gantt {
    let max_columns = max_columns.max(1);
    let first =
        schedule.slots.iter().map(|s| s.date).min().unwrap_or(today).min(today);
    let last =
        schedule.slots.iter().map(|s| s.end).max().unwrap_or(today).max(today);
    let span_days = u32::try_from((last - first).get_days()).unwrap_or(0) + 1;
    let days_per_cell =
        if usize::try_from(span_days).unwrap_or(usize::MAX) <= max_columns {
            1
        } else if usize::try_from(span_days.div_ceil(7)).unwrap_or(usize::MAX)
            <= max_columns
        {
            7
        } else {
            30
        };
    let columns = usize::try_from(span_days.div_ceil(days_per_cell))
        .unwrap_or(max_columns)
        .min(max_columns);
    let mut gantt = Gantt {
        start: first,
        days_per_cell,
        columns,
        months: Vec::new(),
        today: None,
        rows: Vec::new(),
    };
    gantt.today = gantt.column_of(today);

    // Month labels where a month starts inside the range, plus the first.
    let mut d = first;
    let mut last_month = None;
    while d <= last {
        if last_month != Some((d.year(), d.month())) {
            if let Some(col) = gantt.column_of(d) {
                let label = if d.year() == today.year() {
                    d.strftime("%b").to_string()
                } else {
                    d.strftime("%b %Y").to_string()
                };
                if gantt.months.last().is_none_or(|(c, _)| *c != col) {
                    gantt.months.push((col, label));
                }
            }
            last_month = Some((d.year(), d.month()));
        }
        let Ok(next) = d.tomorrow() else { break };
        d = next;
    }

    for s in &schedule.slots {
        let mut cells = vec![Cell::Empty; columns];
        let mut d = s.date;
        while d <= s.end {
            if let Some(col) = gantt.column_of(d) {
                cells[col] = Cell::Span;
            }
            let Ok(next) = d.tomorrow() else { break };
            d = next;
        }
        gantt.rows.push(Row {
            workload: s.workload.clone(),
            environment: s.environment.clone(),
            wave: s.wave,
            locked: s.locked,
            cells,
        });
    }
    gantt
}

/// Plain text, for the command line: a month line, a today marker, then
/// one line per row with `█` for agreed spans and `░` for proposed ones.
pub fn render(gantt: &Gantt, label_width: usize) -> Vec<String> {
    let mut months = vec![' '; gantt.columns];
    for (col, label) in &gantt.months {
        for (i, c) in label.chars().enumerate() {
            if let Some(cell) = months.get_mut(col + i) {
                *cell = c;
            }
        }
    }
    let mut lines = vec![format!(
        "{:<label_width$} {}",
        format!("per cell: {}", unit(gantt.days_per_cell)),
        months.into_iter().collect::<String>()
    )];
    let mut today = vec![' '; gantt.columns];
    if let Some(col) = gantt.today {
        today[col] = '▼';
    }
    lines.push(format!(
        "{:<label_width$} {}",
        "today",
        today.into_iter().collect::<String>()
    ));
    for r in &gantt.rows {
        let bar: String = r
            .cells
            .iter()
            .map(|c| match (c, r.locked) {
                (Cell::Empty, _) => ' ',
                (Cell::Span, true) => '█',
                (Cell::Span, false) => '░',
            })
            .collect();
        let label = format!("{} {} / {}", r.wave, r.workload, r.environment);
        let label: String = label.chars().take(label_width).collect();
        lines.push(format!("{label:<label_width$} {bar}"));
    }
    lines
}

pub fn unit(days_per_cell: u32) -> &'static str {
    match days_per_cell {
        1 => "a day",
        7 => "a week",
        _ => "a month",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::Slot;
    use jiff::civil::date;

    fn slot(w: &str, start: Date, end: Date, locked: bool) -> Slot {
        Slot {
            workload: w.into(),
            environment: "prod".into(),
            wave: 1,
            date: start,
            end,
            weight: 1,
            locked,
            reason: String::new(),
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
        let g = build(&s, date(2026, 10, 14), 80);
        assert_eq!((g.days_per_cell, g.columns), (1, 9));
        assert_eq!(g.today, Some(0));
        assert_eq!(g.rows[1].cells[6..=8], [Cell::Span; 3]);
        assert_eq!(g.rows[1].cells[5], Cell::Empty);
        assert_eq!(g.months, vec![(0, "Oct".to_string())]);
        let text = render(&g, 12);
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
        let g = build(&s, date(2026, 10, 14), 20);
        assert_eq!(g.days_per_cell, 7);
        assert!(g.columns <= 20);
        assert_eq!(g.months.len(), 3, "Oct, Nov, Dec");
        assert!(g.rows[0].cells.iter().all(|c| *c == Cell::Span));
    }
}
