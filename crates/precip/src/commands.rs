//! One function per top-level command.

use anyhow::Context;
use camino::Utf8PathBuf;
use precip_db::{DispositionSource, Store, Trigger, export};
use precip_engine::sizing::profiles::BuiltinProfile;
use precip_tui::workspace::WorkspaceDir;
use precip_types::{Disposition, PairKey, Provider};
use serde::de::DeserializeOwned;

use crate::cli::{Cli, ExportWhat, PairArgs, ScanArgs, SizingArgs};
use crate::schedule::today;
use crate::{init_tracing, print_line};
use precip_engine::sizing::{Fit, Instance, Sizing, WorkloadDemand};
use precip_types::scan::TargetStatus;

/// Where the command works: the workspaces directory and the database
/// chosen by --workspace or --db.
pub(crate) struct Env {
    pub(crate) dir: WorkspaceDir,
    pub(crate) db: Option<Utf8PathBuf>,
    pub(crate) workspace: Option<String>,
}

impl Env {
    pub(crate) fn new(cli: &Cli) -> Self {
        let dir = WorkspaceDir(cli.workspaces_dir.clone());
        let db = match (&cli.db, &cli.workspace) {
            (Some(db), _) => Some(db.clone()),
            (None, Some(name)) => Some(dir.path_for(name)),
            (None, None) => None,
        };
        Self { dir, db, workspace: cli.workspace.clone() }
    }

    /// The chosen database, which `what` needs.
    pub(crate) fn db(&self, what: &str) -> anyhow::Result<&Utf8PathBuf> {
        self.db.as_ref().with_context(|| {
            format!("{what} needs --workspace <name> or --db <path>")
        })
    }

    pub(crate) async fn store(&self, what: &str) -> anyhow::Result<Store> {
        let db = self.db(what)?;
        Store::open(db).await.with_context(|| format!("opening {db}"))
    }
}

/// `$XDG_DATA_HOME/precip/workspaces`, which is `~/.local/share/precip/
/// workspaces` unless the variable is set.
#[cfg(unix)]
pub(crate) fn default_workspaces_dir() -> Utf8PathBuf {
    xdg::BaseDirectories::with_prefix("precip")
        .get_data_home()
        .and_then(|p| Utf8PathBuf::from_path_buf(p).ok())
        .map_or_else(fallback_workspaces_dir, |p| p.join("workspaces"))
}

/// `%LOCALAPPDATA%\precip\workspaces`: XDG has no meaning on Windows.
#[cfg(windows)]
pub(crate) fn default_workspaces_dir() -> Utf8PathBuf {
    std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|v| !v.is_empty())
        .map_or_else(fallback_workspaces_dir, |p| {
            Utf8PathBuf::from(p).join("precip").join("workspaces")
        })
}

/// Next to where precip runs, when the platform names no data directory.
fn fallback_workspaces_dir() -> Utf8PathBuf {
    Utf8PathBuf::from(".precip").join("workspaces")
}

/// One cloud estate to scan: how to sign in and what to select.
#[derive(Clone)]
pub(crate) enum Estate {
    Aws { config: Box<aws_config::SdkConfig>, opts: precip_aws::Options },
    Azure { credential: precip_azure::Credential, opts: precip_azure::Options },
    Gcp { credential: precip_gcp::Credential, opts: precip_gcp::Options },
    Oci { credential: precip_oci::Credential, opts: precip_oci::Options },
}

/// A cloud's selection: the saved JSON when there is one, else the flags.
fn selection<T: DeserializeOwned>(
    saved: Option<&str>,
    cloud: &str,
    from_flags: impl FnOnce() -> T,
) -> anyhow::Result<T> {
    match saved {
        Some(j) => serde_json::from_str(j).with_context(|| {
            format!("reading the saved {cloud} scan selection")
        }),
        None => Ok(from_flags()),
    }
}

impl Estate {
    /// The estate `provider` names, signed in with the flags' profile or
    /// tenant, selecting what `saved` selected, else what the flags do.
    pub(crate) async fn load(
        provider: Provider,
        args: &ScanArgs,
        saved: Option<&str>,
    ) -> anyhow::Result<Self> {
        Ok(match provider {
            Provider::Aws => Self::Aws {
                config: Box::new(
                    precip_aws::load_config(args.aws_profile.as_deref()).await,
                ),
                opts: selection(saved, "AWS", || args.aws_options())?,
            },
            Provider::Azure => {
                let opts = selection(saved, "Azure", || args.azure_options())?;
                Self::Azure {
                    credential: precip_azure::load_credential(
                        opts.tenant.as_deref(),
                        opts.cloud,
                    )
                    .context("loading Azure credentials")?,
                    opts,
                }
            }
            Provider::Gcp => Self::Gcp {
                credential: precip_gcp::load_credential()
                    .context("loading Google Cloud credentials")?,
                opts: selection(saved, "Google Cloud", || args.gcp_options())?,
            },
            Provider::Oci => Self::Oci {
                credential: precip_oci::load_credential(
                    args.oci_profile.as_deref(),
                )
                .context("loading OCI credentials")?,
                opts: selection(saved, "OCI", || args.oci_options())?,
            },
        })
    }

    fn provider(&self) -> Provider {
        match self {
            Self::Aws { .. } => Provider::Aws,
            Self::Azure { .. } => Provider::Azure,
            Self::Gcp { .. } => Provider::Gcp,
            Self::Oci { .. } => Provider::Oci,
        }
    }

    fn options_json(&self) -> serde_json::Result<String> {
        match self {
            Self::Aws { opts, .. } => serde_json::to_string(opts),
            Self::Azure { opts, .. } => serde_json::to_string(opts),
            Self::Gcp { opts, .. } => serde_json::to_string(opts),
            Self::Oci { opts, .. } => serde_json::to_string(opts),
        }
    }

    async fn scan(
        &self,
        progress: Option<precip_collect::ProgressSender>,
    ) -> anyhow::Result<precip_types::Scan> {
        Ok(match self {
            Self::Aws { config, opts } => {
                precip_aws::scan_estate(config, opts, progress).await?
            }
            Self::Azure { credential, opts } => {
                precip_azure::scan_estate(credential, opts, progress).await?
            }
            Self::Gcp { credential, opts } => {
                precip_gcp::scan_estate(credential, opts, progress).await?
            }
            Self::Oci { credential, opts } => {
                precip_oci::scan_estate(credential, opts, progress).await?
            }
        })
    }

    /// What the interface's header says the scan reads.
    pub(crate) fn describe(&self, args: &ScanArgs) -> String {
        match self {
            Self::Aws { .. } => match &args.aws_profile {
                Some(p) => format!("aws profile {p}"),
                None => "aws default chain".into(),
            },
            Self::Azure { opts, .. } => match &opts.tenant {
                Some(t) => format!("{} tenant {t}", opts.cloud.name()),
                None => format!("{} sign-in", opts.cloud.name()),
            },
            Self::Gcp { opts, .. } => {
                match (&opts.folder, &opts.organization) {
                    (Some(f), _) => format!("gcp folder {f}"),
                    (None, Some(o)) => format!("gcp organization {o}"),
                    (None, None) => "gcp projects".into(),
                }
            }
            Self::Oci { .. } => match &args.oci_profile {
                Some(p) => format!("oci profile {p}"),
                None => "oci default profile".into(),
            },
        }
    }
}

/// Scan, store, and group. Shared by the headless command and the TUI.
pub(crate) async fn scan_and_store(
    estate: Estate,
    store: precip_db::Store,
    progress: Option<precip_collect::ProgressSender>,
) -> anyhow::Result<precip_db::ScanStats> {
    store
        .save_scan_options(estate.provider(), &estate.options_json()?)
        .await
        .context("saving the scan selection")?;
    let scan = estate.scan(progress).await.context("scanning estate")?;
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
            !matches!(t.status, TargetStatus::Scanned | TargetStatus::Empty)
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
            "marked migrated but compute is still in the cloud"
        );
    }
    Ok(stats)
}

/// The profile to size with: the flag, else the workspace's remembered
/// choice, else the built-in. A remembered choice that no longer loads
/// falls back to the built-in rather than stopping the command.
pub(crate) async fn choose_profile(
    store: &precip_db::Store,
    flag: Option<String>,
) -> anyhow::Result<precip_types::Profile> {
    if let Some(name) = flag {
        return load_profile(&name);
    }
    let default = BuiltinProfile::DEFAULT.as_str();
    match store.sizing_profile().await? {
        Some(name) => match load_profile(&name) {
            Ok(p) => Ok(p),
            Err(e) => {
                tracing::warn!(
                    profile = %name,
                    error = format!("{e:#}"),
                    "remembered profile cannot be loaded; sizing with {default}"
                );
                load_profile(default)
            }
        },
        None => load_profile(default),
    }
}

/// A built-in profile by name, or a TOML file by path.
pub(crate) fn load_profile(
    name_or_path: &str,
) -> anyhow::Result<precip_types::Profile> {
    if let Ok(built_in) = name_or_path.parse::<BuiltinProfile>() {
        return built_in.profile().context("parsing built-in profile");
    }
    let text = std::fs::read_to_string(name_or_path)
        .with_context(|| format!("reading profile {name_or_path}"))?;
    precip_types::Profile::from_toml(&text)
        .with_context(|| format!("parsing profile {name_or_path}"))
}

pub(crate) async fn latest_scan(
    store: &precip_db::Store,
) -> anyhow::Result<precip_db::ScanSummary> {
    store
        .list_scans()
        .await?
        .into_iter()
        .next()
        .context("no scans in this workspace")
}

pub(crate) fn print_change(c: &precip_db::DispositionChange) {
    let change = match c.previous {
        Some(p) => format!("{p} -> {}", c.disposition),
        None => c.disposition.to_string(),
    };
    out!(
        "{}  {:<24} {:<8} {}",
        c.changed_at.strftime("%Y-%m-%d %H:%M:%S"),
        change,
        c.source,
        c.note.as_deref().unwrap_or("")
    );
}

pub(crate) async fn scan(env: &Env, args: &ScanArgs) -> anyhow::Result<()> {
    init_tracing();
    let db = env.db("headless scan")?;
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {parent}"))?;
    }
    let store = precip_db::Store::open(&db)
        .await
        .with_context(|| format!("opening {db}"))?;
    // With no cloud named, rescan every cloud that has a saved selection.
    let providers = match args.provider()? {
        Some(p) => vec![p],
        None => {
            let mut saved = Vec::new();
            for p in Provider::ALL {
                if store.saved_scan_options(*p).await?.is_some() {
                    saved.push(*p);
                }
            }
            if saved.is_empty() { vec![Provider::Aws] } else { saved }
        }
    };
    for provider in providers {
        // With no selection on the command line, repeat the last one.
        let saved = if args.is_default(provider) {
            store.saved_scan_options(provider).await?
        } else {
            None
        };
        if saved.is_some() {
            tracing::info!(%provider, "using the workspace's saved scan selection");
        }
        let estate = Estate::load(provider, args, saved.as_deref()).await?;
        scan_and_store(estate, store.clone(), None).await?;
    }
    Ok(())
}

pub(crate) async fn tui(env: Env, args: &ScanArgs) -> anyhow::Result<()> {
    // Logs would corrupt the terminal; the activity feed replaces
    // them. RUST_LOG still works for anyone redirecting stderr.
    if std::env::var_os("RUST_LOG").is_some() {
        init_tracing();
    }
    let provider = args.provider()?.unwrap_or(Provider::Aws);
    let estate = Estate::load(provider, args, None).await?;
    let context = precip_tui::Context {
        target: estate.describe(args),
        workspace: String::new(),
    };
    let launcher: precip_tui::Launcher = Box::new(move |store| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let estate = estate.clone();
        let done = tokio::spawn(async move {
            scan_and_store(estate, store, Some(tx))
                .await
                .map_err(|e| format!("{e:#}"))
        });
        precip_tui::ScanHandle { events: rx, done }
    });
    tokio::task::block_in_place(|| {
        let app = precip_tui::App::new(env.dir, env.db, context, launcher)?;
        app.run()
    })
    .context("running TUI")?;
    Ok(())
}

pub(crate) async fn group(env: &Env) -> anyhow::Result<()> {
    init_tracing();
    let store = env.store("group").await?;
    let latest = latest_scan(&store).await?;
    let stats = store
        .group_scan(&latest.id, &precip_types::GroupingConfig::default())
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
            row.disposition,
            row.resources,
            accounts.join(","),
            row.regions.join(",")
        );
    }
    Ok(())
}

pub(crate) async fn merge(
    env: &Env,
    alias: &str,
    into: &str,
) -> anyhow::Result<()> {
    init_tracing();
    let store = env.store("merge").await?;
    store.merge_workload(&alias, &into).await.context("merging")?;
    if let Some(latest) = store.list_scans().await?.into_iter().next() {
        let stats = store
            .group_scan(&latest.id, &precip_types::GroupingConfig::default())
            .await
            .context("regrouping")?;
        tracing::info!(
            alias,
            into,
            workloads = stats.workloads,
            "merged and regrouped"
        );
    }
    Ok(())
}

pub(crate) async fn disposition(
    env: &Env,
    pair: &PairArgs,
    disposition: Disposition,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let store = env.store("disposition").await?;
    store
        .set_disposition_with(
            &pair.workload,
            &pair.environment,
            disposition,
            DispositionSource::User,
            note,
        )
        .await
        .context("setting disposition")?;
    for c in
        store.disposition_history(&pair.workload, &pair.environment).await?
    {
        print_change(&c);
    }
    Ok(())
}

pub(crate) async fn default_disposition(
    env: &Env,
    disposition: Disposition,
    apply: bool,
) -> anyhow::Result<()> {
    let store = env.store("default-disposition").await?;
    store.set_default_disposition(disposition, apply).await?;
    out!(
        "new pairs start as {disposition}{}",
        if apply { "; pairs nobody set by hand changed too" } else { "" }
    );
    Ok(())
}

pub(crate) async fn workspaces(env: &Env) -> anyhow::Result<()> {
    for w in env.dir.list().context("listing workspaces")? {
        out!(
            "{:<32} {}",
            w.name,
            w.modified
                .and_then(|m| jiff::Timestamp::try_from(m).ok())
                .map(|t| t.strftime("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default()
        );
    }
    Ok(())
}

pub(crate) async fn overview(env: &Env) -> anyhow::Result<()> {
    let store = env.store("overview").await?;
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
    Ok(())
}

pub(crate) async fn scans(env: &Env) -> anyhow::Result<()> {
    let store = env.store("scans").await?;
    for s in store.list_scans().await? {
        out!(
            "#{:<4} {:<6} {}  {}  {}",
            s.seq,
            s.provider,
            s.started.strftime("%Y-%m-%d %H:%M"),
            s.finished
                .map(|f| f.strftime("%H:%M").to_string())
                .unwrap_or_else(|| "unfinished".into()),
            s.caller
        );
    }
    Ok(())
}

pub(crate) async fn accounts(env: &Env) -> anyhow::Result<()> {
    let store = env.store("accounts").await?;
    let latest = latest_scan(&store).await?;
    for (id, name) in store.account_names(latest.seq).await? {
        out!("{id}  {name}");
    }
    Ok(())
}

pub(crate) async fn workloads(env: &Env) -> anyhow::Result<()> {
    let store = env.store("workloads").await?;
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
            w.disposition,
            w.resources,
            w.accounts.join(","),
            w.regions.join(",")
        );
    }
    Ok(())
}

pub(crate) async fn members(env: &Env, pair: &PairArgs) -> anyhow::Result<()> {
    let store = env.store("members").await?;
    let latest = latest_scan(&store).await?;
    for m in store
        .pair_members(&latest.id, &pair.workload, &pair.environment)
        .await?
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
    Ok(())
}

pub(crate) async fn history(env: &Env, pair: &PairArgs) -> anyhow::Result<()> {
    let store = env.store("history").await?;
    for c in
        store.disposition_history(&pair.workload, &pair.environment).await?
    {
        print_change(&c);
    }
    Ok(())
}

pub(crate) async fn size(env: &Env, sizing: &SizingArgs) -> anyhow::Result<()> {
    init_tracing();
    let store = env.store("size").await?;
    let latest = latest_scan(&store).await?;
    let profile = choose_profile(&store, sizing.profile.clone()).await?;
    let assumptions = sizing.assumptions();
    let instances = store.sized_instances(&latest.id, &profile).await?;
    let inputs: Vec<Instance> = instances.iter().map(Into::into).collect();
    let result = Sizing::new(&profile, &inputs, &assumptions);
    let cap = result.sled_capacity;

    out!(
        "profile {}  oxide release {}  {} sleds per rack",
        profile.name,
        profile.oxide_release,
        profile.sleds_per_rack
    );
    out!(
        "assumptions: growth x{:.2}, threshold {:.0}%, N+{} per rack",
        sizing.growth,
        sizing.threshold * 100.0,
        sizing.headroom
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
    for d in WorkloadDemand::by_pair(&instances) {
        out!(
            "{:<28} {:<8} {:>5} {:>7} {:>9} {:>9}  {} fit as-is, {} need changes, {} cannot move",
            d.pair.workload,
            d.pair.environment,
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
        result.compute_sleds,
        result.storage_sleds,
        result.bound
    );
    out!(
        "sleds {} (including N+{})   racks {}   spare slots {}",
        result.sleds,
        sizing.headroom,
        result.racks,
        result.spare_sleds
    );
    if !result.not_sized.is_empty() {
        out!("not sized (disposition): {}", result.not_sized.len());
    }
    if !result.excluded.is_empty() {
        out!("");
        out!("cannot move as they are ({}):", result.excluded.len());
        for i in instances.iter().filter(|i| result.excluded.contains(&i.arn)) {
            out!(
                "  {} ({} {}): {}",
                i.name,
                i.instance_type,
                i.architecture,
                i.findings_text()
            );
        }
    }
    let remediations: Vec<_> =
        instances.iter().filter(|i| i.fit == Fit::NeedsRemediation).collect();
    if !remediations.is_empty() {
        out!("");
        out!("need changes before moving ({}):", remediations.len());
        for i in remediations {
            out!("  {} ({}): {}", i.name, i.instance_type, i.findings_text());
        }
    }
    Ok(())
}

pub(crate) async fn prune(env: &Env, all: bool) -> anyhow::Result<()> {
    init_tracing();
    let store = env.store("prune").await?;
    let latest = latest_scan(&store).await?;
    let removed = store.prune_stale_pairs(&latest.id, !all).await?;
    tracing::info!(removed, kept_user_changed = !all, "pruned stale pairs");
    Ok(())
}

pub(crate) async fn export(
    env: &Env,
    what: ExportWhat,
    out: Option<Utf8PathBuf>,
    format: Option<String>,
    pair: Option<(String, String)>,
    sizing: &SizingArgs,
) -> anyhow::Result<()> {
    // Warnings go to stderr so they never mix into an export
    // written to stdout.
    init_tracing();
    let store = env.store("export").await?;
    let format = match (&format, &out, what) {
        (Some(f), _, _) => f.to_lowercase(),
        (None, Some(path), _) => {
            path.extension().unwrap_or_default().to_lowercase()
        }
        (None, None, ExportWhat::Terraform) => "tf".into(),
        (
            None,
            None,
            ExportWhat::Workloads | ExportWhat::Sizing | ExportWhat::Schedule,
        ) => anyhow::bail!("give --out <file> or --format"),
    };
    let latest = latest_scan(&store).await?;
    let text = match (what, format.as_str()) {
        (ExportWhat::Workloads, "csv") => {
            export::workloads_csv(&store.workload_summary(&latest.id).await?)
        }
        (ExportWhat::Workloads, "json") => {
            export::workloads_json(&store.workload_summary(&latest.id).await?)?
        }
        (ExportWhat::Sizing, "csv" | "json") => {
            let profile =
                choose_profile(&store, sizing.profile.clone()).await?;
            let assumptions = sizing.assumptions();
            let instances = store.sized_instances(&latest.id, &profile).await?;
            if format == "csv" {
                export::sizing_csv(&instances)
            } else {
                let inputs: Vec<Instance> =
                    instances.iter().map(Into::into).collect();
                let result = Sizing::new(&profile, &inputs, &assumptions);
                let demand = WorkloadDemand::by_pair(&instances);
                export::sizing_json(
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
                store.compute_schedule(today(), Trigger::Export).await?;
            match format.as_str() {
                "csv" => export::schedule_csv(&schedule),
                "json" => export::schedule_json(&schedule)?,
                _ => export::schedule_ics(
                    &schedule,
                    env.workspace.as_deref().unwrap_or("precip"),
                    jiff::Timestamp::now(),
                ),
            }
        }
        (ExportWhat::Terraform, "tf") => {
            let Some((workload, environment)) = pair else {
                anyhow::bail!(
                    "terraform needs --workload <name> --environment <name>"
                );
            };
            let key = PairKey::new(workload.as_str(), environment.as_str());
            let summary = store.workload_summary(&latest.id).await?;
            if !summary.iter().any(|r| {
                r.workload == key.workload.as_str()
                    && r.environment == key.environment.as_str()
            }) {
                anyhow::bail!(
                    "no pair {workload} {environment} in the latest scan; see `precip workloads`"
                );
            }
            store.pair_terraform(&latest.id, &key).await?
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
    Ok(())
}

pub(crate) async fn profiles(
    env: &Env,
    use_profile: Option<String>,
) -> anyhow::Result<()> {
    if let Some(name) = use_profile {
        load_profile(&name)?;
        let store = env.store("profiles --use-profile").await?;
        store.set_sizing_profile(&name).await?;
        out!("sizing with {name}");
    } else {
        for p in BuiltinProfile::ALL {
            out!("{p}");
        }
    }
    Ok(())
}
