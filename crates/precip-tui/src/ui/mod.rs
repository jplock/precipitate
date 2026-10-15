//! Drawing. Layout: header (mark, name, context), tab bar, body, activity
//! feed, key hints.

mod overview;
mod popups;
mod schedule;
mod sizing;
mod workloads;

use self::overview::{draw_accounts, draw_overview, draw_picker, draw_scans};
use self::popups::{
    draw_action_menu, draw_date_picker, draw_depends_picker,
    draw_export_prompt, draw_history, draw_holiday_picker, draw_merge_prompt,
    draw_pair_editor, draw_picker_popup, draw_reason_picker, draw_rules_editor,
    draw_weekday_picker,
};
use self::schedule::draw_schedule;
use self::sizing::draw_sizing;
use self::workloads::draw_workloads;
use crate::popup::PopupTag;
use crate::{App, Screen, Tab, logo};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Gauge, Padding, Paragraph, Tabs};

pub(crate) const ACCENT: Color = logo::OX_GREEN_LIGHT;

pub(crate) const DIM: Color = logo::OX_GRAY_DARK;

pub(crate) fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn dim() -> Style {
    Style::default().fg(DIM)
}

pub(crate) fn panel(title: &str) -> Block<'_> {
    Block::default().borders(Borders::ALL).border_style(dim()).title(
        Span::styled(
            format!(" {title} "),
            accent().add_modifier(Modifier::BOLD),
        ),
    )
}

pub(crate) fn draw(app: &mut App, frame: &mut Frame) {
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
    // Popups in stack order, so the one with the keys is drawn last.
    for tag in app.popups.tags() {
        match tag {
            PopupTag::Disposition => draw_picker_popup(app, frame),
            PopupTag::Date => draw_date_picker(app, frame),
            PopupTag::History => draw_history(app, frame),
            PopupTag::Rules => draw_rules_editor(app, frame),
            PopupTag::Pair => draw_pair_editor(app, frame),
            PopupTag::Weekdays => draw_weekday_picker(app, frame),
            PopupTag::Depends => draw_depends_picker(app, frame),
            PopupTag::Holidays => draw_holiday_picker(app, frame),
            PopupTag::Merge => draw_merge_prompt(app, frame),
            PopupTag::Reason => draw_reason_picker(app, frame),
            PopupTag::Export => draw_export_prompt(app, frame),
            PopupTag::Actions => draw_action_menu(app, frame),
        }
    }
}

pub(crate) fn warn() -> Style {
    Style::default().fg(logo::OX_YELLOW).add_modifier(Modifier::BOLD)
}

pub(crate) fn value(text: String) -> Span<'static> {
    Span::styled(text, accent())
}

/// "Nov 26", or "Nov 26 2027" when the year is not this one.
pub(crate) fn short_date(
    d: jiff::civil::Date,
    today: jiff::civil::Date,
) -> String {
    if d.year() == today.year() {
        d.strftime("%b %-d").to_string()
    } else {
        d.strftime("%b %-d %Y").to_string()
    }
}

/// "Nov 26", or "Oct 26 – Oct 30" for a range.
pub(crate) fn short_span(
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

pub(crate) fn draw_header(app: &App, frame: &mut Frame, area: Rect) {
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
        kv("scans", app.context.target.clone()),
        kv(
            "last scan",
            latest
                .map(|s| {
                    format!(
                        "#{} {} on {}",
                        s.seq,
                        s.provider,
                        s.started.strftime("%Y-%m-%d %H:%M UTC")
                    )
                })
                .unwrap_or_else(|| "none".into()),
        ),
    ];
    if let Some(s) = latest {
        lines.push(kv(
            "caller",
            s.caller.rsplit('/').next().unwrap_or(&s.caller).to_string(),
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

pub(crate) fn draw_tabs(app: &App, frame: &mut Frame, area: Rect) {
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

pub(crate) fn stat_card<'a>(
    title: &'a str,
    value: String,
    note: &str,
) -> Paragraph<'a> {
    Paragraph::new(vec![
        Line::from(Span::styled(
            value,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(note.to_string(), dim())),
    ])
    .block(panel(title).padding(Padding::horizontal(1)))
}

pub(crate) fn draw_activity(app: &App, frame: &mut Frame, area: Rect) {
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

pub(crate) fn draw_hints(app: &App, frame: &mut Frame, area: Rect) {
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
