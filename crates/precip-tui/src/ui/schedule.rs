//! The Schedule tab, as a table or a Gantt chart.

use super::{accent, dim, panel, short_date};
use crate::{App, ScheduleView, logo, next_step};
use precip_engine::schedule::day_name;
use ratatui::prelude::*;
use ratatui::widgets::{Padding, Paragraph, Row, Table};

/// "waves  1 ends 2026-11-07 (3/5)   2 finance ends 2026-12-01 (0/2)".
pub(crate) fn wave_spans(m: &precip_db::Metrics) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("waves      ", dim())];
    if m.waves.is_empty() {
        spans.push(Span::styled("none defined, everything is wave 1", dim()));
        return spans;
    }
    for w in &m.waves {
        let late = matches!((w.projected_end, w.target_end), (Some(e), Some(t)) if e > t);
        let label = format!(
            "{}{} {}/{} ends {}",
            w.number,
            if w.name.is_empty() {
                String::new()
            } else {
                format!(" {}", w.name)
            },
            w.migrated,
            w.total,
            w.projected_end
                .map(|d| d.to_string())
                .unwrap_or_else(|| "done".into())
        );
        spans.push(Span::styled(
            label,
            if late {
                Style::default().fg(logo::OX_YELLOW)
            } else {
                Style::default()
            },
        ));
        spans.push(Span::raw("   "));
    }
    spans
}

/// The schedule as bars: agreed in green, proposed dimmed, today marked,
/// months along the top, and how the projected end has moved below.
pub(crate) fn draw_gantt(v: &ScheduleView, frame: &mut Frame, area: Rect) {
    use precip_engine::schedule::gantt::Gantt;
    let label_width: u16 = 30;
    let width = usize::from(label_width);
    let inner = area.width.saturating_sub(label_width + 4);
    let g = Gantt::new(&v.schedule, v.today, usize::from(inner.max(1)));
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{:<width$}", format!("each cell is {}", g.unit)),
                dim(),
            ),
            Span::raw(" "),
            Span::styled(g.month_line(), dim()),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<width$}", "today"), dim()),
            Span::raw(" "),
            Span::styled(g.today_line(), accent()),
        ]),
    ];
    for r in &g.rows {
        let label: String =
            format!("{} {}", r.wave, r.pair).chars().take(width).collect();
        let bar: String =
            r.cells.iter().map(|on| if *on { '█' } else { ' ' }).collect();
        lines.push(Line::from(vec![
            Span::raw(format!("{label:<width$}")),
            Span::raw(" "),
            Span::styled(bar, if r.agreed { accent() } else { dim() }),
        ]));
    }
    let m = &v.metrics;
    if m.end_history.len() > 1 {
        let trail: Vec<String> = m
            .end_history
            .iter()
            .map(|(day, ends)| {
                format!(
                    "{}: {}",
                    short_date(*day, v.today),
                    short_date(*ends, v.today)
                )
            })
            .collect();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("projected end over time   ", dim()),
            Span::raw(trail.join("  →  ")),
        ]));
    }
    if m.forecast_within_week_rate > 0.0 || m.forecast_error_days != 0.0 {
        lines.push(Line::from(vec![
            Span::styled("forecast                  ", dim()),
            Span::raw(format!(
                "first plan off by {:+.1} days on average; {:.0}% landed within a week of it",
                m.forecast_error_days,
                m.forecast_within_week_rate * 100.0
            )),
        ]));
    }
    let title = format!(
        "{} to migrate   green agreed, grey proposed",
        v.schedule.slots.len()
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(&title).padding(Padding::horizontal(1))),
        area,
    );
}

pub(crate) fn draw_schedule(app: &mut App, frame: &mut Frame, area: Rect) {
    let Some(v) = app.schedule.as_mut() else {
        frame.render_widget(
            Paragraph::new("  The schedule appears once a scan has been stored and workloads have dispositions.")
                .style(dim())
                .block(panel("Schedule")),
            area,
        );
        return;
    };
    let header_rows =
        if v.metrics.reschedule_reasons.is_empty() { 8 } else { 9 };
    let [rules_area, table_area] =
        Layout::vertical([Constraint::Length(header_rows), Constraint::Min(6)])
            .areas(area);

    let r = &v.rules;
    let s = &v.schedule;
    let projected = match (s.ends, r.target_end) {
        (Some(end), Some(t)) if end > t => Span::styled(
            format!(
                "projected end {end}, {} days past the {t} target",
                (end - t).get_days()
            ),
            Style::default().fg(logo::OX_YELLOW),
        ),
        (Some(end), Some(t)) => Span::styled(
            format!("projected end {end}, within the {t} target"),
            accent(),
        ),
        (Some(end), None) => {
            Span::raw(format!("projected end {end}, no target set"))
        }
        (None, _) => Span::styled("nothing to schedule", dim()),
    };
    let weekdays = r.weekdays.to_string();
    let m = &v.metrics;
    let pct = |r: f64| format!("{:.0}%", r * 100.0);
    let drift = match m.end_drift_days {
        0 => Span::styled("no drift since first plan".to_string(), dim()),
        d if d > 0 => Span::styled(
            format!("slipped {d} days since first plan"),
            Style::default().fg(logo::OX_YELLOW),
        ),
        d => Span::styled(format!("pulled in {} days", -d), accent()),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("next       ", dim()),
            Span::styled(next_step(v), accent().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("capacity   ", dim()),
            Span::raw(format!(
                "{} per day, {} per week, {} per month, {weekdays}",
                r.per_day, r.per_week, r.per_month
            )),
            Span::styled(
                if r.nonprod_before_prod {
                    "   non-prod before prod"
                } else {
                    ""
                },
                dim(),
            ),
        ]),
        Line::from(vec![
            Span::styled("today      ", dim()),
            Span::raw(v.today.to_string()),
            Span::raw("   "),
            projected,
            Span::raw("   "),
            drift,
        ]),
        Line::from(vec![
            Span::styled("progress   ", dim()),
            Span::raw(format!(
                "{} of {} migrated, {} confirmed, {} proposed, {} missed",
                m.migrated, m.total, m.confirmed, m.proposed, m.missed
            )),
            Span::styled(
                format!("   {} in the last 30 days", m.migrated_last_30_days),
                dim(),
            ),
        ]),
        Line::from(vec![
            Span::styled("rates      ", dim()),
            Span::raw(format!(
                "rescheduled {} of confirmed   first-time success {}   confirmed {}   rolled back {}   average slip {:.1} days",
                pct(m.reschedule_rate),
                pct(m.first_time_success_rate),
                pct(m.confirmation_rate),
                pct(m.rollback_rate),
                m.average_slip_days
            )),
        ]),
        Line::from(wave_spans(m)),
    ];
    let mut lines = lines;
    if !m.reschedule_reasons.is_empty() {
        let list: Vec<String> = m
            .reschedule_reasons
            .iter()
            .map(|(r, n)| format!("{r} {n}"))
            .collect();
        lines.push(Line::from(vec![
            Span::styled("why moved  ", dim()),
            Span::raw(list.join("   ")),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            panel(&format!("Schedule   version #{}", v.version))
                .padding(Padding::horizontal(1)),
        ),
        rules_area,
    );

    if v.gantt {
        draw_gantt(v, frame, table_area);
        return;
    }
    let confirmed = s.slots.iter().filter(|x| x.agreed).count();
    let rows: Vec<Row> = s
        .slots
        .iter()
        .map(|x| {
            let alts: Vec<String> =
                x.alternatives.iter().map(|d| d.to_string()).collect();
            let style = if x.agreed {
                accent()
            } else if r.target_end.is_some_and(|t| x.date > t) {
                Style::default().fg(logo::OX_YELLOW)
            } else {
                Style::default()
            };
            let days = x.days();
            Row::new(vec![
                x.wave.to_string(),
                x.date.to_string(),
                day_name(x.date.weekday()).to_string(),
                if x.weight > 1 {
                    format!("{days}x{}", x.weight)
                } else {
                    days.to_string()
                },
                x.pair.workload.to_string(),
                x.pair.environment.to_string(),
                x.state_label().to_string(),
                alts.join(" or "),
                x.reason(),
            ])
            .style(style)
        })
        .chain(v.findings.iter().map(|f| {
            Row::new(vec![
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                f.workload.clone(),
                f.environment.clone(),
                "migrated?".to_string(),
                String::new(),
                format!(
                    "{} resource(s) still in the latest scan; press enter",
                    f.remaining
                ),
            ])
            .style(Style::default().fg(logo::OX_YELLOW))
        }))
        .collect();
    let title = format!(
        "{} to migrate, {confirmed} confirmed, {} late{}{}{}",
        s.slots.len(),
        s.late.len(),
        if s.unplaced.is_empty() {
            String::new()
        } else {
            format!(", {} unplaced", s.unplaced.len())
        },
        if s.conflicts.is_empty() {
            String::new()
        } else {
            format!(", {} conflicts", s.conflicts.len())
        },
        if v.findings.is_empty() {
            String::new()
        } else {
            format!(", {} to verify", v.findings.len())
        }
    );
    frame.render_stateful_widget(
        Table::new(
            rows,
            [
                Constraint::Length(4),
                Constraint::Length(11),
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Length(24),
                Constraint::Length(9),
                Constraint::Length(12),
                Constraint::Length(26),
                Constraint::Min(16),
            ],
        )
        .column_spacing(2)
        .header(
            Row::new([
                "wave",
                "start",
                "day",
                "days",
                "workload",
                "env",
                "state",
                "alternatives",
                "why this date",
            ])
            .style(dim()),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .block(panel(&title).padding(Padding::horizontal(1))),
        table_area,
        &mut v.selected,
    );
}
