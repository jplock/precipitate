//! Files for the people who do not open the tool: CSV for spreadsheets,
//! JSON for other programs, and an iCalendar feed of the migration dates.

use precip_engine::schedule::Schedule;
use precip_engine::sizing::inputs::SizedInstance;
use precip_engine::sizing::{Assumptions, Sizing, WorkloadDemand};
use precip_types::Profile;

use crate::WorkloadSummary;

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

fn csv_row(fields: &[String]) -> String {
    fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",")
}

fn csv(header: &[&str], rows: Vec<Vec<String>>) -> String {
    let mut out = String::new();
    out.push_str(&header.join(","));
    out.push('\n');
    for r in rows {
        out.push_str(&csv_row(&r));
        out.push('\n');
    }
    out
}

pub fn workloads_csv(rows: &[WorkloadSummary]) -> String {
    csv(
        &[
            "workload",
            "environment",
            "disposition",
            "resources",
            "accounts",
            "regions",
        ],
        rows.iter()
            .map(|w| {
                vec![
                    w.workload.clone(),
                    w.environment.clone(),
                    w.disposition.to_string(),
                    w.resources.to_string(),
                    w.accounts.join(" "),
                    w.regions.join(" "),
                ]
            })
            .collect(),
    )
}

pub fn workloads_json(
    rows: &[WorkloadSummary],
) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(rows)
}

/// One row per instance, with how it fits.
pub fn sizing_csv(instances: &[SizedInstance]) -> String {
    csv(
        &[
            "workload",
            "environment",
            "disposition",
            "name",
            "instance_type",
            "architecture",
            "vcpus",
            "memory_gib",
            "disk_gib",
            "volumes",
            "fit",
            "account_id",
            "region",
            "arn",
        ],
        instances
            .iter()
            .map(|i| {
                vec![
                    i.pair.workload.to_string(),
                    i.pair.environment.to_string(),
                    i.disposition.to_string(),
                    i.name.clone(),
                    i.instance_type.clone(),
                    i.architecture.clone(),
                    i.vcpus.to_string(),
                    i.memory_gib.to_string(),
                    i.disk_gib.to_string(),
                    i.volumes.to_string(),
                    i.fit.to_string(),
                    i.account_id.to_string(),
                    i.region.to_string(),
                    i.arn.to_string(),
                ]
            })
            .collect(),
    )
}

#[derive(serde::Serialize)]
struct SizingReport<'a> {
    profile: &'a Profile,
    assumptions: &'a Assumptions,
    result: &'a Sizing,
    by_workload: &'a [WorkloadDemand],
    instances: &'a [SizedInstance],
}

pub fn sizing_json(
    profile: &Profile,
    assumptions: &Assumptions,
    result: &Sizing,
    by_workload: &[WorkloadDemand],
    instances: &[SizedInstance],
) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&SizingReport {
        profile,
        assumptions,
        result,
        by_workload,
        instances,
    })
}

pub fn schedule_csv(schedule: &Schedule) -> String {
    csv(
        &[
            "start",
            "end",
            "wave",
            "workload",
            "environment",
            "state",
            "reason",
            "alternatives",
        ],
        schedule
            .slots
            .iter()
            .map(|s| {
                vec![
                    s.date.to_string(),
                    s.end.to_string(),
                    s.wave.to_string(),
                    s.pair.workload.to_string(),
                    s.pair.environment.to_string(),
                    s.state_label().to_string(),
                    s.reason(),
                    s.alternatives
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" "),
                ]
            })
            .collect(),
    )
}

pub fn schedule_json(schedule: &Schedule) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(schedule)
}

/// An iCalendar feed: one all-day event per migration, agreed dates as
/// confirmed and proposed ones as tentative, so owners can subscribe.
pub fn schedule_ics(
    schedule: &Schedule,
    calendar_name: &str,
    stamp: jiff::Timestamp,
) -> String {
    let day = |d: jiff::civil::Date| d.strftime("%Y%m%d").to_string();
    let stamp = stamp.strftime("%Y%m%dT%H%M%SZ").to_string();
    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//precip//migration calendar//EN\r\n");
    out.push_str(&format!("X-WR-CALNAME:{}\r\n", ics_text(calendar_name)));
    for s in &schedule.slots {
        let after_end = s.end.tomorrow().unwrap_or(s.end);
        let state = if s.agreed { "agreed" } else { "proposed" };
        out.push_str("BEGIN:VEVENT\r\n");
        out.push_str(&format!(
            "UID:{}-{}@precip\r\n",
            uid_part(s.pair.workload.as_str()),
            uid_part(s.pair.environment.as_str())
        ));
        out.push_str(&format!("DTSTAMP:{stamp}\r\n"));
        out.push_str(&format!("DTSTART;VALUE=DATE:{}\r\n", day(s.date)));
        out.push_str(&format!("DTEND;VALUE=DATE:{}\r\n", day(after_end)));
        out.push_str(&format!(
            "SUMMARY:{}\r\n",
            ics_text(&format!("Migrate {} ({state})", s.pair))
        ));
        out.push_str(&format!(
            "DESCRIPTION:{}\r\n",
            ics_text(&format!("Wave {}. {}.", s.wave, s.reason()))
        ));
        out.push_str(if s.agreed {
            "STATUS:CONFIRMED\r\n"
        } else {
            "STATUS:TENTATIVE\r\n"
        });
        out.push_str("END:VEVENT\r\n");
    }
    out.push_str("END:VCALENDAR\r\n");
    out
}

fn ics_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
}

/// A name made safe for an iCalendar UID. Every byte other than an
/// ASCII letter or digit is percent-encoded, so distinct names never
/// collide and the `-` between the parts stays unambiguous.
fn uid_part(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;
    use precip_engine::schedule::Slot;

    #[test]
    fn uids_differ_for_names_that_differ_only_in_punctuation() {
        assert_eq!(uid_part("shop"), "shop");
        assert_ne!(uid_part("a.b"), uid_part("a-b"));
        assert_eq!(uid_part("a-b"), "a%2Db");
    }

    #[test]
    fn csv_quotes_only_what_needs_it() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("has, comma"), "\"has, comma\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn ics_has_one_all_day_event_per_slot() {
        let schedule = Schedule {
            slots: vec![Slot {
                pair: precip_types::PairKey::new("shop", "prod"),
                wave: 1,
                date: date(2026, 11, 7),
                end: date(2026, 11, 8),
                weight: 1,
                agreed: true,
                placement: precip_engine::schedule::Placement::Agreed,
                reopened: None,
                alternatives: Vec::new(),
            }],
            ..Default::default()
        };
        let ics = schedule_ics(
            &schedule,
            "acme",
            "2026-10-14T12:00:00Z".parse().unwrap(),
        );
        assert!(ics.contains("DTSTART;VALUE=DATE:20261107\r\n"));
        assert!(
            ics.contains("DTEND;VALUE=DATE:20261109\r\n"),
            "the day after the last day"
        );
        assert!(ics.contains("STATUS:CONFIRMED"));
        assert!(ics.contains("SUMMARY:Migrate shop / prod (agreed)"));
        assert_eq!(ics.matches("BEGIN:VEVENT").count(), 1);
    }
}
