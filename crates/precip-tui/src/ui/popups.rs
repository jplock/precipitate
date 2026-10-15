//! Popups, drawn over the tab in stack order.

use super::{accent, dim, short_date, short_span, value, warn};
use crate::workloads::merge_preview;
use crate::{
    ActionMenu, App, DISPOSITIONS, DatePicker, DependsPicker,
    DispositionPicker, ExportPrompt, Fields, HistoryView, HolidayPicker,
    MergePrompt, PairEditor, PairRow, ReasonPicker, RuleRow, RulesEditor,
    WEEKDAY_ROWS, WeekdayPicker, disposition_help,
};
use precip_engine::schedule::{WeekdayPreset, Weekdays, day_name};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Padding, Paragraph,
};

pub(crate) fn draw_date_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.popups.get_mut::<DatePicker>() else { return };
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
                key_label(i),
                Span::styled(
                    format!("{d}  {}", day_name(d.weekday())),
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
        format!(" Re-confirm {} ", p.pair)
    } else {
        format!(" Confirm {} ", p.pair)
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
                        " a number or enter to confirm   esc to cancel ",
                        dim(),
                    ))
                    .padding(Padding::new(1, 1, 1, 0)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

/// The number key that picks row `i`, as a label: `[1]` to `[9]`, `[0]`
/// for the tenth row, and blank beyond.
pub(crate) fn key_label(i: usize) -> Span<'static> {
    let text = match i {
        0..=8 => format!("[{}] ", i + 1),
        9 => "[0] ".to_string(),
        _ => "    ".to_string(),
    };
    Span::styled(text, accent())
}

/// A centred rectangle for a popup, clamped to the terminal.
pub(crate) fn popup_rect(frame: &Frame, width: u16, height: u16) -> Rect {
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

pub(crate) fn popup_block<'a>(
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

pub(crate) fn draw_action_menu(app: &mut App, frame: &mut Frame) {
    let Some(m) = app.popups.get_mut::<ActionMenu>() else { return };
    let popup = popup_rect(
        frame,
        96,
        u16::try_from(m.actions.len() * 2).unwrap_or(6) + 5,
    );
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = m
        .actions
        .iter()
        .enumerate()
        .map(|(i, a)| {
            ListItem::new(vec![
                Line::from(vec![
                    key_label(i),
                    Span::styled(
                        a.label(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(Span::styled(format!("      {}", a.help()), dim())),
            ])
        })
        .collect();
    let (hint, style) = match m.confirming {
        Some(a) => (
            format!("{}? enter again to confirm, esc to go back", a.label()),
            warn(),
        ),
        None => {
            ("a number or enter to choose   esc to close".to_string(), dim())
        }
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
pub(crate) fn typed(
    input: &Option<Fields>,
    editing: bool,
    shown: String,
) -> String {
    match input {
        Some(f) if editing => format!("{}_", f.get(0)),
        _ => shown,
    }
}

/// The boxes of a form: label, value, a cursor in the active one.
pub(crate) fn form_spans(f: &Fields) -> Vec<Span<'static>> {
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

/// A label in a fixed column, then the value.
pub(crate) fn field(label: &str, value_text: String) -> Line<'static> {
    Line::from(vec![Span::raw(format!("  {label:<18}")), value(value_text)])
}

/// An "add" row: short until it is being edited, then its boxes.
pub(crate) fn add_row(
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

pub(crate) fn header_row(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        dim().add_modifier(Modifier::BOLD),
    ))
}

/// The hint for the selected row, in one place, instead of on every row.
pub(crate) fn editor_hint(
    what: &str,
    input: bool,
    confirming: bool,
) -> (String, Style) {
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

pub(crate) fn draw_rules_editor(app: &mut App, frame: &mut Frame) {
    let Some(ed) = app.popups.get_mut::<RulesEditor>() else { return };
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
                RuleRow::Weekdays => field("Days", r.weekdays.to_string()),
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
                        Some(pair) => format!("   only {pair}"),
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

pub(crate) fn draw_pair_editor(app: &mut App, frame: &mut Frame) {
    let Some(ed) = app.popups.get_mut::<PairEditor>() else { return };
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
                    match &p.weekdays {
                        None => "as the plan allows".into(),
                        Some(days) => days.to_string(),
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
                    field("Days in a row", p.duration.to_string())
                }
                PairRow::Weight => {
                    field("Counts as", format!("{} migration(s)", p.weight))
                }
                PairRow::Priority => field("Priority", p.priority.to_string()),
                PairRow::Dependency(i) => Line::from(vec![
                    Span::raw("  "),
                    value(p.depends_on[i].to_string()),
                ]),
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
            .block(popup_block(p.key.to_string(), hint, style))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut ed.state,
    );
}

pub(crate) fn draw_holiday_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.popups.get_mut::<HolidayPicker>() else { return };
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
        .enumerate()
        .map(|(i, (h, on))| {
            let mark = if *on { "[x] " } else { "[ ] " };
            ListItem::new(Line::from(vec![
                key_label(i),
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
                "space or a number to tick   enter to add the ticked ones   esc to cancel".into(),
                dim(),
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

pub(crate) fn draw_reason_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.popups.get_mut::<ReasonPicker>() else { return };
    let popup = popup_rect(
        frame,
        64,
        u16::try_from(crate::ReasonPicker::rows()).unwrap_or(6) + 5,
    );
    frame.render_widget(Clear, popup);
    let mut items: Vec<ListItem> = precip_db::RESCHEDULE_REASONS
        .iter()
        .enumerate()
        .map(|(i, r)| {
            ListItem::new(Line::from(vec![key_label(i), Span::raw(*r)]))
        })
        .collect();
    let mut other = vec![key_label(items.len())];
    other.extend(match &p.custom {
        Some(text) => vec![Span::raw("other: "), value(format!("{text}_"))],
        None => vec![Span::styled("other, typed in", dim())],
    });
    items.push(ListItem::new(Line::from(other)));
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
        "a number or enter to choose   esc to cancel"
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

pub(crate) fn draw_depends_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.popups.get_mut::<DependsPicker>() else { return };
    let popup = popup_rect(
        frame,
        60,
        u16::try_from(p.candidates.len()).unwrap_or(8).min(20) + 5,
    );
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = p
        .candidates
        .iter()
        .enumerate()
        .map(|(i, pair)| {
            ListItem::new(Line::from(vec![
                key_label(i),
                Span::raw(pair.to_string()),
            ]))
        })
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .block(popup_block(
                "Wait for which pair?".into(),
                "a number or enter to choose   esc to cancel".into(),
                dim(),
            ))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

pub(crate) fn draw_merge_prompt(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.popups.get::<MergePrompt>() else { return };
    let into = p.text.trim();
    let landing = merge_preview(&app.workloads, &p.alias, into);
    let mut lines = vec![
        Line::from(vec![
            Span::raw(format!("{} is really part of ", p.alias)),
            value(format!("{}_", p.text)),
        ]),
        Line::from(""),
    ];
    // Each environment keeps its name; only the workload changes.
    for (env, exists) in &landing {
        let target = if into.is_empty() { "?" } else { into };
        let mut spans = vec![
            Span::raw(format!("  {env:<12}")),
            Span::styled("→ ", dim()),
            Span::raw(format!("{target} / {env}")),
        ];
        if !into.is_empty() {
            spans.push(Span::styled(
                if *exists {
                    "   joins the existing pair"
                } else {
                    "   becomes a new pair"
                },
                dim(),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Resources move when the scan is regrouped and keep their environment.",
        dim(),
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "The {} pairs' dispositions and migration settings stay behind.",
            p.alias
        ),
        dim(),
    )));
    let height = u16::try_from(landing.len()).unwrap_or(4) + 9;
    let popup = popup_rect(frame, 80, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(popup_block(
            "Fold into another workload".into(),
            "type the workload name, enter to save   esc to cancel".into(),
            dim(),
        )),
        popup,
    );
}

pub(crate) fn draw_export_prompt(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.popups.get::<ExportPrompt>() else { return };
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

pub(crate) fn draw_history(app: &mut App, frame: &mut Frame) {
    let Some(h) = app.popups.get_mut::<HistoryView>() else { return };
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

pub(crate) fn draw_weekday_picker(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.popups.get_mut::<WeekdayPicker>() else { return };
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
                0 => p.days.preset() == Some(WeekdayPreset::Any),
                1 => p.days.preset() == Some(WeekdayPreset::Weekdays),
                2 => p.days.preset() == Some(WeekdayPreset::Weekends),
                _ => Weekdays::EVERY_DAY
                    .get(i - 3)
                    .is_some_and(|d| p.days.contains(*d)),
            };
            let mark = if on { "[x] " } else { "[ ] " };
            let style = if on { accent() } else { Style::default() };
            let mut spans = vec![
                key_label(i),
                Span::styled(format!("{mark}{label}"), style),
            ];
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
                        " space or a number to toggle   enter to save ",
                        dim(),
                    ))
                    .padding(Padding::new(1, 1, 1, 0)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut p.state,
    );
}

pub(crate) fn draw_picker_popup(app: &mut App, frame: &mut Frame) {
    let Some(p) = app.popups.get_mut::<DispositionPicker>() else { return };
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
        .map(|d| 16 + disposition_help(*d).chars().count())
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
            key_label(i),
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
                        " a number or enter to set   esc to cancel ",
                        dim(),
                    ))
                    .padding(Padding::new(1, 1, 1, 0)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        popup,
        &mut render_state,
    );
}
