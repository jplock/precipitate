//! The command line: every subcommand and its arguments.

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use jiff::civil::Date;
use precip_engine::schedule::Weekdays;
use precip_engine::sizing::Assumptions;
use precip_types::{AccountId, Provider};

use crate::commands::default_workspaces_dir;

/// Scan AWS and Azure estates and size them for Oxide racks. With no
/// subcommand, opens the terminal interface.
#[derive(Parser)]
#[command(name = "precip", version)]
pub(crate) struct Cli {
    /// Directory holding workspaces, one SQLite file each.
    #[arg(long, global = true, default_value_t = default_workspaces_dir())]
    pub(crate) workspaces_dir: Utf8PathBuf,

    /// Workspace name to open, skipping the picker.
    #[arg(long, global = true, conflicts_with = "db")]
    pub(crate) workspace: Option<String>,

    /// Explicit database path instead of a named workspace.
    #[arg(long, global = true)]
    pub(crate) db: Option<Utf8PathBuf>,

    #[command(flatten)]
    pub(crate) scan: ScanArgs,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// Which cloud to scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Cloud {
    Aws,
    Azure,
}

impl From<Cloud> for Provider {
    fn from(c: Cloud) -> Self {
        match c {
            Cloud::Aws => Provider::Aws,
            Cloud::Azure => Provider::Azure,
        }
    }
}

/// Which Azure cloud to sign in to, named as `az cloud set` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum AzureCloudArg {
    #[value(name = "AzureCloud")]
    Public,
    #[value(name = "AzureUSGovernment")]
    UsGovernment,
}

impl From<AzureCloudArg> for precip_azure::AzureCloud {
    fn from(c: AzureCloudArg) -> Self {
        match c {
            AzureCloudArg::Public => Self::Public,
            AzureCloudArg::UsGovernment => Self::UsGovernment,
        }
    }
}

/// How to reach the estate. Accepted before or after a subcommand.
#[derive(Args, Clone)]
pub(crate) struct ScanArgs {
    /// The cloud to scan. Without it, the selection flags decide; with
    /// none, `scan` repeats every cloud's saved selection and the
    /// interface scans AWS.
    #[arg(long, global = true, value_enum)]
    pub(crate) cloud: Option<Cloud>,

    /// AWS CLI profile to use instead of the default credential chain.
    #[arg(long, global = true, help_heading = "AWS")]
    pub(crate) profile: Option<String>,
    /// Role to try in each member account, in order (repeatable).
    /// Defaults to OrganizationAccountAccessRole, then
    /// AWSControlTowerExecution.
    #[arg(long = "member-role", global = true, help_heading = "AWS")]
    pub(crate) member_roles: Vec<String>,
    /// Scan only these member account ids (repeatable).
    #[arg(long = "account", global = true, help_heading = "AWS")]
    pub(crate) accounts: Vec<AccountId>,
    /// Skip these member account ids (repeatable).
    #[arg(long = "exclude-account", global = true, help_heading = "AWS")]
    pub(crate) exclude_accounts: Vec<AccountId>,
    /// Scan only the caller's account, even in a management account.
    #[arg(long, global = true, help_heading = "AWS")]
    pub(crate) single_account: bool,

    /// Azure cloud to scan. Defaults to AzureCloud, the public cloud. With
    /// the Azure CLI, run `az cloud set` to the same cloud first.
    #[arg(long, global = true, value_enum, help_heading = "Azure")]
    pub(crate) azure_cloud: Option<AzureCloudArg>,
    /// Entra tenant to sign in to and scan.
    #[arg(long, global = true, help_heading = "Azure")]
    pub(crate) tenant: Option<String>,
    /// Scan only these subscription ids (repeatable).
    #[arg(long = "subscription", global = true, help_heading = "Azure")]
    pub(crate) subscriptions: Vec<AccountId>,
    /// Skip these subscription ids (repeatable).
    #[arg(
        long = "exclude-subscription",
        global = true,
        help_heading = "Azure"
    )]
    pub(crate) exclude_subscriptions: Vec<AccountId>,
    /// Scan only subscriptions under this management group, at any depth.
    #[arg(long, global = true, help_heading = "Azure")]
    pub(crate) management_group: Option<String>,
}

impl ScanArgs {
    fn aws_selection(&self) -> bool {
        !self.member_roles.is_empty()
            || !self.accounts.is_empty()
            || !self.exclude_accounts.is_empty()
            || self.single_account
    }

    fn azure_selection(&self) -> bool {
        self.azure_cloud.is_some()
            || self.tenant.is_some()
            || !self.subscriptions.is_empty()
            || !self.exclude_subscriptions.is_empty()
            || self.management_group.is_some()
    }

    /// The cloud the flags name: `--cloud`, else the one whose flags are
    /// given. `None` when nothing says.
    pub(crate) fn provider(&self) -> anyhow::Result<Option<Provider>> {
        let aws = self.aws_selection() || self.profile.is_some();
        let azure = self.azure_selection();
        match (self.cloud.map(Provider::from), aws, azure) {
            (Some(Provider::Aws), _, true) => {
                anyhow::bail!("Azure flags given with --cloud aws")
            }
            (Some(Provider::Azure), true, _) => {
                anyhow::bail!("AWS flags given with --cloud azure")
            }
            (None, true, true) => {
                anyhow::bail!(
                    "both AWS and Azure flags given; scan one cloud at a time"
                )
            }
            (Some(p), _, _) => Ok(Some(p)),
            (None, true, false) => Ok(Some(Provider::Aws)),
            (None, false, true) => Ok(Some(Provider::Azure)),
            (None, false, false) => Ok(None),
        }
    }

    /// Whether `provider` was given no selection flags, so its saved
    /// selection applies.
    pub(crate) fn is_default(&self, provider: Provider) -> bool {
        match provider {
            Provider::Aws => !self.aws_selection(),
            Provider::Azure => !self.azure_selection(),
        }
    }

    pub(crate) fn aws_options(&self) -> precip_aws::Options {
        let defaults = precip_aws::Options::default();
        precip_aws::Options {
            member_roles: if self.member_roles.is_empty() {
                defaults.member_roles
            } else {
                self.member_roles.clone()
            },
            accounts: self.accounts.clone(),
            exclude_accounts: self.exclude_accounts.clone(),
            single_account: self.single_account,
        }
    }

    pub(crate) fn azure_options(&self) -> precip_azure::Options {
        precip_azure::Options {
            cloud: self.azure_cloud.map(Into::into).unwrap_or_default(),
            tenant: self.tenant.clone(),
            subscriptions: self.subscriptions.clone(),
            exclude_subscriptions: self.exclude_subscriptions.clone(),
            management_group: self.management_group.clone(),
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Scan the estate the credentials can reach and append the result to
    /// the database, without the interface. In an AWS Organization
    /// management account this covers every active member account through
    /// the member role; in Azure, every subscription the sign-in can read.
    Scan,
    /// Regroup the latest scan of a workspace into workloads without
    /// rescanning, and print the result.
    Group,
    /// Record that one workload name is really another, then regroup.
    Merge {
        /// The name to fold in, for example vouch-us.
        alias: String,
        /// The workload it belongs to, for example vouch.
        into: String,
    },
    /// Set the disposition of a workload and environment pair.
    Disposition {
        #[command(flatten)]
        pair: PairArgs,
        /// retire, retain, rehost, relocate, repurchase, replatform, refactor
        disposition: precip_types::Disposition,
        /// Why, kept with the change.
        #[arg(long)]
        note: Option<String>,
    },
    /// Set the disposition new pairs start with; --apply also changes
    /// every pair nobody has set by hand.
    DefaultDisposition {
        disposition: precip_types::Disposition,
        #[arg(long)]
        apply: bool,
    },
    /// List workspaces, most recently changed first.
    Workspaces,
    /// Resource counts by kind, accounts and regions of the latest scan.
    Overview,
    /// Every scan of the workspace, newest first.
    Scans,
    /// Accounts seen in the latest scan.
    Accounts,
    /// Workload and environment pairs of the latest scan with their
    /// disposition, resource count, accounts and regions.
    Workloads,
    /// The resources of one workload and environment pair.
    Members {
        #[command(flatten)]
        pair: PairArgs,
    },
    /// Show every disposition change for a workload and environment pair.
    History {
        #[command(flatten)]
        pair: PairArgs,
    },
    /// Size the rehost and relocate workloads of the latest scan.
    Size {
        #[command(flatten)]
        sizing: SizingArgs,
    },
    /// Remove workload pairs that nothing in the latest scan belongs to.
    /// Pairs a user has set a disposition on are kept unless --all.
    Prune {
        #[arg(long)]
        all: bool,
    },
    /// Migration calendar: rules, blackouts, holidays, status and the
    /// computed schedule.
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    /// List built-in Oxide profiles, or choose the one the workspace
    /// sizes with.
    Profiles {
        /// Make this the workspace's profile.
        #[arg(long)]
        use_profile: Option<String>,
    },
    /// Write the workloads, the sizing or the schedule to a file, or to
    /// standard output. The format follows the file's extension (.csv,
    /// .json, .ics) or --format.
    Export {
        what: ExportWhat,
        #[arg(long)]
        out: Option<Utf8PathBuf>,
        /// csv, json, or ics (schedule only); needed without --out.
        #[arg(long)]
        format: Option<String>,
        #[command(flatten)]
        sizing: SizingArgs,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum ExportWhat {
    Workloads,
    Sizing,
    Schedule,
}

#[derive(Subcommand)]
pub(crate) enum ScheduleCommand {
    /// Compute and print the schedule from today.
    Show,
    /// Set global rules. Unset flags keep their value.
    Set {
        #[arg(long)]
        per_day: Option<u32>,
        #[arg(long)]
        per_week: Option<u32>,
        #[arg(long)]
        per_month: Option<u32>,
        /// "weekdays", "weekends", "any", or a comma list such as tue,thu.
        #[arg(long)]
        weekdays: Option<Weekdays>,
        /// YYYY-MM-DD; "none" clears it.
        #[arg(long, value_parser = parse_target_end)]
        target_end: Option<TargetEnd>,
        #[arg(long)]
        nonprod_before_prod: Option<bool>,
    },
    /// Add a freeze, or remove one with --remove. Dates are YYYY-MM-DD,
    /// inclusive; leave the end out for a single day.
    Blackout {
        name: String,
        #[arg(value_parser = parse_date)]
        start: Date,
        /// Defaults to the start: a one-day freeze.
        #[arg(value_parser = parse_date)]
        end: Option<Date>,
        /// Freeze only this workload; needs --environment.
        #[arg(long, requires = "environment")]
        workload: Option<String>,
        /// Freeze only this environment; needs --workload.
        #[arg(long, requires = "workload")]
        environment: Option<String>,
        #[arg(long)]
        remove: bool,
    },
    /// Freezes still ahead, soonest first.
    Blackouts,
    /// Add a country's public holidays as freezes for a year, one each.
    Holidays {
        /// Only "us" (federal) is built in.
        country: String,
        year: i16,
        /// Comma list of the only holidays to add, by name.
        #[arg(long)]
        only: Option<String>,
        /// Comma list of holidays to leave out, by name.
        #[arg(long)]
        skip: Option<String>,
    },
    /// List a country's public holidays for a year, to pick from.
    HolidayNames { country: String, year: i16 },
    /// Rules for one pair: allowed weekdays, earliest date, priority.
    Rule {
        #[command(flatten)]
        pair: PairArgs,
        /// "weekdays", "weekends", or a comma list such as tue,thu; "any"
        /// or leaving it out follows the plan's days.
        #[arg(long)]
        weekdays: Option<Weekdays>,
        #[arg(long, value_parser = parse_date)]
        earliest: Option<Date>,
        #[arg(long, default_value_t = 0)]
        priority: i32,
    },
    /// Which wave a pair is in, how many days its migration takes, and
    /// how many capacity units it uses on each of them.
    Plan {
        #[command(flatten)]
        pair: PairArgs,
        #[arg(long, default_value_t = 1)]
        wave: u32,
        #[arg(long, default_value_t = 1)]
        duration: u32,
        #[arg(long, default_value_t = 1)]
        weight: u32,
    },
    /// Name a wave and give it a target end date, or remove it; its
    /// pairs go back to wave 1.
    Wave {
        number: u32,
        #[arg(long, default_value = "")]
        name: String,
        /// YYYY-MM-DD
        #[arg(long, value_parser = parse_date)]
        target_end: Option<Date>,
        #[arg(long)]
        remove: bool,
    },
    /// Make a pair wait for another.
    Depends {
        #[command(flatten)]
        pair: PairArgs,
        on_workload: String,
        on_environment: String,
    },
    /// Confirm a date with the owners. It never moves; confirming
    /// another future date replaces it and counts as a reschedule.
    Confirm {
        #[command(flatten)]
        pair: PairArgs,
        #[arg(value_parser = parse_date)]
        date: Date,
        /// Why, when this replaces an agreed date. See `schedule reasons`.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Mark a pair migrated, today unless a date is given.
    Migrated {
        #[command(flatten)]
        pair: PairArgs,
        #[arg(long, value_parser = parse_date)]
        date: Option<Date>,
    },
    /// Mark a pair as missed; a new date is proposed.
    Missed {
        #[command(flatten)]
        pair: PairArgs,
        /// Why. See `schedule reasons` for the usual ones.
        #[arg(long)]
        reason: Option<String>,
    },
    /// The usual reasons an agreed date moves, for --reason.
    Reasons,
    /// A migrated pair moved back to the cloud; a new date is proposed.
    RolledBack {
        #[command(flatten)]
        pair: PairArgs,
        #[arg(long)]
        note: Option<String>,
    },
    /// Migrated pairs whose instances are still in the latest scan.
    Verify,
    /// Every change to the plan, oldest first, with the end date each one
    /// led to.
    History,
    /// Rebuild the rules, freezes, waves and statuses from the change log
    /// and compute the schedule once. Proves the log is the truth.
    Replay,
    /// The schedule as a Gantt chart, one row per migration.
    Gantt {
        /// Characters available for the bars.
        #[arg(long, default_value_t = 90)]
        width: usize,
    },
    /// Reschedule rate, projected end and drift, success and slip.
    Metrics,
}

/// One workload and environment pair, as two positional arguments.
#[derive(Args, Clone)]
pub(crate) struct PairArgs {
    pub(crate) workload: String,
    pub(crate) environment: String,
}

/// How to size: the profile and the assumptions.
#[derive(Args, Clone)]
pub(crate) struct SizingArgs {
    /// Built-in profile name, or a path to a profile TOML file.
    /// Defaults to the workspace's choice, else the built-in.
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// Multiplier on demand.
    #[arg(long, default_value_t = 1.0)]
    pub(crate) growth: f64,
    /// Share of sled capacity the packer may fill.
    #[arg(long, default_value_t = 0.75)]
    pub(crate) threshold: f64,
    /// Extra sleds per rack.
    #[arg(long, default_value_t = 1)]
    pub(crate) headroom: u32,
}

impl SizingArgs {
    pub(crate) fn assumptions(&self) -> Assumptions {
        Assumptions {
            growth_factor: self.growth,
            threshold: self.threshold,
            headroom: self.headroom,
        }
    }
}

/// A target end date, or `none` to clear it.
#[derive(Clone, Copy)]
pub(crate) struct TargetEnd(pub(crate) Option<Date>);

fn parse_date(s: &str) -> Result<Date, String> {
    s.parse().map_err(|_| format!("{s} is not a YYYY-MM-DD date"))
}

fn parse_target_end(s: &str) -> Result<TargetEnd, String> {
    if s == "none" {
        Ok(TargetEnd(None))
    } else {
        parse_date(s).map(|d| TargetEnd(Some(d)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn dates_and_targets_parse_or_explain() {
        assert_eq!(
            parse_date("2026-11-03"),
            Ok(jiff::civil::date(2026, 11, 3))
        );
        assert_eq!(
            parse_date("tomorrow"),
            Err("tomorrow is not a YYYY-MM-DD date".to_string())
        );
        assert!(parse_target_end("none").is_ok_and(|t| t.0.is_none()));
        assert!(parse_target_end("2026-12-01").is_ok_and(|t| t.0.is_some()));
        assert!(parse_target_end("soon").is_err());
    }

    #[test]
    fn pairs_and_dates_arrive_typed() {
        let cli = Cli::try_parse_from([
            "precip",
            "schedule",
            "confirm",
            "shop",
            "prod",
            "2026-11-03",
        ])
        .unwrap();
        let Some(Command::Schedule {
            command: ScheduleCommand::Confirm { pair, date, .. },
        }) = cli.command
        else {
            panic!("parsed as another command");
        };
        assert_eq!(
            (pair.workload.as_str(), pair.environment.as_str()),
            ("shop", "prod")
        );
        assert_eq!(date, jiff::civil::date(2026, 11, 3));
        assert!(
            Cli::try_parse_from([
                "precip", "schedule", "confirm", "shop", "prod", "11/03",
            ])
            .is_err()
        );
    }

    fn provider(args: &[&str]) -> anyhow::Result<Option<Provider>> {
        let cli = Cli::try_parse_from(
            std::iter::once("precip").chain(args.iter().copied()),
        )?;
        cli.scan.provider()
    }

    #[test]
    fn the_cloud_comes_from_the_flag_or_the_selection_flags() {
        assert_eq!(provider(&["scan"]).unwrap(), None);
        assert_eq!(
            provider(&["scan", "--cloud", "azure"]).unwrap(),
            Some(Provider::Azure)
        );
        assert_eq!(
            provider(&["--profile", "mgmt", "scan"]).unwrap(),
            Some(Provider::Aws)
        );
        assert_eq!(
            provider(&["scan", "--subscription", "s1"]).unwrap(),
            Some(Provider::Azure)
        );
        assert_eq!(
            provider(&["scan", "--cloud", "azure", "--tenant", "t"]).unwrap(),
            Some(Provider::Azure)
        );
    }

    #[test]
    fn flags_of_the_other_cloud_are_refused() {
        assert!(
            provider(&["scan", "--cloud", "aws", "--tenant", "t"]).is_err()
        );
        assert!(
            provider(&["scan", "--cloud", "azure", "--single-account"])
                .is_err()
        );
        assert!(
            provider(&["scan", "--account", "1", "--subscription", "s1"])
                .is_err()
        );
    }

    #[test]
    fn a_profile_alone_still_uses_the_saved_selection() {
        let cli = Cli::try_parse_from(["precip", "--profile", "mgmt", "scan"])
            .unwrap();
        assert!(cli.scan.is_default(Provider::Aws));
        let cli = Cli::try_parse_from([
            "precip",
            "scan",
            "--management-group",
            "prod",
        ])
        .unwrap();
        assert!(!cli.scan.is_default(Provider::Azure));
        assert_eq!(
            cli.scan.azure_options().management_group.as_deref(),
            Some("prod")
        );
    }

    #[test]
    fn the_azure_cloud_flag_selects_azure_and_its_cloud() {
        let cli = Cli::try_parse_from(["precip", "scan"]).unwrap();
        assert_eq!(
            cli.scan.azure_options().cloud,
            precip_azure::AzureCloud::Public
        );
        let cli = Cli::try_parse_from([
            "precip",
            "scan",
            "--azure-cloud",
            "AzureUSGovernment",
        ])
        .unwrap();
        assert_eq!(cli.scan.provider().unwrap(), Some(Provider::Azure));
        assert_eq!(
            cli.scan.azure_options().cloud,
            precip_azure::AzureCloud::UsGovernment
        );
        assert!(
            Cli::try_parse_from(["precip", "scan", "--azure-cloud", "gov"])
                .is_err()
        );
    }
}
