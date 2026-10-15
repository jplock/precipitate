//! The command line: every subcommand and its arguments.

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use jiff::civil::Date;
use precip_engine::schedule::Weekdays;
use precip_engine::sizing::Assumptions;
use precip_types::{AccountId, Provider, Region};

use crate::commands::default_workspaces_dir;

/// Scan AWS, Azure, Google Cloud and OCI estates and size them for Oxide
/// racks. With no subcommand, opens the terminal interface.
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
    Gcp,
    Oci,
}

impl From<Cloud> for Provider {
    fn from(c: Cloud) -> Self {
        match c {
            Cloud::Aws => Provider::Aws,
            Cloud::Azure => Provider::Azure,
            Cloud::Gcp => Provider::Gcp,
            Cloud::Oci => Provider::Oci,
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

/// How to reach the estate. Accepted before or after a subcommand. Flags
/// of two clouds together are refused by [`ScanArgs::provider`]: clap's
/// argument groups do not reach subcommands, where global flags do.
#[derive(Args, Clone)]
pub(crate) struct ScanArgs {
    /// The cloud to scan. Without it, the selection flags decide; with
    /// none, `scan` repeats every cloud's saved selection and the
    /// interface scans AWS.
    #[arg(long, global = true, value_enum)]
    pub(crate) cloud: Option<Cloud>,

    /// AWS CLI profile to use instead of the default credential chain.
    #[arg(long = "aws-profile", global = true, help_heading = "AWS")]
    pub(crate) aws_profile: Option<String>,
    /// Role to try in each member account, in order (repeatable).
    /// Defaults to OrganizationAccountAccessRole, then
    /// AWSControlTowerExecution.
    #[arg(long = "aws-member-role", global = true, help_heading = "AWS")]
    pub(crate) aws_member_roles: Vec<String>,
    /// Scan only these member accounts, by id or name (repeatable).
    #[arg(long = "aws-account", global = true, help_heading = "AWS")]
    pub(crate) aws_accounts: Vec<AccountId>,
    /// Skip these member accounts, by id or name (repeatable).
    #[arg(long = "aws-exclude-account", global = true, help_heading = "AWS")]
    pub(crate) aws_exclude_accounts: Vec<AccountId>,
    /// Scan only the caller's account, even in a management account.
    #[arg(long = "aws-single-account", global = true, help_heading = "AWS")]
    pub(crate) aws_single_account: bool,

    /// Azure cloud to scan. Defaults to AzureCloud, the public cloud. With
    /// the Azure CLI, run `az cloud set` to the same cloud first.
    #[arg(
        long = "azure-cloud",
        global = true,
        value_enum,
        help_heading = "Azure"
    )]
    pub(crate) azure_cloud: Option<AzureCloudArg>,
    /// Entra tenant to sign in to and scan.
    #[arg(long = "azure-tenant", global = true, help_heading = "Azure")]
    pub(crate) azure_tenant: Option<String>,
    /// Scan only these subscriptions, by id or name (repeatable).
    #[arg(long = "azure-subscription", global = true, help_heading = "Azure")]
    pub(crate) azure_subscriptions: Vec<AccountId>,
    /// Skip these subscriptions, by id or name (repeatable).
    #[arg(
        long = "azure-exclude-subscription",
        global = true,
        help_heading = "Azure"
    )]
    pub(crate) azure_exclude_subscriptions: Vec<AccountId>,
    /// Scan only subscriptions under this management group, at any depth.
    #[arg(
        long = "azure-management-group",
        global = true,
        help_heading = "Azure"
    )]
    pub(crate) azure_management_group: Option<String>,

    /// Scan only projects under this organization number, listing the
    /// whole organization in one pass.
    #[arg(
        long = "gcp-organization",
        global = true,
        help_heading = "Google Cloud"
    )]
    pub(crate) gcp_organization: Option<String>,
    /// Scan only projects under this folder number, at any depth.
    #[arg(long = "gcp-folder", global = true, help_heading = "Google Cloud")]
    pub(crate) gcp_folder: Option<String>,
    /// Scan only these projects, by id or number (repeatable).
    #[arg(long = "gcp-project", global = true, help_heading = "Google Cloud")]
    pub(crate) gcp_projects: Vec<AccountId>,
    /// Skip these projects, by id or number (repeatable).
    #[arg(
        long = "gcp-exclude-project",
        global = true,
        help_heading = "Google Cloud"
    )]
    pub(crate) gcp_exclude_projects: Vec<AccountId>,

    /// OCI CLI configuration profile to sign in with. Defaults to
    /// OCI_CLI_PROFILE, else DEFAULT.
    #[arg(long = "oci-profile", global = true, help_heading = "OCI")]
    pub(crate) oci_profile: Option<String>,
    /// Scan only these compartments, by OCID or name (repeatable).
    #[arg(long = "oci-compartment", global = true, help_heading = "OCI")]
    pub(crate) oci_compartments: Vec<String>,
    /// Skip these compartments, by OCID or name (repeatable).
    #[arg(
        long = "oci-exclude-compartment",
        global = true,
        help_heading = "OCI"
    )]
    pub(crate) oci_exclude_compartments: Vec<String>,
    /// Scan only these subscribed regions (repeatable).
    #[arg(long = "oci-region", global = true, help_heading = "OCI")]
    pub(crate) oci_regions: Vec<Region>,
}

impl ScanArgs {
    fn aws_selection(&self) -> bool {
        !self.aws_member_roles.is_empty()
            || !self.aws_accounts.is_empty()
            || !self.aws_exclude_accounts.is_empty()
            || self.aws_single_account
    }

    fn azure_selection(&self) -> bool {
        self.azure_cloud.is_some()
            || self.azure_tenant.is_some()
            || !self.azure_subscriptions.is_empty()
            || !self.azure_exclude_subscriptions.is_empty()
            || self.azure_management_group.is_some()
    }

    fn gcp_selection(&self) -> bool {
        self.gcp_organization.is_some()
            || self.gcp_folder.is_some()
            || !self.gcp_projects.is_empty()
            || !self.gcp_exclude_projects.is_empty()
    }

    fn oci_selection(&self) -> bool {
        !self.oci_compartments.is_empty()
            || !self.oci_exclude_compartments.is_empty()
            || !self.oci_regions.is_empty()
    }

    /// The cloud the flags name: `--cloud`, else the one whose flags are
    /// given. `None` when nothing says.
    pub(crate) fn provider(&self) -> anyhow::Result<Option<Provider>> {
        let flagged: Vec<Provider> = [
            (Provider::Aws, self.aws_selection() || self.aws_profile.is_some()),
            (Provider::Azure, self.azure_selection()),
            (Provider::Gcp, self.gcp_selection()),
            (Provider::Oci, self.oci_selection() || self.oci_profile.is_some()),
        ]
        .into_iter()
        .filter_map(|(p, given)| given.then_some(p))
        .collect();
        let named = self.cloud.map(Provider::from);
        if let Some(p) = named {
            if let Some(other) = flagged.iter().find(|f| **f != p) {
                anyhow::bail!("{other} flags given with --cloud {p}");
            }
            return Ok(Some(p));
        }
        match flagged.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
            [_, _, ..] => anyhow::bail!(
                "flags for more than one cloud given; scan one cloud at a time"
            ),
        }
    }

    /// Whether `provider` was given no selection flags, so its saved
    /// selection applies.
    pub(crate) fn is_default(&self, provider: Provider) -> bool {
        match provider {
            Provider::Aws => !self.aws_selection(),
            Provider::Azure => !self.azure_selection(),
            Provider::Gcp => !self.gcp_selection(),
            Provider::Oci => !self.oci_selection(),
        }
    }

    pub(crate) fn aws_options(&self) -> precip_aws::Options {
        let defaults = precip_aws::Options::default();
        precip_aws::Options {
            member_roles: if self.aws_member_roles.is_empty() {
                defaults.member_roles
            } else {
                self.aws_member_roles.clone()
            },
            accounts: self.aws_accounts.clone(),
            exclude_accounts: self.aws_exclude_accounts.clone(),
            single_account: self.aws_single_account,
        }
    }

    pub(crate) fn azure_options(&self) -> precip_azure::Options {
        precip_azure::Options {
            cloud: self.azure_cloud.map(Into::into).unwrap_or_default(),
            tenant: self.azure_tenant.clone(),
            subscriptions: self.azure_subscriptions.clone(),
            exclude_subscriptions: self.azure_exclude_subscriptions.clone(),
            management_group: self.azure_management_group.clone(),
        }
    }

    pub(crate) fn gcp_options(&self) -> precip_gcp::Options {
        precip_gcp::Options {
            organization: self.gcp_organization.clone(),
            folder: self.gcp_folder.clone(),
            projects: self.gcp_projects.clone(),
            exclude_projects: self.gcp_exclude_projects.clone(),
        }
    }

    pub(crate) fn oci_options(&self) -> precip_oci::Options {
        precip_oci::Options {
            compartments: self.oci_compartments.clone(),
            exclude_compartments: self.oci_exclude_compartments.clone(),
            regions: self.oci_regions.clone(),
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Scan the estate the credentials can reach and append the result to
    /// the database, without the interface. In an AWS Organization
    /// management account this covers every active member account through
    /// the member role; in Azure, every subscription the sign-in can read;
    /// in Google Cloud, every active project it can see; in OCI, every
    /// compartment it can read in every subscribed region.
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
    /// .json, .ics) or --format. `terraform` writes one pair's Oxide
    /// Terraform and needs --workload and --environment.
    Export {
        what: ExportWhat,
        #[arg(long)]
        out: Option<Utf8PathBuf>,
        /// csv, json, or ics (schedule only); needed without --out, except
        /// for terraform.
        #[arg(long)]
        format: Option<String>,
        /// The pair to write Terraform for.
        #[arg(long, requires = "environment")]
        workload: Option<String>,
        #[arg(long, requires = "workload")]
        environment: Option<String>,
        #[command(flatten)]
        sizing: SizingArgs,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum ExportWhat {
    Workloads,
    Sizing,
    Schedule,
    Terraform,
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
            provider(&["--aws-profile", "mgmt", "scan"]).unwrap(),
            Some(Provider::Aws)
        );
        assert_eq!(
            provider(&["scan", "--azure-subscription", "s1"]).unwrap(),
            Some(Provider::Azure)
        );
        assert_eq!(
            provider(&["scan", "--cloud", "azure", "--azure-tenant", "t"])
                .unwrap(),
            Some(Provider::Azure)
        );
    }

    #[test]
    fn flags_of_the_other_cloud_are_refused() {
        assert!(
            provider(&["scan", "--cloud", "aws", "--azure-tenant", "t"])
                .is_err()
        );
        assert!(
            provider(&["scan", "--cloud", "azure", "--aws-single-account"])
                .is_err()
        );
        assert!(
            provider(&[
                "scan",
                "--aws-account",
                "1",
                "--azure-subscription",
                "s1"
            ])
            .is_err()
        );
    }

    #[test]
    fn a_profile_alone_still_uses_the_saved_selection() {
        let cli =
            Cli::try_parse_from(["precip", "--aws-profile", "mgmt", "scan"])
                .unwrap();
        assert!(cli.scan.is_default(Provider::Aws));
        let cli = Cli::try_parse_from([
            "precip",
            "scan",
            "--azure-management-group",
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

    #[test]
    fn google_cloud_flags_select_gcp_and_conflict_with_other_clouds() {
        assert_eq!(
            provider(&["scan", "--gcp-organization", "9"]).unwrap(),
            Some(Provider::Gcp)
        );
        assert_eq!(
            provider(&["scan", "--cloud", "gcp"]).unwrap(),
            Some(Provider::Gcp)
        );
        assert!(
            provider(&["scan", "--cloud", "gcp", "--azure-subscription", "s1"])
                .is_err()
        );
        assert!(
            provider(&["scan", "--gcp-project", "p", "--aws-account", "1"])
                .is_err()
        );
        let cli = Cli::try_parse_from([
            "precip",
            "scan",
            "--gcp-folder",
            "5",
            "--gcp-project",
            "shop",
            "--gcp-exclude-project",
            "old",
        ])
        .unwrap();
        assert!(!cli.scan.is_default(Provider::Gcp));
        assert!(cli.scan.is_default(Provider::Azure));
        let opts = cli.scan.gcp_options();
        assert_eq!(opts.folder.as_deref(), Some("5"));
        assert_eq!(opts.projects, [AccountId::from("shop")]);
        assert_eq!(opts.exclude_projects, [AccountId::from("old")]);
    }

    #[test]
    fn oci_flags_select_oci_and_a_profile_alone_keeps_the_saved_selection() {
        assert_eq!(
            provider(&["scan", "--oci-compartment", "shop"]).unwrap(),
            Some(Provider::Oci)
        );
        assert_eq!(
            provider(&["--oci-profile", "prod", "scan"]).unwrap(),
            Some(Provider::Oci)
        );
        assert!(
            provider(&["scan", "--cloud", "oci", "--gcp-project", "p"])
                .is_err()
        );
        let cli =
            Cli::try_parse_from(["precip", "--oci-profile", "prod", "scan"])
                .unwrap();
        assert!(cli.scan.is_default(Provider::Oci));
        let cli = Cli::try_parse_from([
            "precip",
            "scan",
            "--oci-compartment",
            "shop",
            "--oci-exclude-compartment",
            "sandbox",
            "--oci-region",
            "us-ashburn-1",
        ])
        .unwrap();
        assert!(!cli.scan.is_default(Provider::Oci));
        let opts = cli.scan.oci_options();
        assert_eq!(opts.compartments, ["shop"]);
        assert_eq!(opts.exclude_compartments, ["sandbox"]);
        assert_eq!(opts.regions, [Region::from("us-ashburn-1")]);
    }
}
