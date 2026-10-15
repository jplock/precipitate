//! Drawing. Layout: header (mark, name, context), tab bar, body, activity
//! feed, key hints.

use crate::{
    ALL_WEEKDAYS, App, DISPOSITIONS, Fields, PairRow, RuleRow, ScheduleView,
    Screen, Tab, WEEKDAY_ROWS, disposition_help, logo, next_step,
    weekday_rule_label,
};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, Padding, Paragraph, Row,
    Table, Tabs,
};

const ACCENT: Color = logo::OX_GREEN_LIGHT;
const DIM: Color = logo::OX_GRAY_DARK;

fn accent() -> Style {
    Style::default().fg(ACCENT)
}

fn dim() -> Style {
    Style::default().fg(DIM)
}

fn panel(title: &str) -> Block<'_> {
    Block::default().borders(Borders::ALL).border_style(dim()).title(
        Span::styled(
            format!(" {title} "),
            accent().add_modifier(Modifier::BOLD),
        ),
    )
}

pub fn draw(app: &mut App, frame: &mut Frame) {
    if let Screen::Picker(_) = app.screen {
        draw_picker(app, frame);
        return;
    }
    let scanning = app.progress.as_ref().is_some_and(|p| p.running());
    let [header, tabs, body, activity, hints] = Layout::vertical([
        Constraint::Length(10),
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(if scanning { 10 } else { 7 }),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(app, frame, header);
    draw_tabs(app, frame, tabs);
    match app.tab {
        Tab::Overview => draw_overview(app, frame, body),
        Tab::Workloads => draw_workloads(app, frame, body),
        Tab::Sizing => draw_sizing(app, frame, body),
        Tab::Schedule => draw_schedule(app, frame, body),
        Tab::Scans => draw_scans(app, frame, body),
        Tab::Accounts => draw_accounts(app, frame, body),
    }
    draw_activity(app, frame, activity);
    draw_hints(app, frame, hints);
    draw_picker_popup(app, frame);
    draw_date_picker(app, frame);
    draw_history(app, frame);
    draw_rules_editor(app, frame);
    draw_pair_editor(app, frame);
    draw_weekday_picker(app, frame);
    draw_depends_picker(app, frame);
    draw_holiday_picker(app, frame);
    draw_merge_prompt(app, frame);
    draw_reason_picker(app, frame);
    draw_export_prompt(app, frame);
    draw_action_menu(app, frame);
}

fn draw_date_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.date_picker.as_mut() else { return };
    let area = frame.area();
    let width = 56.min(area.width.saturating_sub(4));
    let height = u16::try_from(p.dates.len()).unwrap_or(3) + 5;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = p
        .dates
        .iter()
        .enumerate()
        .map(|(i, d)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{d}  {}", precip_db::weekday_name(d.weekday())),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if i == 0 && !p.replacing {
                        "   proposed"
                    } else {
                        "   alternative"
                    },
                    dim(),
                ),
            ]))
        })
        .collect();
    let title = if p.replacing {
        format!(" Re-confirm {} / {} ", p.workload, p.environment)
    } else {
        format!(" Confirm {} / {} ", p.workload, p.environment)
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(accent())
                    .title(Span::styled(
                        title,
                        accent().add_modifier(Modifier::BOLD),
                    ))
                    .title_bottom(Span::styled(
                        " enter to confirm   esc to cancel ",
                        dim(),
                    ))
                    .padding(Padding::new(1, 1, 1, 0)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

/// "waves  1 ends 2026-11-07 (3/5)   2 finance ends 2026-12-01 (0/2)".
fn wave_spans(m: &precip_db::Metrics) -> Vec<Span<'static>> {
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

/// A centred rectangle for a popup, clamped to the terminal.
fn popup_rect(frame: &Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn popup_block<'a>(
    title: String,
    hint: String,
    hint_style: Style,
) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(accent())
        .title(Span::styled(
            format!(" {title} "),
            accent().add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(format!(" {hint} "), hint_style))
        .padding(Padding::new(1, 1, 1, 0))
}

fn warn() -> Style {
    Style::default().fg(logo::OX_YELLOW).add_modifier(Modifier::BOLD)
}

fn draw_action_menu(app: &mut App, frame: &mut Frame) {
    let Some(m) = app.action_menu.as_mut() else { return };
    let popup = popup_rect(
        frame,
        96,
        u16::try_from(m.actions.len() * 2).unwrap_or(6) + 5,
    );
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = m
        .actions
        .iter()
        .map(|a| {
            ListItem::new(vec![
                Line::from(Span::styled(
                    a.label(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(format!("  {}", a.help()), dim())),
            ])
        })
        .collect();
    let (hint, style) = match m.confirming {
        Some(a) => (
            format!("{}? enter again to confirm, esc to go back", a.label()),
            warn(),
        ),
        None => ("enter to choose   esc to close".to_string(), dim()),
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(popup_block(m.title.clone(), hint, style))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut m.state,
    );
}

/// The value column of a one-box row, showing the box while it is
/// being edited.
fn typed(input: &Option<Fields>, editing: bool, shown: String) -> String {
    match input {
        Some(f) if editing => format!("{}_", f.get(0)),
        _ => shown,
    }
}

/// The boxes of a form: label, value, a cursor in the active one.
fn form_spans(f: &Fields) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (label, v)) in f.labels.iter().zip(&f.values).enumerate() {
        let active = i == f.active;
        spans.push(Span::styled(format!("{label}: "), dim()));
        let shown = if active {
            format!("{v}_")
        } else if v.is_empty() {
            "____".to_string()
        } else {
            v.clone()
        };
        let style = if active {
            accent().add_modifier(Modifier::BOLD)
        } else if v.is_empty() {
            dim()
        } else {
            accent()
        };
        spans.push(Span::styled(shown, style));
        spans.push(Span::raw("   "));
    }
    spans
}

fn value(text: String) -> Span<'static> {
    Span::styled(text, accent())
}

/// "Nov 26", or "Nov 26 2027" when the year is not this one.
fn short_date(d: jiff::civil::Date, today: jiff::civil::Date) -> String {
    if d.year() == today.year() {
        d.strftime("%b %-d").to_string()
    } else {
        d.strftime("%b %-d %Y").to_string()
    }
}

/// "Nov 26", or "Oct 26 – Oct 30" for a range.
fn short_span(
    start: jiff::civil::Date,
    end: jiff::civil::Date,
    today: jiff::civil::Date,
) -> String {
    if start == end {
        short_date(start, today)
    } else {
        format!("{} – {}", short_date(start, today), short_date(end, today))
    }
}

/// A label in a fixed column, then the value.
fn field(label: &str, value_text: String) -> Line<'static> {
    Line::from(vec![Span::raw(format!("  {label:<18}")), value(value_text)])
}

/// An "add" row: short until it is being edited, then its boxes.
fn add_row(
    label: &str,
    input: &Option<Fields>,
    editing: bool,
) -> Line<'static> {
    match input {
        Some(f) if editing => {
            let mut spans = vec![Span::raw(format!("  + {label}:   "))];
            spans.extend(form_spans(f));
            Line::from(spans)
        }
        _ => Line::from(Span::styled(format!("  + {label}"), dim())),
    }
}

fn header_row(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        dim().add_modifier(Modifier::BOLD),
    ))
}

/// The hint for the selected row, in one place, instead of on every row.
fn editor_hint(what: &str, input: bool, confirming: bool) -> (String, Style) {
    if confirming {
        ("remove this? press the same key again, esc to keep".into(), warn())
    } else if input {
        (
            "type   tab next box   enter to save   esc to cancel   a freeze with no \"to\" is one day"
                .into(),
            dim(),
        )
    } else {
        (
            format!("{what}   ↑ ↓ move   esc to close   changes save at once"),
            dim(),
        )
    }
}

fn draw_rules_editor(app: &mut App, frame: &mut Frame) {
    let Some(ed) = app.rules_editor.as_mut() else { return };
    let popup =
        popup_rect(frame, 84, u16::try_from(ed.rows.len()).unwrap_or(8) + 5);
    frame.render_widget(Clear, popup);
    let r = &ed.rules;
    let today = ed.today;
    let selected = ed.state.selected().unwrap_or(1);
    let items: Vec<ListItem> = ed
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let editing = selected == i;
            let line = match *row {
                RuleRow::Header(text) => header_row(text),
                RuleRow::PerDay => field("Per day", r.per_day.to_string()),
                RuleRow::PerWeek => field("Per week", r.per_week.to_string()),
                RuleRow::PerMonth => {
                    field("Per month", r.per_month.to_string())
                }
                RuleRow::Weekdays => {
                    field("Days", weekday_rule_label(&r.weekdays))
                }
                RuleRow::TargetEnd => field(
                    "Done by",
                    typed(
                        &ed.input,
                        editing,
                        r.target_end
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "no date set".into()),
                    ),
                ),
                RuleRow::NonprodFirst => field(
                    "Non-prod first",
                    if r.nonprod_before_prod {
                        "yes".into()
                    } else {
                        "no".into()
                    },
                ),
                RuleRow::Blackout(i) => {
                    let b = &ed.blackouts[i];
                    let who = match &b.pair {
                        Some((w, e)) => format!("   only {w} / {e}"),
                        None => String::new(),
                    };
                    Line::from(vec![
                        Span::raw("  "),
                        value(format!(
                            "{:<18}",
                            short_span(b.start, b.end, today)
                        )),
                        Span::raw(b.name.clone()),
                        Span::styled(who, dim()),
                    ])
                }
                RuleRow::AddBlackout => {
                    add_row("add a freeze", &ed.input, editing)
                }
                RuleRow::Holidays => add_row(
                    "pick from the US federal holidays of a year",
                    &ed.input,
                    editing,
                ),
                RuleRow::Wave(i) => {
                    let w = &ed.waves[i];
                    let mut spans =
                        vec![Span::raw(format!("  {:<18}", w.number))];
                    match &ed.input {
                        Some(f) if editing => spans.extend(form_spans(f)),
                        _ => spans.push(value(format!(
                            "{}{}",
                            if w.name.is_empty() {
                                "(unnamed)".to_string()
                            } else {
                                w.name.clone()
                            },
                            w.target_end
                                .map(|d| format!(
                                    "   by {}",
                                    short_date(d, today)
                                ))
                                .unwrap_or_default()
                        ))),
                    }
                    Line::from(spans)
                }
                RuleRow::AddWave => add_row("add a wave", &ed.input, editing),
            };
            ListItem::new(line)
        })
        .collect();
    let what = match ed.row() {
        RuleRow::PerDay | RuleRow::PerWeek | RuleRow::PerMonth => {
            "← → change the limit"
        }
        RuleRow::Weekdays => "enter to pick days",
        RuleRow::TargetEnd => "enter to type a date   ← → a week at a time",
        RuleRow::NonprodFirst => "enter to flip",
        RuleRow::Blackout(_) => "enter to remove this freeze",
        RuleRow::AddBlackout | RuleRow::Holidays | RuleRow::AddWave => {
            "enter to add"
        }
        RuleRow::Wave(_) => {
            "enter to rename or set when it's due   backspace to remove"
        }
        RuleRow::Header(_) => "",
    };
    let (hint, style) =
        editor_hint(what, ed.input.is_some(), ed.confirming_delete);
    frame.render_stateful_widget(
        List::new(items)
            .block(popup_block("The rules of the plan".into(), hint, style))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut ed.state,
    );
}

fn draw_pair_editor(app: &mut App, frame: &mut Frame) {
    let Some(ed) = app.pair_editor.as_mut() else { return };
    let popup =
        popup_rect(frame, 84, u16::try_from(ed.rows.len()).unwrap_or(8) + 5);
    frame.render_widget(Clear, popup);
    let p = &ed.pair;
    let today = jiff::Zoned::now().date();
    let selected = ed.state.selected().unwrap_or(1);
    let items: Vec<ListItem> = ed
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let editing = selected == i;
            let line = match *row {
                PairRow::Header(text) => header_row(text),
                PairRow::Wave => field("Wave", p.wave.to_string()),
                PairRow::Weekdays => field(
                    "Days",
                    if p.weekdays.is_empty() {
                        "as the plan allows".into()
                    } else {
                        weekday_rule_label(&p.weekdays)
                    },
                ),
                PairRow::Earliest => field(
                    "Not before",
                    typed(
                        &ed.input,
                        editing,
                        p.earliest
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "any date".into()),
                    ),
                ),
                PairRow::Duration => {
                    field("Days in a row", p.duration_days.to_string())
                }
                PairRow::Weight => {
                    field("Counts as", format!("{} migration(s)", p.weight))
                }
                PairRow::Priority => field("Priority", p.priority.to_string()),
                PairRow::Dependency(i) => {
                    let (w, e) = &p.depends_on[i];
                    Line::from(vec![
                        Span::raw("  "),
                        value(format!("{w} / {e}")),
                    ])
                }
                PairRow::AddDependency => {
                    add_row("wait for another pair", &ed.input, editing)
                }
                PairRow::Freeze(i) => {
                    let b = &ed.freezes[i];
                    Line::from(vec![
                        Span::raw("  "),
                        value(format!(
                            "{:<18}",
                            short_span(b.start, b.end, today)
                        )),
                        Span::raw(b.name.clone()),
                    ])
                }
                PairRow::AddFreeze => {
                    add_row("add a freeze", &ed.input, editing)
                }
            };
            ListItem::new(line)
        })
        .collect();
    let what = match ed.row() {
        PairRow::Wave
        | PairRow::Duration
        | PairRow::Weight
        | PairRow::Priority => "← → change",
        PairRow::Weekdays => "enter to pick days",
        PairRow::Earliest => "enter to type a date   ← → a week at a time",
        PairRow::Dependency(_) => "enter to stop waiting for it",
        PairRow::AddDependency => "enter to pick a pair",
        PairRow::Freeze(_) => "enter to remove this freeze",
        PairRow::AddFreeze => "enter to add",
        PairRow::Header(_) => "",
    };
    let (hint, style) =
        editor_hint(what, ed.input.is_some(), ed.confirming_delete);
    frame.render_stateful_widget(
        List::new(items)
            .block(popup_block(
                format!("{} / {}", p.workload, p.environment),
                hint,
                style,
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut ed.state,
    );
}

fn draw_holiday_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.holiday_picker.as_mut() else { return };
    let popup = popup_rect(
        frame,
        64,
        u16::try_from(p.holidays.len()).unwrap_or(11) + 5,
    );
    frame.render_widget(Clear, popup);
    let today = jiff::Zoned::now().date();
    let items: Vec<ListItem> = p
        .holidays
        .iter()
        .zip(&p.chosen)
        .map(|(h, on)| {
            let mark = if *on { "[x] " } else { "[ ] " };
            ListItem::new(Line::from(vec![
                Span::styled(mark, if *on { accent() } else { dim() }),
                Span::raw(format!("{:<8}", short_date(h.date, today))),
                Span::raw(h.name),
            ]))
        })
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .block(popup_block(
                format!("Which US federal holidays of {} do you observe?", p.year),
                "space to tick or untick   enter to add the ticked ones   esc to cancel".into(),
                dim(),
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

fn draw_reason_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.reason_picker.as_mut() else { return };
    let popup = popup_rect(
        frame,
        64,
        u16::try_from(crate::ReasonPicker::rows()).unwrap_or(6) + 5,
    );
    frame.render_widget(Clear, popup);
    let mut items: Vec<ListItem> = precip_db::RESCHEDULE_REASONS
        .iter()
        .map(|r| ListItem::new(Line::from((*r).to_string())))
        .collect();
    items.push(ListItem::new(Line::from(match &p.custom {
        Some(text) => vec![Span::raw("other: "), value(format!("{text}_"))],
        None => vec![Span::styled("other, typed in", dim())],
    })));
    let (what, title) = match &p.pending {
        crate::Pending::Missed { workload, environment } => {
            ("didn't happen", format!("{workload} / {environment}"))
        }
        crate::Pending::Reconfirm { workload, environment, date } => {
            ("moves", format!("{workload} / {environment} to {date}"))
        }
    };
    let hint = if p.custom.is_some() {
        "type the reason, enter to save   esc to pick from the list"
    } else {
        "enter to choose   esc to cancel"
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(popup_block(
                format!("Why it {what}: {title}"),
                hint.into(),
                dim(),
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

fn draw_depends_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.depends_picker.as_mut() else { return };
    let popup = popup_rect(
        frame,
        60,
        u16::try_from(p.candidates.len()).unwrap_or(8).min(20) + 5,
    );
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = p
        .candidates
        .iter()
        .map(|(w, e)| ListItem::new(Line::from(format!("{w} / {e}"))))
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .block(popup_block(
                "Wait for which pair?".into(),
                "enter to choose   esc to cancel".into(),
                dim(),
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

fn draw_merge_prompt(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.merge_prompt.as_ref() else { return };
    let popup = popup_rect(frame, 72, 6);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(vec![
            Span::raw(format!("{} is really part of ", p.alias)),
            value(format!("{}_", p.text)),
        ]),
        Line::from(Span::styled(
            "Its resources move to that workload when the scan is regrouped.",
            dim(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(popup_block(
            "Fold into another workload".into(),
            "type the workload name, enter to save   esc to cancel".into(),
            dim(),
        )),
        popup,
    );
}

/// The schedule as bars: agreed in green, proposed dimmed, today marked,
/// months along the top, and how the projected end has moved below.
fn draw_gantt(v: &ScheduleView, frame: &mut Frame, area: Rect) {
    use precip_engine::schedule::gantt::{self, Cell};
    let label_width: u16 = 30;
    let inner = area.width.saturating_sub(label_width + 4);
    let g = gantt::build(&v.schedule, v.today, usize::from(inner.max(1)));
    let mut months = vec![' '; g.columns];
    for (col, label) in &g.months {
        for (i, c) in label.chars().enumerate() {
            if let Some(cell) = months.get_mut(col + i) {
                *cell = c;
            }
        }
    }
    let mut today_row = vec![' '; g.columns];
    if let Some(col) = g.today {
        today_row[col] = '▼';
    }
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!(
                    "{:<label_width$}",
                    format!("each cell is {}", gantt::unit(g.days_per_cell)),
                    label_width = usize::from(label_width)
                ),
                dim(),
            ),
            Span::raw(" "),
            Span::styled(months.into_iter().collect::<String>(), dim()),
        ]),
        Line::from(vec![
            Span::styled(
                format!(
                    "{:<label_width$}",
                    "today",
                    label_width = usize::from(label_width)
                ),
                dim(),
            ),
            Span::raw(" "),
            Span::styled(today_row.into_iter().collect::<String>(), accent()),
        ]),
    ];
    for r in &g.rows {
        let label = format!("{} {} / {}", r.wave, r.workload, r.environment);
        let label: String =
            label.chars().take(usize::from(label_width)).collect();
        let bar: String = r
            .cells
            .iter()
            .map(|c| if *c == Cell::Span { '█' } else { ' ' })
            .collect();
        lines.push(Line::from(vec![
            Span::raw(format!(
                "{label:<label_width$}",
                label_width = usize::from(label_width)
            )),
            Span::raw(" "),
            Span::styled(bar, if r.locked { accent() } else { dim() }),
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

fn draw_export_prompt(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.export_prompt.as_ref() else { return };
    let popup = popup_rect(frame, 76, 6);
    frame.render_widget(Clear, popup);
    let (what, formats) = match p.kind {
        crate::ExportKind::Workloads => ("workloads", ".csv or .json"),
        crate::ExportKind::Sizing => ("sizing", ".csv or .json"),
        crate::ExportKind::Schedule => ("schedule", ".ics, .csv or .json"),
    };
    let lines = vec![
        Line::from(vec![Span::raw("file: "), value(format!("{}_", p.text))]),
        Line::from(Span::styled(
            format!("The extension picks the format: {formats}."),
            dim(),
        )),
    ];
    let (hint, style) = if p.confirming_overwrite {
        (
            "that file exists; enter again to overwrite it, esc to keep it"
                .to_string(),
            warn(),
        )
    } else {
        ("type a path, enter to write   esc to cancel".to_string(), dim())
    };
    frame.render_widget(
        Paragraph::new(lines).block(popup_block(
            format!("Export the {what}"),
            hint,
            style,
        )),
        popup,
    );
}

fn draw_history(app: &mut App, frame: &mut Frame) {
    let Some(h) = app.history.as_mut() else { return };
    let popup = popup_rect(
        frame,
        110,
        u16::try_from(h.events.len()).unwrap_or(8).min(24) + 5,
    );
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = h
        .events
        .iter()
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{}  ", e.at.strftime("%Y-%m-%d %H:%M")),
                    dim(),
                ),
                Span::raw(e.event.describe()),
                Span::styled(
                    e.ends
                        .map(|d| format!("   → ends {d}"))
                        .unwrap_or_default(),
                    accent(),
                ),
            ]))
        })
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .block(popup_block(
                "Every change to the plan".into(),
                "each change is recorded and the schedule recomputed from it   esc to close".into(),
                dim(),
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut h.state,
    );
}

fn draw_weekday_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.weekday_picker.as_mut() else { return };
    let area = frame.area();
    let width = 44.min(area.width.saturating_sub(4));
    let height = u16::try_from(WEEKDAY_ROWS.len()).unwrap_or(10) + 5;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = WEEKDAY_ROWS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let on = match i {
                0 => p.days.is_empty(),
                1 => p.days == ALL_WEEKDAYS[..5],
                2 => p.days == ALL_WEEKDAYS[5..],
                _ => p.days.contains(&ALL_WEEKDAYS[i - 3]),
            };
            let mark = if on { "[x] " } else { "[ ] " };
            let style = if on { accent() } else { Style::default() };
            let mut spans = vec![Span::styled(format!("{mark}{label}"), style)];
            if i == 2 {
                spans.push(Span::styled("   presets above, days below", dim()));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(accent())
                    .title(Span::styled(
                        " Days changes may run ",
                        accent().add_modifier(Modifier::BOLD),
                    ))
                    .title_bottom(Span::styled(
                        " space to toggle   enter to save ",
                        dim(),
                    ))
                    .padding(Padding::new(1, 1, 1, 0)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

fn draw_picker_popup(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.picker.as_mut() else { return };
    let area = frame.area();
    // Wide enough for the longest help line plus the name column, or the
    // title, whichever is longer, within the terminal.
    let title = match p.mode {
        crate::PickerMode::Pair => {
            format!(" Disposition for {} / {} ", p.workload, p.environment)
        }
        crate::PickerMode::Default { apply: false } => {
            " Default disposition for new pairs ".to_string()
        }
        crate::PickerMode::Default { apply: true } => {
            " Default disposition, applied to untouched pairs ".to_string()
        }
    };
    let longest_help = DISPOSITIONS
        .iter()
        .map(|d| 12 + disposition_help(*d).chars().count())
        .max()
        .unwrap_or(40);
    let wanted = longest_help.max(title.chars().count()) + 6;
    let width = u16::try_from(wanted)
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(4));
    let height = u16::try_from(DISPOSITIONS.len()).unwrap_or(7) + 6;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    // Sized dispositions first, a rule, then the rest. The rule is a list
    // row too, so the selected index is mapped around it.
    let sized_count = DISPOSITIONS.iter().filter(|d| d.is_sized()).count();
    let rule_width = usize::from(width.saturating_sub(4));
    let mut items: Vec<ListItem> = Vec::new();
    for (i, d) in DISPOSITIONS.iter().enumerate() {
        if i == sized_count {
            let label = " not sized for Oxide ";
            let dashes = rule_width.saturating_sub(label.chars().count());
            items.push(ListItem::new(Line::from(Span::styled(
                format!(
                    "{}{label}{}",
                    "─".repeat(dashes / 2),
                    "─".repeat(dashes - dashes / 2)
                ),
                dim(),
            ))));
        }
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("{d:<12}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(disposition_help(*d), dim()),
        ])));
    }
    let mut render_state = ratatui::widgets::ListState::default();
    render_state.select(
        p.state.selected().map(|i| if i >= sized_count { i + 1 } else { i }),
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(accent())
                    .title(Span::styled(
                        title,
                        accent().add_modifier(Modifier::BOLD),
                    ))
                    .title_bottom(Span::styled(
                        " enter to set   esc to cancel ",
                        dim(),
                    ))
                    .padding(Padding::new(1, 1, 1, 0)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut render_state,
    );
}

fn draw_header(app: &App, frame: &mut Frame, area: Rect) {
    let [banner, context] = Layout::horizontal([
        Constraint::Length(logo::width(logo::PRECIP) + 2),
        Constraint::Min(30),
    ])
    .areas(area);
    let [art, caption] = Layout::vertical([
        Constraint::Length(logo::height(logo::PRECIP)),
        Constraint::Length(1),
    ])
    .areas(banner.inner(Margin::new(1, 1)));
    frame.render_widget(logo::Banner::precip(), art);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                logo::NAME,
                Style::default()
                    .fg(logo::OX_OFF_WHITE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", logo::TAGLINE), dim()),
        ])),
        caption,
    );

    let latest = app.scans.first();
    let kv = |k: &str, v: String| {
        Line::from(vec![Span::styled(format!("{k:<10}"), dim()), Span::raw(v)])
    };
    let mut lines = vec![
        kv("workspace", app.context.workspace.clone()),
        kv(
            "profile",
            app.context
                .profile
                .clone()
                .unwrap_or_else(|| "default chain".into()),
        ),
        kv(
            "last scan",
            latest
                .map(|s| {
                    format!(
                        "#{} on {}",
                        s.seq,
                        s.started.strftime("%Y-%m-%d %H:%M UTC")
                    )
                })
                .unwrap_or_else(|| "none".into()),
        ),
    ];
    if let Some(s) = latest {
        lines.push(kv(
            "caller",
            s.caller_arn
                .rsplit('/')
                .next()
                .unwrap_or(&s.caller_arn)
                .to_string(),
        ));
    }
    let status = match &app.progress {
        Some(p) if p.running() => Span::styled(
            format!("scanning  {}s", p.started.elapsed().as_secs()),
            accent().add_modifier(Modifier::BOLD),
        ),
        _ => Span::styled("idle", dim()),
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{:<10}", "status"), dim()),
        status,
    ]));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().padding(Padding::new(2, 0, 1, 0))),
        context,
    );
}

fn draw_tabs(app: &App, frame: &mut Frame, area: Rect) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            Line::from(vec![
                Span::styled(format!(" [{}] ", i + 1), dim()),
                Span::raw(format!("{} ", t.title())),
            ])
        })
        .collect();
    let selected = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .style(dim())
            .highlight_style(
                accent().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
            .divider(Span::styled("│", dim())),
        area,
    );
}

fn stat_card<'a>(title: &'a str, value: String, note: &str) -> Paragraph<'a> {
    Paragraph::new(vec![
        Line::from(Span::styled(
            value,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(note.to_string(), dim())),
    ])
    .block(panel(title).padding(Padding::horizontal(1)))
}

fn draw_overview(app: &App, frame: &mut Frame, area: Rect) {
    let Some(o) = &app.overview else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from("  No scans in this database yet."),
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Press "),
                    Span::styled("s", accent().add_modifier(Modifier::BOLD)),
                    Span::raw(" to scan the estate the current credentials can reach."),
                ]),
                Line::from(Span::styled(
                    "  In an Organization management account that is every active member account.",
                    dim(),
                )),
            ])
            .block(panel("Overview")),
            area,
        );
        return;
    };
    let count = |k: &str| o.by_kind.get(k).copied().unwrap_or(0);
    let managed = count("managed_service");
    let [cards, breakdown] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(4)])
            .areas(area);
    let card_areas =
        Layout::horizontal([Constraint::Ratio(1, 5); 5]).split(cards);
    let cards = [
        ("Accounts", o.accounts.to_string(), format!("{} regions", o.regions)),
        (
            "Instances",
            count("ec2_instance").to_string(),
            format!("{} ASGs", count("auto_scaling_group")),
        ),
        (
            "Volumes",
            count("ebs_volume").to_string(),
            format!("{} images", count("ami")),
        ),
        (
            "Networking",
            count("vpc").to_string(),
            format!("VPCs, {} security groups", count("security_group")),
        ),
        ("Managed", managed.to_string(), "no Oxide equivalent".to_string()),
    ];
    for (i, (title, value, note)) in cards.into_iter().enumerate() {
        frame.render_widget(stat_card(title, value, &note), card_areas[i]);
    }

    let rows: Vec<Row> = o
        .by_kind
        .iter()
        .map(|(k, n)| Row::new(vec![k.replace('_', " "), n.to_string()]))
        .collect();
    frame.render_widget(
        Table::new(rows, [Constraint::Length(28), Constraint::Length(10)])
            .header(Row::new(["resource kind", "count"]).style(dim()))
            .block(
                panel("Latest scan by kind").padding(Padding::horizontal(1)),
            ),
        breakdown,
    );
}

fn draw_scans(app: &mut App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .scans
        .iter()
        .map(|s| {
            let duration = match s.finished {
                Some(f) => format!("{}s", (f - s.started).get_seconds()),
                None => "in progress".to_string(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("#{:<4}", s.seq), accent()),
                Span::raw(s.started.strftime("%Y-%m-%d %H:%M UTC").to_string()),
                Span::styled(format!("  {duration:>8}  "), dim()),
                Span::styled(s.caller_arn.clone(), dim()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(panel("Scans").padding(Padding::horizontal(1)))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut app.selected);
}

fn draw_accounts(app: &App, frame: &mut Frame, area: Rect) {
    let Some(p) = &app.progress else {
        frame.render_widget(
            Paragraph::new(
                "  Account progress appears here while a scan runs.",
            )
            .style(dim())
            .block(panel("Accounts")),
            area,
        );
        return;
    };
    let rows: Vec<Row> = p
        .accounts
        .iter()
        .map(|(id, a)| {
            let state = a.state();
            let style = if a.note.is_some() {
                Style::default().fg(logo::OX_YELLOW)
            } else if a.planned == 0 && a.skipped > 0 {
                dim()
            } else if a.finished() {
                accent()
            } else {
                Style::default()
            };
            Row::new(vec![
                match &a.name {
                    Some(n) => format!("{n} ({id})"),
                    None => app.account_label(id.as_str()),
                },
                format!("{}/{}", a.done, a.planned),
                a.skipped.to_string(),
                a.resources.to_string(),
                a.failed.to_string(),
                state,
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(44),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new([
            "account",
            "regions",
            "skipped",
            "resources",
            "failed",
            "state",
        ])
        .style(dim()),
    )
    .block(panel("Accounts").padding(Padding::horizontal(1)));
    frame.render_widget(table, area);
}

fn draw_activity(app: &App, frame: &mut Frame, area: Rect) {
    let (gauge_area, feed_area) = match &app.progress {
        Some(p) if p.running() => {
            let [g, f] =
                Layout::vertical([Constraint::Length(3), Constraint::Min(3)])
                    .areas(area);
            (Some((g, p)), f)
        }
        _ => (None, area),
    };
    if let Some((g, p)) = gauge_area {
        let planned = p.regions_planned();
        let done = p.regions_done();
        let ratio =
            if planned == 0 { 0.0 } else { done as f64 / planned as f64 };
        frame.render_widget(
            Gauge::default()
                .block(panel("Progress"))
                .gauge_style(accent())
                .ratio(ratio.clamp(0.0, 1.0))
                .label(format!(
                    "{done}/{planned} regions   {}/{} accounts",
                    p.accounts_done(),
                    p.accounts_total
                )),
            g,
        );
    }
    let lines: Vec<Line> = app
        .activity
        .iter()
        .take(feed_area.height.saturating_sub(2) as usize)
        .map(|a| {
            Line::from(vec![
                Span::styled(
                    format!(
                        "{:>5}s ",
                        a.at.duration_since(app.opened).as_secs()
                    ),
                    dim(),
                ),
                Span::raw(a.text.clone()),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Activity").padding(Padding::horizontal(1))),
        feed_area,
    );
}

fn draw_hints(app: &App, frame: &mut Frame, area: Rect) {
    let key = |k: &str, what: &str| {
        vec![
            Span::styled(
                format!(" {k} "),
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{what}  "), dim()),
        ]
    };
    let mut spans = Vec::new();
    spans.extend(key(
        "s",
        if app.progress.as_ref().is_some_and(|p| p.running()) {
            "scan running"
        } else {
            "scan"
        },
    ));
    spans.extend(key("1-6 ← →", "switch view"));
    if app.tab == Tab::Schedule {
        spans.extend(key("enter", "what can I do with this row"));
        spans.extend(key("r", "rules"));
        spans.extend(key("h", "history"));
        spans.extend(key(
            "g",
            if app.schedule.as_ref().is_some_and(|v| v.gantt) {
                "table"
            } else {
                "gantt"
            },
        ));
        spans.extend(key("e", "export"));
    }
    if app.tab == Tab::Sizing {
        spans.extend(key("g/G", "growth"));
        spans.extend(key("t", "threshold"));
        spans.extend(key("n", "N+"));
        spans.extend(key("p", "profile"));
        spans.extend(key("e", "export"));
    }
    spans.extend(key("↑ ↓", "select"));
    if app.tab == Tab::Workloads {
        spans.extend(key("enter", "what can I do with this row"));
        spans.extend(key("click", "open a row"));
        if app.expanded.is_some() {
            spans.extend(key("esc", "close"));
        }
        spans.extend(key("e", "export"));
    }
    spans.extend(key("q", "quit"));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_picker(app: &mut App, frame: &mut Frame) {
    let Screen::Picker(p) = &mut app.screen else { return };
    let [header, body, hints] = Layout::vertical([
        Constraint::Length(10),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [art, caption] = Layout::vertical([
        Constraint::Length(logo::height(logo::PRECIP)),
        Constraint::Length(1),
    ])
    .areas(header.inner(Margin::new(1, 1)));
    frame.render_widget(logo::Banner::precip(), art);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                logo::NAME,
                Style::default()
                    .fg(logo::OX_OFF_WHITE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", logo::TAGLINE), dim()),
        ])),
        caption,
    );

    let [list_area, side] = Layout::horizontal([
        Constraint::Percentage(55),
        Constraint::Percentage(45),
    ])
    .areas(body);

    let mut items: Vec<ListItem> = p
        .entries
        .iter()
        .map(|e| {
            let when = e
                .modified
                .and_then(|m| m.elapsed().ok())
                .map(|d| {
                    let h = d.as_secs() / 3600;
                    if h < 1 {
                        "updated just now".to_string()
                    } else if h < 48 {
                        format!("updated {h}h ago")
                    } else {
                        format!("updated {}d ago", h / 24)
                    }
                })
                .unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<24}", e.name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(when, dim()),
            ]))
        })
        .collect();
    items.push(ListItem::new(Line::from(Span::styled(
        "+ create a new workspace",
        accent(),
    ))));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel("Workspaces").padding(Padding::horizontal(1)))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list_area,
        &mut p.state,
    );

    let mut lines = vec![
        Line::from("A workspace is one AWS estate under assessment:"),
        Line::from("its scans over time and the migration plan."),
        Line::from(""),
        Line::from(vec![
            Span::styled("directory  ", dim()),
            Span::raw(p.dir.0.to_string()),
        ]),
        Line::from(""),
    ];
    match &p.new_name {
        Some(name) => {
            lines.push(Line::from(vec![
                Span::styled("new name   ", dim()),
                Span::raw(name.clone()),
                Span::styled("▏", accent()),
            ]));
            lines.push(Line::from(Span::styled(
                "letters, digits, - and _   enter to create, esc to cancel",
                dim(),
            )));
        }
        None => lines.push(Line::from(Span::styled(
            "enter to open the selected workspace, n to create one",
            dim(),
        ))),
    }
    if let Some(err) = &p.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err.clone(),
            Style::default().fg(logo::OX_YELLOW),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Choose a workspace").padding(Padding::horizontal(1))),
        side,
    );

    let key = |k: &str, what: &str| {
        vec![
            Span::styled(
                format!(" {k} "),
                accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{what}  "), dim()),
        ]
    };
    let mut spans = Vec::new();
    spans.extend(key("enter", "open"));
    spans.extend(key("n", "new"));
    spans.extend(key("↑ ↓", "select"));
    spans.extend(key("q", "quit"));
    frame.render_widget(Paragraph::new(Line::from(spans)), hints);
}

fn draw_workloads(app: &mut App, frame: &mut Frame, area: Rect) {
    if app.expanded.is_some() {
        let [list, detail] = Layout::vertical([
            Constraint::Percentage(40),
            Constraint::Percentage(60),
        ])
        .areas(area);
        draw_workload_table(app, frame, list);
        draw_expanded(app, frame, detail);
        return;
    }
    draw_workload_table(app, frame, area);
}

fn draw_expanded(app: &mut App, frame: &mut Frame, area: Rect) {
    let Some(x) = app.expanded.as_mut() else { return };
    let history_lines =
        u16::try_from(x.history.len().clamp(1, 4)).unwrap_or(1) + 2;
    let [area, history_area] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(history_lines),
    ])
    .areas(area);

    let lines: Vec<Line> = x
        .history
        .iter()
        .rev()
        .take(4)
        .map(|c| {
            let change = match c.previous {
                Some(prev) => {
                    format!("{prev} → {}", c.disposition)
                }
                None => c.disposition.to_string(),
            };
            let mut spans = vec![
                Span::styled(
                    c.changed_at.strftime("%Y-%m-%d %H:%M ").to_string(),
                    dim(),
                ),
                Span::raw(change),
                Span::styled(format!("  {}", c.source), dim()),
            ];
            if let Some(n) = &c.note {
                spans.push(Span::styled(format!("  {n}"), dim()));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            panel("Disposition history   enter to change")
                .padding(Padding::horizontal(1)),
        ),
        history_area,
    );

    let title = format!(
        " {} / {}   {} resources   esc to close ",
        x.workload,
        x.environment,
        x.members.len()
    );
    let names = &app.account_names;
    let rows: Vec<Row> = x
        .members
        .iter()
        .map(|m| {
            let account = match names.get(&m.account_id) {
                Some(n) => n.clone(),
                None => m.account_id.clone(),
            };
            Row::new(vec![
                kind_label(&m.kind, m.service.as_deref()),
                m.name.clone(),
                account,
                m.region.clone(),
                m.signal.replace('_', " "),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(22),
            Constraint::Min(30),
            Constraint::Length(22),
            Constraint::Length(14),
            Constraint::Length(22),
        ],
    )
    .column_spacing(2)
    .header(
        Row::new(["kind", "name", "account", "region", "placed by"])
            .style(dim()),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(accent())
            .title(Span::styled(title, accent().add_modifier(Modifier::BOLD)))
            .padding(Padding::horizontal(1)),
    );
    frame.render_stateful_widget(table, area, &mut x.selected);
}

fn draw_workload_table(app: &mut App, frame: &mut Frame, area: Rect) {
    if app.workloads.is_empty() {
        frame.render_widget(
            Paragraph::new("  No workloads yet. Run a scan; grouping happens as it is stored.")
                .style(dim())
                .block(panel("Workloads")),
            area,
        );
        return;
    }
    let total: i64 = app.workloads.iter().map(|w| w.resources).sum();
    let title = format!(
        "Workloads  {} pairs, {} resources placed   · not sized",
        app.workloads.len(),
        total
    );
    let rows: Vec<Row> = app
        .workloads
        .iter()
        .map(|w| {
            let disposition = w.disposition.to_string();
            let style = if w.workload == "unassigned" {
                Style::default().fg(logo::OX_YELLOW)
            } else {
                Style::default()
            };
            // Only the disposition cell says whether the pair is sized.
            let disposition_cell = if w.disposition.is_sized() {
                ratatui::widgets::Cell::from(disposition)
            } else {
                ratatui::widgets::Cell::from(format!("{disposition} ·"))
                    .style(dim())
            };
            let compact = |v: Vec<String>| match v.len() {
                0 => String::new(),
                1 => v[0].clone(),
                n => format!("{} +{}", v[0], n - 1),
            };
            let accounts: Vec<String> =
                w.accounts.iter().map(|id| app.account_label(id)).collect();
            Row::new(vec![
                ratatui::widgets::Cell::from(w.workload.clone()),
                ratatui::widgets::Cell::from(w.environment.clone()),
                disposition_cell,
                ratatui::widgets::Cell::from(w.resources.to_string()),
                ratatui::widgets::Cell::from(compact(accounts)),
                ratatui::widgets::Cell::from(compact(w.regions.clone())),
            ])
            .style(style)
        })
        .collect();
    let widest = app
        .workloads
        .iter()
        .map(|w| w.workload.len())
        .max()
        .unwrap_or(8)
        .clamp(8, 48);
    let table = Table::new(
        rows,
        [
            Constraint::Length(u16::try_from(widest).unwrap_or(48)),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(40),
            Constraint::Length(18),
        ],
    )
    .column_spacing(2)
    .header(
        Row::new([
            "workload",
            "environment",
            "disposition",
            "resources",
            "account",
            "region",
        ])
        .style(dim()),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .block(panel(&title).padding(Padding::horizontal(1)));
    // Rows start below the border and the header line.
    app.workload_rows_area = Rect {
        x: area.x + 1,
        y: area.y + 2,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(3),
    };
    frame.render_stateful_widget(table, area, &mut app.workload_selected);
}

/// A readable resource kind. Managed services name their service.
fn kind_label(kind: &str, service: Option<&str>) -> String {
    match (kind, service) {
        ("managed_service", Some("lambda")) => "lambda function".into(),
        ("managed_service", Some("dynamodb")) => "dynamodb table".into(),
        ("managed_service", Some("rds")) => "rds database".into(),
        ("managed_service", Some("eks")) => "eks cluster".into(),
        ("managed_service", Some("ecs")) => "ecs cluster".into(),
        ("managed_service", Some("elasticache")) => {
            "elasticache cluster".into()
        }
        ("managed_service", Some("efs")) => "efs file system".into(),
        ("managed_service", Some(other)) => other.to_string(),
        ("ec2_instance", _) => "ec2 instance".into(),
        ("ebs_volume", _) => "ebs volume".into(),
        ("ami", _) => "ami".into(),
        ("vpc", _) => "vpc".into(),
        ("elastic_ip", _) => "elastic ip".into(),
        ("auto_scaling_group", _) => "auto scaling group".into(),
        ("cloud_formation_stack", _) => "cloudformation stack".into(),
        (other, _) => other.replace('_', " "),
    }
}

fn draw_sizing(app: &App, frame: &mut Frame, area: Rect) {
    let Some(v) = &app.sizing else {
        frame.render_widget(
            Paragraph::new("  Sizing appears once a scan has been stored.")
                .style(dim())
                .block(panel("Sizing")),
            area,
        );
        return;
    };
    let r = &v.result;
    let a = v.assumptions;
    let cap = r.sled_capacity;
    let [cards, assumptions, body] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Min(6),
    ])
    .areas(area);
    let card_areas =
        Layout::horizontal([Constraint::Ratio(1, 5); 5]).split(cards);
    let bound = match r.bound {
        precip_engine::sizing::Bound::Compute => "compute bound",
        precip_engine::sizing::Bound::Storage => "storage bound",
    };
    let cards = [
        (
            "Racks",
            r.racks.to_string(),
            format!("{} sleds, {} spare slots", r.sleds, r.spare_sleds),
        ),
        (
            "Sleds",
            format!(
                "{} + N+{}",
                r.sleds.saturating_sub(a.headroom * r.racks.max(1)),
                a.headroom
            ),
            bound.to_string(),
        ),
        (
            "Per sled usable",
            format!("{:.0} vCPU", cap.vcpus),
            format!(
                "{:.0} GiB, {:.1} TiB at {:.0}%",
                cap.memory_gib,
                cap.disk_tib,
                a.threshold * 100.0
            ),
        ),
        (
            "Growth",
            format!("x{:.1}", a.growth_factor),
            "on vCPU, memory and disk".to_string(),
        ),
        (
            "Profile",
            v.profile.name.clone(),
            format!(
                "release {}, {} sleds/rack",
                v.profile.oxide_release, v.profile.sleds_per_rack
            ),
        ),
    ];
    for (i, (title, value, note)) in cards.into_iter().enumerate() {
        frame.render_widget(stat_card(title, value, &note), card_areas[i]);
    }

    // What each key changes, with the current value and its effect.
    let key = |k: &str| {
        Span::styled(format!(" {k} "), accent().add_modifier(Modifier::BOLD))
    };
    let setting = |v: String| {
        Span::styled(
            format!("{v:<10}"),
            Style::default().add_modifier(Modifier::BOLD),
        )
    };
    let lines = vec![
        Line::from(vec![
            key("g/G"),
            Span::raw(" growth     "),
            setting(format!("x{:.1}", a.growth_factor)),
            Span::styled(
                "multiplies every instance's vCPU, memory and disk before packing; x1.3 sizes for 30% more than today",
                dim(),
            ),
        ]),
        Line::from(vec![
            key(" t "),
            Span::raw(" threshold  "),
            setting(format!("{:.0}%", a.threshold * 100.0)),
            Span::styled(
                format!(
                    "share of each sled the packer may fill; the rest ({:.0}%) stays free for placement, maintenance and growth (RFD 70 default 75%)",
                    (1.0 - a.threshold) * 100.0
                ),
                dim(),
            ),
        ]),
        Line::from(vec![
            key(" n "),
            Span::raw(" headroom   "),
            setting(format!("N+{}", a.headroom)),
            Span::styled(
                format!(
                    "extra sleds per rack beyond demand, so {} can fail or be serviced without crossing the threshold",
                    match a.headroom {
                        0 => "none".to_string(),
                        1 => "one sled".to_string(),
                        n => format!("{n} sleds"),
                    }
                ),
                dim(),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Assumptions").padding(Padding::horizontal(1))),
        assumptions,
    );

    let [table_area, findings_area] = Layout::vertical([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ])
    .areas(body);
    let widest = v
        .demand
        .iter()
        .map(|d| d.pair.workload.as_str().len())
        .max()
        .unwrap_or(8)
        .clamp(8, 48);
    let rows: Vec<Row> = v
        .demand
        .iter()
        .map(|d| {
            Row::new(vec![
                d.pair.workload.to_string(),
                d.pair.environment.to_string(),
                d.instances.to_string(),
                d.vcpus.to_string(),
                d.memory_gib.to_string(),
                d.disk_gib.to_string(),
                d.clean.to_string(),
                d.remediate.to_string(),
                d.blocked.to_string(),
            ])
            .style(if d.blocked > 0 {
                Style::default().fg(logo::OX_YELLOW)
            } else {
                Style::default()
            })
        })
        .collect();
    let totals = v.demand.iter().fold((0usize, 0u64, 0u64, 0u64), |t, d| {
        (t.0 + d.instances, t.1 + d.vcpus, t.2 + d.memory_gib, t.3 + d.disk_gib)
    });
    let title = format!(
        "Demand   {} instances, {} vCPU, {} GiB memory, {} GiB disk   (rehost, relocate, replatform)",
        totals.0, totals.1, totals.2, totals.3
    );
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(u16::try_from(widest).unwrap_or(48)),
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(9),
                Constraint::Length(11),
                Constraint::Length(13),
                Constraint::Length(12),
            ],
        )
        .column_spacing(2)
        .header(
            Row::new([
                "workload",
                "env",
                "inst",
                "vCPU",
                "GiB",
                "disk GiB",
                "fits as-is",
                "needs changes",
                "cannot move",
            ])
            .style(dim()),
        )
        .block(panel(&title).padding(Padding::horizontal(1))),
        table_area,
    );

    let lines: Vec<Line> = v
        .instances
        .iter()
        .filter(|i| i.fit != precip_engine::sizing::Fit::CleanFit)
        .map(|i| {
            let tag = match i.fit {
                precip_engine::sizing::Fit::CannotMove => Span::styled(
                    "cannot move    ",
                    Style::default().fg(logo::OX_YELLOW),
                ),
                precip_engine::sizing::Fit::NeedsRemediation
                | precip_engine::sizing::Fit::CleanFit => {
                    Span::styled("needs changes  ", accent())
                }
            };
            Line::from(vec![
                tag,
                Span::raw(format!("{} ({}) ", i.name, i.instance_type)),
                Span::styled(i.findings.join("; "), dim()),
            ])
        })
        .collect();
    let count = lines.len();
    frame.render_widget(
        Paragraph::new(lines).block(
            panel(&format!("Findings   {count} instances need changes or cannot move as they are"))
                .padding(Padding::horizontal(1)),
        ),
        findings_area,
    );
}

fn draw_schedule(app: &mut App, frame: &mut Frame, area: Rect) {
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
    let weekdays = weekday_rule_label(&r.weekdays);
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
    let confirmed = s.slots.iter().filter(|x| x.locked).count();
    let rows: Vec<Row> = s
        .slots
        .iter()
        .map(|x| {
            let state = if x.locked {
                "confirmed"
            } else if x.reason.starts_with("rescheduled") {
                "rescheduled"
            } else {
                "proposed"
            };
            let alts: Vec<String> =
                x.alternatives.iter().map(|d| d.to_string()).collect();
            let style = if x.locked {
                accent()
            } else if r.target_end.is_some_and(|t| x.date > t) {
                Style::default().fg(logo::OX_YELLOW)
            } else {
                Style::default()
            };
            let days = (x.end - x.date).get_days() + 1;
            Row::new(vec![
                x.wave.to_string(),
                x.date.to_string(),
                precip_db::weekday_name(x.date.weekday()).to_string(),
                if x.weight > 1 {
                    format!("{days}x{}", x.weight)
                } else {
                    days.to_string()
                },
                x.workload.clone(),
                x.environment.clone(),
                state.to_string(),
                alts.join(" or "),
                x.reason.clone(),
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
