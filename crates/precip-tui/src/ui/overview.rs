//! The workspace picker and the Overview, Scans and Accounts tabs.

use super::{accent, dim, panel, stat_card};
use crate::{App, Screen, logo};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, Padding, Paragraph, Row, Table};

pub(crate) fn draw_overview(app: &App, frame: &mut Frame, area: Rect) {
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
                    "  From an AWS Organization management account, every active member account; in Azure, every readable subscription; in Google Cloud, every active project; in OCI, every readable compartment.",
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
            count("instance").to_string(),
            format!("{} scale groups", count("scale_group")),
        ),
        (
            "Volumes",
            count("volume").to_string(),
            format!("{} images", count("image")),
        ),
        (
            "Networking",
            count("network").to_string(),
            format!("networks, {} security groups", count("security_group")),
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

pub(crate) fn draw_scans(app: &mut App, frame: &mut Frame, area: Rect) {
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
                Span::styled(format!("{:<6}", s.provider), accent()),
                Span::styled(s.caller.clone(), dim()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(panel("Scans").padding(Padding::horizontal(1)))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut app.selected);
}

pub(crate) fn draw_accounts(app: &App, frame: &mut Frame, area: Rect) {
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

pub(crate) fn draw_picker(app: &mut App, frame: &mut Frame) {
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
        Line::from(
            "A workspace is one company's cloud estate under assessment:",
        ),
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
