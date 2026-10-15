//! The Sizing tab.

use super::{accent, dim, panel, stat_card};
use crate::{App, logo};
use ratatui::prelude::*;
use ratatui::widgets::{Padding, Paragraph, Row, Table};

pub(crate) fn draw_sizing(app: &App, frame: &mut Frame, area: Rect) {
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
                Span::styled(i.findings_text(), dim()),
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
