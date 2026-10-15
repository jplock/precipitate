//! The Workloads tab.

use super::{accent, dim, panel};
use crate::{App, logo};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Row, Table};

pub(crate) fn draw_workloads(app: &mut App, frame: &mut Frame, area: Rect) {
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

pub(crate) fn draw_expanded(app: &mut App, frame: &mut Frame, area: Rect) {
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

pub(crate) fn draw_workload_table(
    app: &mut App,
    frame: &mut Frame,
    area: Rect,
) {
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
pub(crate) fn kind_label(kind: &str, service: Option<&str>) -> String {
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
