//! The fixed strings the store writes into text columns, as enums. Each
//! keeps the string it has always stored, so existing workspaces read the
//! same.

use precip_types::UnknownName;
use std::fmt;
use std::str::FromStr;

precip_types::named_enum! {
    /// A key in the settings table.
    pub(crate) enum Setting {
        AwsScanOptions = "scan.options.aws",
        AzureScanOptions = "scan.options.azure",
        SizingProfile = "sizing.profile",
        DefaultDisposition = "default_disposition",
        PerDay = "schedule.per_day",
        PerWeek = "schedule.per_week",
        PerMonth = "schedule.per_month",
        Weekdays = "schedule.weekdays",
        TargetEnd = "schedule.target_end",
        NonprodBeforeProd = "schedule.nonprod_before_prod",
    }
}

precip_types::named_enum! {
    /// Who set a pair's disposition.
    pub enum DispositionSource {
        /// The default, given when a scan first sees the pair.
        Default = "default",
        /// A new default applied to every existing pair at once.
        Bulk = "bulk",
        /// A change to this one pair.
        User = "user",
    }
}

precip_types::named_enum! {
    /// The kind of a plan event, stored beside its body.
    pub enum EventKind {
        RulesSet = "rules_set",
        BlackoutAdded = "blackout_added",
        BlackoutRemoved = "blackout_removed",
        PairRuleSet = "pair_rule_set",
        PairPlanSet = "pair_plan_set",
        WaveSet = "wave_set",
        WaveRemoved = "wave_removed",
        DependencyAdded = "dependency_added",
        DependencyRemoved = "dependency_removed",
        StatusChanged = "status_changed",
        DispositionSet = "disposition_set",
        DefaultDispositionSet = "default_disposition_set",
        WorkloadMerged = "workload_merged",
    }
}

precip_types::named_enum! {
    /// One entry in a pair's migration history.
    pub enum MigrationEventKind {
        Confirmed = "confirmed",
        /// A second confirmation that replaced an agreed date.
        Reconfirmed = "reconfirmed",
        Unconfirmed = "unconfirmed",
        Missed = "missed",
        Migrated = "migrated",
        RolledBack = "rolled_back",
    }
}

impl MigrationEventKind {
    /// Whether an agreed date moved: it was missed or replaced.
    pub(crate) fn moved_a_date(self) -> bool {
        match self {
            Self::Missed | Self::Reconfirmed => true,
            Self::Confirmed
            | Self::Unconfirmed
            | Self::Migrated
            | Self::RolledBack => false,
        }
    }
}

/// Why a schedule version was computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A plan change, recorded in the event log.
    Event(EventKind),
    /// A command that shows the schedule.
    Command,
    /// The workspace was opened.
    Open,
    /// The plan was rebuilt from the event log.
    Replay,
    /// The schedule was exported.
    Export,
    /// A view refreshed after a status change.
    StatusChange,
    /// A view refreshed after a rule change.
    RuleChange,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Event(kind) => kind.as_str(),
            Self::Command => "command",
            Self::Open => "open",
            Self::Replay => "replay",
            Self::Export => "export",
            Self::StatusChange => "status_change",
            Self::RuleChange => "rule_change",
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromStr for Trigger {
    type Err = UnknownName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "command" => Self::Command,
            "open" => Self::Open,
            "replay" => Self::Replay,
            "export" => Self::Export,
            "status_change" => Self::StatusChange,
            "rule_change" => Self::RuleChange,
            other => Self::Event(other.parse().map_err(|_| UnknownName {
                kind: "Trigger",
                value: other.to_string(),
            })?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_round_trip_through_their_stored_names() {
        let all = [
            Trigger::Event(EventKind::WaveSet),
            Trigger::Command,
            Trigger::Open,
            Trigger::Replay,
            Trigger::Export,
            Trigger::StatusChange,
            Trigger::RuleChange,
        ];
        for t in all {
            assert_eq!(t.as_str().parse::<Trigger>(), Ok(t));
        }
        assert_eq!(Trigger::Event(EventKind::WaveSet).to_string(), "wave_set");
        assert!("nonsense".parse::<Trigger>().is_err());
    }

    #[test]
    fn only_misses_and_replacements_move_a_date() {
        let moved: Vec<_> = MigrationEventKind::ALL
            .iter()
            .filter(|k| k.moved_a_date())
            .collect();
        assert_eq!(
            moved,
            [&MigrationEventKind::Reconfirmed, &MigrationEventKind::Missed]
        );
    }
}
