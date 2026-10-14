use anyhow::Context;
use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand};

/// `println!` that stops quietly when the reader has gone away, so
/// `precip ... | head` ends like any other command instead of panicking.
macro_rules! out {
    ($($arg:tt)*) => {
        print_line(format_args!($($arg)*))
    };
}

fn print_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let written = stdout.write_fmt(args).and_then(|()| stdout.write_all(b"\n"));
    if written.is_err_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
        std::process::exit(0);
    }
}

/// mimalloc in secure mode for the whole binary: guard pages, randomised
/// allocation and double-free detection, at a small speed cost.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Scan an AWS estate and size it for Oxide racks. With no subcommand,
/// opens the terminal interface.
#[derive(Parser)]
#[command(name = "precip", version)]
struct Cli {
    /// Directory holding workspaces, one SQLite file each.
    #[arg(long, global = true, default_value_t = default_workspaces_dir())]
    workspaces_dir: Utf8PathBuf,

    /// Workspace name to open, skipping the picker.
    #[arg(long, global = true, conflicts_with = "db")]
    workspace: Option<String>,

    /// Explicit database path instead of a named workspace.
    #[arg(long, global = true)]
    db: Option<Utf8PathBuf>,

    #[command(flatten)]
    scan: ScanArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

/// How to reach the estate. Accepted before or after a subcommand.
#[derive(Args, Clone)]
struct ScanArgs {
    /// AWS CLI profile to use instead of the default credential chain.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Role to try in each member account, in order (repeatable).
    /// Defaults to OrganizationAccountAccessRole, then
    /// AWSControlTowerExecution.
    #[arg(long = "member-role", global = true)]
    member_roles: Vec<String>,
    /// Scan only these member account ids (repeatable).
    #[arg(long = "account", global = true)]
    accounts: Vec<String>,
    /// Skip these member account ids (repeatable).
    #[arg(long = "exclude-account", global = true)]
    exclude_accounts: Vec<String>,
    /// Scan only the caller's account, even in a management account.
    #[arg(long, global = true)]
    single_account: bool,
}

impl ScanArgs {
    /// No selection flags given, only (perhaps) a credentials profile.
    fn is_default(&self) -> bool {
        self.member_roles.is_empty()
            && self.accounts.is_empty()
            && self.exclude_accounts.is_empty()
            && !self.single_account
    }

    fn options(&self) -> precip_aws::Options {
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
}

#[derive(Subcommand)]
enum Command {
    /// Scan the estate the credentials can reach and append the result to
    /// the database, without the interface. In an Organization management
    /// account this covers every active member account through the member
    /// role.
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
        workload: String,
        environment: String,
        /// retire, retain, rehost, relocate, repurchase, replatform, refactor
        disposition: String,
        /// Why, kept with the change.
        #[arg(long)]
        note: Option<String>,
    },
    /// Set the disposition new pairs start with; --apply also changes
    /// every pair nobody has set by hand.
    DefaultDisposition {
        disposition: String,
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
    Members { workload: String, environment: String },
    /// Show every disposition change for a workload and environment pair.
    History { workload: String, environment: String },
    /// Size the rehost and relocate workloads of the latest scan.
    Size {
        /// Built-in profile name, or a path to a profile TOML file.
        /// Defaults to the workspace's choice, else gen2-1152.
        #[arg(long)]
        profile: Option<String>,
        /// Multiplier on demand.
        #[arg(long, default_value_t = 1.0)]
        growth: f64,
        /// Share of sled capacity the packer may fill.
        #[arg(long, default_value_t = 0.75)]
        threshold: f64,
        /// Extra sleds per rack.
        #[arg(long, default_value_t = 1)]
        headroom: u32,
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
        /// Sizing profile, growth, threshold and headroom as for `size`.
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value_t = 1.0)]
        growth: f64,
        #[arg(long, default_value_t = 0.75)]
        threshold: f64,
        #[arg(long, default_value_t = 1)]
        headroom: u32,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ExportWhat {
    Workloads,
    Sizing,
    Schedule,
}

#[derive(Subcommand)]
enum ScheduleCommand {
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
        weekdays: Option<String>,
        /// YYYY-MM-DD; "none" clears it.
        #[arg(long)]
        target_end: Option<String>,
        #[arg(long)]
        nonprod_before_prod: Option<bool>,
    },
    /// Add a freeze, or remove one with --remove. Dates are YYYY-MM-DD,
    /// inclusive; leave the end out for a single day.
    Blackout {
        name: String,
        start: String,
        /// Defaults to the start: a one-day freeze.
        end: Option<String>,
        #[arg(long)]
        workload: Option<String>,
        #[arg(long)]
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
        workload: String,
        environment: String,
        #[arg(long)]
        weekdays: Option<String>,
        #[arg(long)]
        earliest: Option<String>,
        #[arg(long, default_value_t = 0)]
        priority: i32,
    },
    /// Which wave a pair is in, how many days its migration takes, and
    /// how many capacity units it uses on each of them.
    Plan {
        workload: String,
        environment: String,
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
        #[arg(long)]
        target_end: Option<String>,
        #[arg(long)]
        remove: bool,
    },
    /// Make a pair wait for another.
    Depends {
        workload: String,
        environment: String,
        on_workload: String,
        on_environment: String,
    },
    /// Confirm a date with the owners. It never moves; confirming
    /// another future date replaces it and counts as a reschedule.
    Confirm {
        workload: String,
        environment: String,
        date: String,
        /// Why, when this replaces an agreed date. See `schedule reasons`.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Mark a pair migrated, today unless a date is given.
    Migrated {
        workload: String,
        environment: String,
        #[arg(long)]
        date: Option<String>,
    },
    /// Mark a pair as missed; a new date is proposed.
    Missed {
        workload: String,
        environment: String,
        /// Why. See `schedule reasons` for the usual ones.
        #[arg(long)]
        reason: Option<String>,
    },
    /// The usual reasons an agreed date moves, for --reason.
    Reasons,
    /// A migrated pair moved back to AWS; a new date is proposed.
    RolledBack {
        workload: String,
        environment: String,
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

/// `$XDG_DATA_HOME/precip/workspaces`, which is `~/.local/share/precip/
/// workspaces` unless the variable is set.
fn default_workspaces_dir() -> Utf8PathBuf {
    xdg::BaseDirectories::with_prefix("precip")
        .get_data_home()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
        .map(|p| p.join("workspaces"))
        .unwrap_or_else(|| Utf8PathBuf::from(".precip").join("workspaces"))
}

/// Logs to stderr. `RUST_LOG` selects levels; the default shows precip at
/// info and hides the AWS SDK's own chatter.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new("info,aws=warn,hyper=warn")
        });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// Scan, store, and group. Shared by the headless command and the TUI.
async fn scan_and_store(
    config: aws_config::SdkConfig,
    opts: precip_aws::Options,
    store: precip_db::Store,
    progress: Option<precip_aws::ProgressSender>,
) -> anyhow::Result<precip_db::ScanStats> {
    store
        .save_scan_options(&serde_json::to_string(&opts)?)
        .await
        .context("saving the scan selection")?;
    let scan = precip_aws::scan_estate(&config, &opts, progress)
        .await
        .context("scanning estate")?;
    let stats = store.record_scan(&scan).await.context("storing scan")?;
    let grouped = store
        .group_scan(&scan.id, &precip_types::GroupingConfig::default())
        .await
        .context("grouping workloads")?;
    let pruned = store
        .prune_stale_pairs(&scan.id, true)
        .await
        .context("pruning stale pairs")?;
    let failed = scan
        .targets
        .iter()
        .filter(|t| {
            !matches!(
                t.status,
                precip_types::scan::TargetStatus::Scanned
                    | precip_types::scan::TargetStatus::Empty
            )
        })
        .count();
    tracing::info!(
        id = %scan.id,
        seq = stats.seq,
        resources = scan.resources.len(),
        added = stats.added,
        changed = stats.changed,
        unchanged = stats.unchanged,
        reappeared = stats.reappeared,
        grouped = grouped.resources,
        unassigned = grouped.unassigned,
        workloads = grouped.workloads,
        stale_pairs_pruned = pruned,
        targets = scan.targets.len(),
        targets_failed_or_partial = failed,
        "scan stored"
    );
    for f in store
        .verify_migrated(&scan.id)
        .await
        .context("verifying migrated pairs")?
    {
        tracing::warn!(
            workload = %f.workload,
            environment = %f.environment,
            remaining = f.remaining,
            "marked migrated but compute is still in AWS"
        );
    }
    Ok(stats)
}

/// A built-in profile by name, or a TOML file by path.
fn load_profile(name_or_path: &str) -> anyhow::Result<precip_types::Profile> {
    if let Some(built_in) =
        precip_engine::sizing::profiles::builtin(name_or_path)
    {
        return built_in.context("parsing built-in profile");
    }
    let text = std::fs::read_to_string(name_or_path)
        .with_context(|| format!("reading profile {name_or_path}"))?;
    precip_types::Profile::from_toml(&text)
        .with_context(|| format!("parsing profile {name_or_path}"))
}

async fn open_store(
    chosen_db: Option<Utf8PathBuf>,
    what: &str,
) -> anyhow::Result<precip_db::Store> {
    let db = chosen_db.with_context(|| {
        format!("{what} needs --workspace <name> or --db <path>")
    })?;
    precip_db::Store::open(&db).await.with_context(|| format!("opening {db}"))
}

async fn latest_scan(
    store: &precip_db::Store,
) -> anyhow::Result<precip_db::ScanSummary> {
    store
        .list_scans()
        .await?
        .into_iter()
        .next()
        .context("no scans in this workspace")
}

fn today() -> jiff::civil::Date {
    jiff::Zoned::now().date()
}

fn parse_date(s: &str) -> anyhow::Result<jiff::civil::Date> {
    s.parse().with_context(|| format!("{s} is not a YYYY-MM-DD date"))
}

/// Print the schedule: the version the last change produced, or a fresh
/// computation when `trigger` is given (the `show` command, a new day).
async fn print_schedule(
    store: &precip_db::Store,
    trigger: Option<&str>,
) -> anyhow::Result<()> {
    let rules = store.global_rules().await?;
    let latest = match trigger {
        Some(_) => None,
        None => store
            .latest_schedule()
            .await?
            .filter(|(_, as_of, _)| *as_of == today()),
    };
    let (version, schedule) = match latest {
        Some((id, _, schedule)) => (id, schedule),
        None => {
            store
                .compute_schedule(today(), trigger.unwrap_or("command"))
                .await?
        }
    };
    out!(
        "schedule #{version}  {} per day, {} per week, {} per month, {}{}{}",
        rules.per_day,
        rules.per_week,
        rules.per_month,
        weekday_rule_label(&rules.weekdays),
        rules
            .target_end
            .map(|d| format!("  target end {d}"))
            .unwrap_or_default(),
        if rules.nonprod_before_prod { "  non-prod before prod" } else { "" }
    );
    out!("");
    out!(
        "{:<5} {:<12} {:<4} {:<6} {:<28} {:<10} {:<10} {}",
        "wave",
        "start",
        "day",
        "days",
        "workload",
        "env",
        "state",
        "alternatives / reason"
    );
    for s in &schedule.slots {
        let alts: Vec<String> =
            s.alternatives.iter().map(|d| d.to_string()).collect();
        let days = (s.end - s.date).get_days() + 1;
        out!(
            "{:<5} {:<12} {:<4} {:<6} {:<28} {:<10} {:<10} {}  ({})",
            s.wave,
            s.date,
            precip_db::weekday_name(s.date.weekday()),
            if s.weight > 1 {
                format!("{days}x{}", s.weight)
            } else {
                days.to_string()
            },
            s.workload,
            s.environment,
            if s.locked { "confirmed" } else { "proposed" },
            alts.join(" or "),
            s.reason
        );
    }
    for (wave, end) in &schedule.wave_ends {
        out!("wave {wave} ends {end}");
    }
    for ((w, e), why) in &schedule.conflicts {
        out!("CONFLICT {w} / {e}: {why}");
    }
    for ((w, e), why) in &schedule.unplaced {
        out!("UNPLACED {w} / {e}: {why}");
    }
    if let Some(end) = schedule.ends {
        out!("");
        out!(
            "projected end {end}{}",
            match rules.target_end {
                Some(t) if end > t => format!(
                    "  LATE by {} days against {t}",
                    (end - t).get_days()
                ),
                Some(t) => format!("  within target {t}"),
                None => String::new(),
            }
        );
    }
    for (w, e) in &schedule.expired {
        out!(
            "expired: {w} / {e} confirmed date passed without a migration; rescheduled"
        );
    }
    for ((w, e), why) in &schedule.unplaced {
        out!("unplaced: {w} / {e}: {why}");
    }
    Ok(())
}

async fn run_schedule(
    store: &precip_db::Store,
    command: ScheduleCommand,
) -> anyhow::Result<()> {
    match command {
        ScheduleCommand::Show => print_schedule(store, Some("command")).await?,
        ScheduleCommand::Set {
            per_day,
            per_week,
            per_month,
            weekdays,
            target_end,
            nonprod_before_prod,
        } => {
            let mut rules = store.global_rules().await?;
            if let Some(v) = per_day {
                rules.per_day = v;
            }
            if let Some(v) = per_week {
                rules.per_week = v;
            }
            if let Some(v) = per_month {
                rules.per_month = v;
            }
            if let Some(v) = weekdays {
                rules.weekdays = precip_db::weekdays_from_spec(&v);
            }
            if let Some(v) = target_end {
                rules.target_end =
                    if v == "none" { None } else { Some(parse_date(&v)?) };
            }
            if let Some(v) = nonprod_before_prod {
                rules.nonprod_before_prod = v;
            }
            store.set_global_rules(&rules).await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Blackout {
            name,
            start,
            end,
            workload,
            environment,
            remove,
        } => {
            let start = parse_date(&start)?;
            let b = precip_engine::schedule::Blackout {
                name,
                start,
                end: end.map(|e| parse_date(&e)).transpose()?.unwrap_or(start),
                pair: workload.zip(environment),
            };
            if remove {
                store.remove_blackout(&b).await?;
            } else {
                store.add_blackout(&b).await?;
            }
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Blackouts => {
            let today = today();
            for b in
                store.blackouts().await?.into_iter().filter(|b| b.end >= today)
            {
                out!(
                    "{} to {}  {}{}",
                    b.start,
                    b.end,
                    b.name,
                    b.pair
                        .map(|(w, e)| format!("  (only {w} / {e})"))
                        .unwrap_or_default()
                );
            }
        }
        ScheduleCommand::Holidays { country, year, only, skip } => {
            anyhow::ensure!(
                country.eq_ignore_ascii_case("us"),
                "only US federal holidays are built in; add others with `schedule blackout`"
            );
            let names = |list: Option<String>| -> Vec<String> {
                list.map(|l| {
                    l.split(',').map(|n| n.trim().to_lowercase()).collect()
                })
                .unwrap_or_default()
            };
            let (only, skip) = (names(only), names(skip));
            let chosen: Vec<_> =
                precip_engine::schedule::holidays::us_federal(year)
                    .into_iter()
                    .filter(|h| {
                        let n = h.name.to_lowercase();
                        (only.is_empty() || only.contains(&n))
                            && !skip.contains(&n)
                    })
                    .collect();
            store.add_holidays(&chosen).await?;
            for h in &chosen {
                out!("{}  {}", h.date, h.name);
            }
            print_schedule(store, None).await?;
        }
        ScheduleCommand::HolidayNames { country, year } => {
            anyhow::ensure!(
                country.eq_ignore_ascii_case("us"),
                "only US federal holidays are built in"
            );
            for h in precip_engine::schedule::holidays::us_federal(year) {
                out!("{}  {}", h.date, h.name);
            }
        }
        ScheduleCommand::Rule {
            workload,
            environment,
            weekdays,
            earliest,
            priority,
        } => {
            let days = weekdays
                .map(|w| precip_db::weekdays_from_spec(&w))
                .unwrap_or_default();
            let earliest = earliest.map(|e| parse_date(&e)).transpose()?;
            store
                .set_pair_rule(
                    &workload,
                    &environment,
                    &days,
                    earliest,
                    priority,
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Plan {
            workload,
            environment,
            wave,
            duration,
            weight,
        } => {
            store
                .set_pair_plan(&workload, &environment, wave, duration, weight)
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Wave { number, name, target_end, remove } => {
            if remove {
                let moved = store.remove_wave(number).await?;
                out!("wave {number} removed; {moved} pair(s) back in wave 1");
            } else {
                let target = target_end.map(|d| parse_date(&d)).transpose()?;
                store.set_wave(number, &name, target).await?;
            }
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Depends {
            workload,
            environment,
            on_workload,
            on_environment,
        } => {
            store
                .add_dependency(
                    &workload,
                    &environment,
                    &on_workload,
                    &on_environment,
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Confirm { workload, environment, date, reason } => {
            store
                .set_pair_status(
                    &workload,
                    &environment,
                    precip_engine::schedule::Status::Confirmed,
                    Some(parse_date(&date)?),
                    today(),
                    reason.as_deref(),
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Migrated { workload, environment, date } => {
            let date = date.map(|d| parse_date(&d)).transpose()?;
            store
                .set_pair_status(
                    &workload,
                    &environment,
                    precip_engine::schedule::Status::Migrated,
                    date,
                    today(),
                    None,
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Missed { workload, environment, reason } => {
            store
                .set_pair_status(
                    &workload,
                    &environment,
                    precip_engine::schedule::Status::Missed,
                    None,
                    today(),
                    reason.as_deref(),
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Reasons => {
            for r in precip_db::RESCHEDULE_REASONS {
                out!("{r}");
            }
        }
        ScheduleCommand::RolledBack { workload, environment, note } => {
            store
                .set_pair_status(
                    &workload,
                    &environment,
                    precip_engine::schedule::Status::RolledBack,
                    None,
                    today(),
                    note.as_deref(),
                )
                .await?;
            print_schedule(store, None).await?;
        }
        ScheduleCommand::Verify => {
            let Some(latest) = store.list_scans().await?.into_iter().next()
            else {
                anyhow::bail!("no scans yet");
            };
            let findings = store.verify_migrated(&latest.id).await?;
            if findings.is_empty() {
                out!("every migrated pair is gone from scan #{}", latest.seq);
            }
            for f in findings {
                out!(
                    "{} / {}: marked migrated but {} resource(s) still in scan #{}",
                    f.workload,
                    f.environment,
                    f.remaining,
                    latest.seq
                );
            }
        }
        ScheduleCommand::Metrics => {
            let m = store.metrics(today()).await?;
            print_metrics(&m);
        }
        ScheduleCommand::History => {
            for e in store.events().await? {
                out!(
                    "{}  {}{}",
                    e.at.strftime("%Y-%m-%d %H:%M"),
                    e.event.describe(),
                    e.ends
                        .map(|d| format!("   -> ends {d}"))
                        .unwrap_or_default()
                );
            }
        }
        ScheduleCommand::Gantt { width } => {
            let (_, schedule) =
                store.compute_schedule(today(), "command").await?;
            let gantt = precip_engine::schedule::gantt::build(
                &schedule,
                today(),
                width,
            );
            for line in precip_engine::schedule::gantt::render(&gantt, 32) {
                out!("{line}");
            }
        }
        ScheduleCommand::Replay => {
            let (version, schedule) = store.replay().await?;
            out!(
                "rebuilt the plan from {} events; schedule #{version} ends {}",
                store.events().await?.len(),
                schedule
                    .ends
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "-".into())
            );
        }
    }
    Ok(())
}

fn print_change(c: &precip_db::DispositionChange) -> anyhow::Result<()> {
    let name = |d: precip_types::Disposition| -> anyhow::Result<String> {
        Ok(serde_json::to_value(d)?.as_str().unwrap_or_default().to_string())
    };
    let change = match c.previous {
        Some(p) => format!("{} -> {}", name(p)?, name(c.disposition)?),
        None => name(c.disposition)?,
    };
    out!(
        "{}  {:<24} {:<8} {}",
        c.changed_at.strftime("%Y-%m-%d %H:%M:%S"),
        change,
        c.source,
        c.note.as_deref().unwrap_or("")
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let args = cli.scan;
    let dir = precip_tui::workspace::WorkspaceDir(cli.workspaces_dir.clone());
    let chosen_db: Option<Utf8PathBuf> = match (&cli.db, &cli.workspace) {
        (Some(db), _) => Some(db.clone()),
        (None, Some(name)) => Some(dir.path_for(name)),
        (None, None) => None,
    };

    match cli.command {
        Some(Command::Scan) => {
            init_tracing();
            let db = chosen_db.context(
                "headless scan needs --workspace <name> or --db <path>",
            )?;
            if let Some(parent) = db.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {parent}"))?;
            }
            let config = precip_aws::load_config(args.profile.as_deref()).await;
            let store = precip_db::Store::open(&db)
                .await
                .with_context(|| format!("opening {db}"))?;
            // With no selection on the command line, repeat the last one.
            let opts = if args.is_default() {
                match store.saved_scan_options().await? {
                    Some(json) => {
                        tracing::info!(
                            "using the workspace's saved scan selection"
                        );
                        serde_json::from_str(&json)
                            .context("reading the saved scan selection")?
                    }
                    None => args.options(),
                }
            } else {
                args.options()
            };
            scan_and_store(config, opts, store, None).await?;
        }
        None => {
            // Logs would corrupt the terminal; the activity feed replaces
            // them. RUST_LOG still works for anyone redirecting stderr.
            if std::env::var_os("RUST_LOG").is_some() {
                init_tracing();
            }
            let config = precip_aws::load_config(args.profile.as_deref()).await;
            let opts = args.options();
            let launcher: precip_tui::Launcher = Box::new(move |store| {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let config = config.clone();
                let opts = opts.clone();
                let done = tokio::spawn(async move {
                    scan_and_store(config, opts, store, Some(tx))
                        .await
                        .map_err(|e| format!("{e:#}"))
                });
                precip_tui::ScanHandle { events: rx, done }
            });
            let context = precip_tui::Context {
                profile: args.profile.clone(),
                workspace: String::new(),
            };
            tokio::task::block_in_place(|| {
                let app =
                    precip_tui::App::new(dir, chosen_db, context, launcher)?;
                app.run()
            })
            .context("running TUI")?;
        }
        Some(Command::Group) => {
            init_tracing();
            let db = chosen_db
                .context("group needs --workspace <name> or --db <path>")?;
            let store = precip_db::Store::open(&db)
                .await
                .with_context(|| format!("opening {db}"))?;
            let latest = store
                .list_scans()
                .await
                .context("listing scans")?
                .into_iter()
                .next()
                .context("no scans in this workspace")?;
            let stats = store
                .group_scan(
                    &latest.id,
                    &precip_types::GroupingConfig::default(),
                )
                .await
                .context("grouping workloads")?;
            tracing::info!(
                scan = latest.seq,
                resources = stats.resources,
                unassigned = stats.unassigned,
                workloads = stats.workloads,
                pairs = stats.pairs,
                "grouped"
            );
            let names = store.account_names(latest.seq).await?;
            for row in store.workload_summary(&latest.id).await? {
                let accounts: Vec<String> = row
                    .accounts
                    .iter()
                    .map(|id| match names.get(id) {
                        Some(n) => format!("{n} ({id})"),
                        None => id.clone(),
                    })
                    .collect();
                out!(
                    "{:<32} {:<10} {:<11} {:>5}  {}  {}",
                    row.workload,
                    row.environment,
                    serde_json::to_value(row.disposition)?
                        .as_str()
                        .unwrap_or_default(),
                    row.resources,
                    accounts.join(","),
                    row.regions.join(",")
                );
            }
        }
        Some(Command::Merge { alias, into }) => {
            init_tracing();
            let db = chosen_db
                .context("merge needs --workspace <name> or --db <path>")?;
            let store = precip_db::Store::open(&db)
                .await
                .with_context(|| format!("opening {db}"))?;
            store.merge_workload(&alias, &into).await.context("merging")?;
            if let Some(latest) = store.list_scans().await?.into_iter().next() {
                let stats = store
                    .group_scan(
                        &latest.id,
                        &precip_types::GroupingConfig::default(),
                    )
                    .await
                    .context("regrouping")?;
                tracing::info!(
                    alias,
                    into,
                    workloads = stats.workloads,
                    "merged and regrouped"
                );
            }
        }
        Some(Command::Disposition {
            workload,
            environment,
            disposition,
            note,
        }) => {
            let db = chosen_db.context(
                "disposition needs --workspace <name> or --db <path>",
            )?;
            let store = precip_db::Store::open(&db)
                .await
                .with_context(|| format!("opening {db}"))?;
            let d: precip_types::Disposition = serde_json::from_value(
                serde_json::Value::String(disposition.clone()),
            )
            .with_context(|| format!("unknown disposition {disposition}"))?;
            store
                .set_disposition_with(
                    &workload,
                    &environment,
                    d,
                    "user",
                    note.as_deref(),
                )
                .await
                .context("setting disposition")?;
            for c in store.disposition_history(&workload, &environment).await? {
                print_change(&c)?;
            }
        }
        Some(Command::DefaultDisposition { disposition, apply }) => {
            let store = open_store(chosen_db, "default-disposition").await?;
            let d: precip_types::Disposition = serde_json::from_value(
                serde_json::Value::String(disposition.clone()),
            )
            .with_context(|| format!("unknown disposition {disposition}"))?;
            store.set_default_disposition(d, apply).await?;
            out!(
                "new pairs start as {disposition}{}",
                if apply {
                    "; pairs nobody set by hand changed too"
                } else {
                    ""
                }
            );
        }
        Some(Command::Workspaces) => {
            for w in dir.list().context("listing workspaces")? {
                out!(
                    "{:<32} {}",
                    w.name,
                    w.modified
                        .and_then(|m| jiff::Timestamp::try_from(m).ok())
                        .map(|t| t.strftime("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_default()
                );
            }
        }
        Some(Command::Overview) => {
            let store = open_store(chosen_db, "overview").await?;
            let latest = latest_scan(&store).await?;
            let o = store.overview(latest.seq).await?;
            out!(
                "scan #{}  {} accounts  {} regions",
                latest.seq,
                o.accounts,
                o.regions
            );
            for (kind, n) in &o.by_kind {
                out!("{n:>8}  {kind}");
            }
        }
        Some(Command::Scans) => {
            let store = open_store(chosen_db, "scans").await?;
            for s in store.list_scans().await? {
                out!(
                    "#{:<4} {}  {}  {}",
                    s.seq,
                    s.started.strftime("%Y-%m-%d %H:%M"),
                    s.finished
                        .map(|f| f.strftime("%H:%M").to_string())
                        .unwrap_or_else(|| "unfinished".into()),
                    s.caller_arn
                );
            }
        }
        Some(Command::Accounts) => {
            let store = open_store(chosen_db, "accounts").await?;
            let latest = latest_scan(&store).await?;
            for (id, name) in store.account_names(latest.seq).await? {
                out!("{id}  {name}");
            }
        }
        Some(Command::Workloads) => {
            let store = open_store(chosen_db, "workloads").await?;
            let latest = latest_scan(&store).await?;
            out!(
                "{:<28} {:<10} {:<11} {:>9}  {:<30} {}",
                "workload",
                "env",
                "disposition",
                "resources",
                "accounts",
                "regions"
            );
            for w in store.workload_summary(&latest.id).await? {
                out!(
                    "{:<28} {:<10} {:<11} {:>9}  {:<30} {}",
                    w.workload,
                    w.environment,
                    serde_json::to_value(w.disposition)?
                        .as_str()
                        .unwrap_or_default(),
                    w.resources,
                    w.accounts.join(","),
                    w.regions.join(",")
                );
            }
        }
        Some(Command::Members { workload, environment }) => {
            let store = open_store(chosen_db, "members").await?;
            let latest = latest_scan(&store).await?;
            for m in
                store.pair_members(&latest.id, &workload, &environment).await?
            {
                out!(
                    "{:<20} {:<36} {:<14} {:<14} {:<14} {}",
                    m.service.as_deref().unwrap_or(&m.kind),
                    m.name,
                    m.account_id,
                    m.region,
                    m.signal,
                    m.arn
                );
            }
        }
        Some(Command::History { workload, environment }) => {
            let db = chosen_db
                .context("history needs --workspace <name> or --db <path>")?;
            let store = precip_db::Store::open(&db)
                .await
                .with_context(|| format!("opening {db}"))?;
            for c in store.disposition_history(&workload, &environment).await? {
                print_change(&c)?;
            }
        }
        Some(Command::Size { profile, growth, threshold, headroom }) => {
            let db = chosen_db
                .context("size needs --workspace <name> or --db <path>")?;
            let store = precip_db::Store::open(&db)
                .await
                .with_context(|| format!("opening {db}"))?;
            let latest = store
                .list_scans()
                .await?
                .into_iter()
                .next()
                .context("no scans in this workspace")?;
            let name = match profile {
                Some(p) => p,
                None => store
                    .sizing_profile()
                    .await?
                    .unwrap_or_else(|| "gen2-1152".into()),
            };
            let profile = load_profile(&name)?;
            let assumptions = precip_engine::sizing::Assumptions {
                growth_factor: growth,
                threshold,
                headroom,
            };
            let instances = store.sized_instances(&latest.id, &profile).await?;
            let inputs: Vec<precip_engine::sizing::Instance> =
                instances.iter().map(|i| i.to_instance()).collect();
            let sizing =
                precip_engine::sizing::size(&profile, &inputs, &assumptions);
            let cap = sizing.sled_capacity;

            out!(
                "profile {}  oxide release {}  {} sleds per rack",
                profile.name,
                profile.oxide_release,
                profile.sleds_per_rack
            );
            out!(
                "assumptions: growth x{:.2}, threshold {:.0}%, N+{} per rack",
                growth,
                threshold * 100.0,
                headroom
            );
            out!(
                "usable per sled after threshold: {:.0} vCPU, {:.0} GiB, {:.1} TiB",
                cap.vcpus,
                cap.memory_gib,
                cap.disk_tib
            );
            out!("");
            out!(
                "{:<28} {:<8} {:>5} {:>7} {:>9} {:>9}  fit",
                "workload",
                "env",
                "inst",
                "vCPU",
                "GiB",
                "disk GiB"
            );
            for d in precip_engine::sizing::demand_by_workload(&instances) {
                out!(
                    "{:<28} {:<8} {:>5} {:>7} {:>9} {:>9}  {} fit as-is, {} need changes, {} cannot move",
                    d.workload,
                    d.environment,
                    d.instances,
                    d.vcpus,
                    d.memory_gib,
                    d.disk_gib,
                    d.clean,
                    d.remediate,
                    d.blocked
                );
            }
            out!("");
            out!(
                "compute sleds {}   storage sleds {}   bound by {:?}",
                sizing.compute_sleds,
                sizing.storage_sleds,
                sizing.bound
            );
            out!(
                "sleds {} (including N+{})   racks {}   spare slots {}",
                sizing.sleds,
                headroom,
                sizing.racks,
                sizing.spare_sleds
            );
            if !sizing.not_sized.is_empty() {
                out!("not sized (disposition): {}", sizing.not_sized.len());
            }
            if !sizing.excluded.is_empty() {
                out!("");
                out!("cannot move as they are ({}):", sizing.excluded.len());
                for i in instances
                    .iter()
                    .filter(|i| sizing.excluded.contains(&i.arn))
                {
                    out!(
                        "  {} ({} {}): {}",
                        i.name,
                        i.instance_type,
                        i.architecture,
                        i.findings.join("; ")
                    );
                }
            }
            let remediations: Vec<_> = instances
                .iter()
                .filter(|i| {
                    i.fit == precip_engine::sizing::Fit::NeedsRemediation
                })
                .collect();
            if !remediations.is_empty() {
                out!("");
                out!("need changes before moving ({}):", remediations.len());
                for i in remediations {
                    out!(
                        "  {} ({}): {}",
                        i.name,
                        i.instance_type,
                        i.findings.join("; ")
                    );
                }
            }
        }
        Some(Command::Prune { all }) => {
            init_tracing();
            let db = chosen_db
                .context("prune needs --workspace <name> or --db <path>")?;
            let store = precip_db::Store::open(&db)
                .await
                .with_context(|| format!("opening {db}"))?;
            let latest = store
                .list_scans()
                .await?
                .into_iter()
                .next()
                .context("no scans in this workspace")?;
            let removed = store.prune_stale_pairs(&latest.id, !all).await?;
            tracing::info!(
                removed,
                kept_user_changed = !all,
                "pruned stale pairs"
            );
        }
        Some(Command::Schedule { command }) => {
            let db = chosen_db
                .context("schedule needs --workspace <name> or --db <path>")?;
            let store = precip_db::Store::open(&db)
                .await
                .with_context(|| format!("opening {db}"))?;
            run_schedule(&store, command).await?;
        }
        Some(Command::Export {
            what,
            out,
            format,
            profile,
            growth,
            threshold,
            headroom,
        }) => {
            let store = open_store(chosen_db, "export").await?;
            let format = match (&format, &out) {
                (Some(f), _) => f.to_lowercase(),
                (None, Some(path)) => {
                    path.extension().unwrap_or_default().to_lowercase()
                }
                (None, None) => anyhow::bail!("give --out <file> or --format"),
            };
            let latest = latest_scan(&store).await?;
            let text = match (what, format.as_str()) {
                (ExportWhat::Workloads, "csv") => {
                    precip_db::export::workloads_csv(
                        &store.workload_summary(&latest.id).await?,
                    )
                }
                (ExportWhat::Workloads, "json") => {
                    precip_db::export::workloads_json(
                        &store.workload_summary(&latest.id).await?,
                    )?
                }
                (ExportWhat::Sizing, "csv" | "json") => {
                    let name = match profile {
                        Some(p) => p,
                        None => store
                            .sizing_profile()
                            .await?
                            .unwrap_or_else(|| "gen2-1152".into()),
                    };
                    let profile = load_profile(&name)?;
                    let assumptions = precip_engine::sizing::Assumptions {
                        growth_factor: growth,
                        threshold,
                        headroom,
                    };
                    let instances =
                        store.sized_instances(&latest.id, &profile).await?;
                    if format == "csv" {
                        precip_db::export::sizing_csv(&instances)
                    } else {
                        let inputs: Vec<precip_engine::sizing::Instance> =
                            instances.iter().map(|i| i.to_instance()).collect();
                        let result = precip_engine::sizing::size(
                            &profile,
                            &inputs,
                            &assumptions,
                        );
                        let demand = precip_engine::sizing::demand_by_workload(
                            &instances,
                        );
                        precip_db::export::sizing_json(
                            &profile,
                            &assumptions,
                            &result,
                            &demand,
                            &instances,
                        )?
                    }
                }
                (ExportWhat::Schedule, "csv" | "json" | "ics") => {
                    let (_, schedule) =
                        store.compute_schedule(today(), "export").await?;
                    match format.as_str() {
                        "csv" => precip_db::export::schedule_csv(&schedule),
                        "json" => precip_db::export::schedule_json(&schedule)?,
                        _ => precip_db::export::schedule_ics(
                            &schedule,
                            cli.workspace.as_deref().unwrap_or("precip"),
                            jiff::Timestamp::now(),
                        ),
                    }
                }
                (_, other) => {
                    anyhow::bail!("{other} is not a format this export knows")
                }
            };
            match out {
                Some(path) => {
                    std::fs::write(&path, text)
                        .with_context(|| format!("writing {path}"))?;
                    out!("wrote {path}");
                }
                None => print_line(format_args!("{text}")),
            }
        }
        Some(Command::Profiles { use_profile }) => {
            if let Some(name) = use_profile {
                load_profile(&name)?;
                let store =
                    open_store(chosen_db, "profiles --use-profile").await?;
                store.set_sizing_profile(&name).await?;
                out!("sizing with {name}");
            } else {
                for name in precip_engine::sizing::profiles::NAMES {
                    out!("{name}");
                }
            }
        }
    }
    Ok(())
}

fn print_metrics(m: &precip_db::Metrics) {
    let pct = |r: f64| format!("{:.0}%", r * 100.0);
    let day = |d: Option<jiff::civil::Date>| {
        d.map(|d| d.to_string()).unwrap_or_else(|| "-".into())
    };
    out!(
        "pairs              {}  ({} migrated, {} confirmed, {} proposed, {} missed, {} rolled back)",
        m.total,
        m.migrated,
        m.confirmed,
        m.proposed,
        m.missed,
        m.rolled_back
    );
    out!(
        "reschedule rate    {}  ({} of {} confirmed migrations missed or re-confirmed)",
        pct(m.reschedule_rate),
        m.rescheduled,
        m.scheduled
    );
    out!(
        "first-time success {}  (migrated on the first confirmed date)",
        pct(m.first_time_success_rate)
    );
    out!(
        "confirmation       {}  of outstanding pairs have a confirmed date",
        pct(m.confirmation_rate)
    );
    out!(
        "projected end      {}  first projection {}  drift {:+} days  target {}",
        day(m.projected_end),
        day(m.first_projected_end),
        m.end_drift_days,
        day(m.target_end)
    );
    out!(
        "average slip       {:.1} days from first confirmed date to migration",
        m.average_slip_days
    );
    out!(
        "last 30 days       {} migrated against a capacity of {}",
        m.migrated_last_30_days,
        m.capacity_last_30_days
    );
    out!(
        "rollbacks          {}  ({} pairs currently rolled back)",
        pct(m.rollback_rate),
        m.rolled_back
    );
    if m.forecast_within_week_rate > 0.0 || m.forecast_error_days != 0.0 {
        out!(
            "forecast          first plan was off by {:+.1} days on average; {} of migrations landed within a week of it",
            m.forecast_error_days,
            pct(m.forecast_within_week_rate)
        );
    }
    if m.end_history.len() > 1 {
        let trail: Vec<String> = m
            .end_history
            .iter()
            .map(|(day, ends)| format!("{day}: {ends}"))
            .collect();
        out!("end over time     {}", trail.join("  ->  "));
    }
    if !m.reschedule_reasons.is_empty() {
        let list: Vec<String> = m
            .reschedule_reasons
            .iter()
            .map(|(r, n)| format!("{r} {n}"))
            .collect();
        out!("why dates moved   {}", list.join(", "));
    }
    for w in &m.waves {
        out!(
            "wave {:<3} {:<16} {}/{} migrated  ends {}{}",
            w.number,
            w.name,
            w.migrated,
            w.total,
            day(w.projected_end),
            match (w.projected_end, w.target_end) {
                (Some(e), Some(t)) if e > t =>
                    format!("  LATE by {} days against {t}", (e - t).get_days()),
                (_, Some(t)) => format!("  target {t}"),
                _ => String::new(),
            }
        );
    }
}

fn weekday_rule_label(days: &[jiff::civil::Weekday]) -> String {
    match precip_db::weekday_preset(days) {
        Some(preset) => preset.to_string(),
        None => days
            .iter()
            .map(|d| precip_db::weekday_name(*d))
            .collect::<Vec<_>>()
            .join(","),
    }
}
