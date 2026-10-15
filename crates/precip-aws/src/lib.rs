//! Read-only discovery of an AWS estate. Every call here is a describe or
//! list; the shipped IAM policy grants nothing else.

pub mod accounts;
pub mod arn;
pub mod autoscaling;
pub mod cloudformation;
pub mod collect;
pub mod ec2;
pub mod elb;
pub mod explorer;
pub mod managed;
pub mod tagging;
pub mod vpc;

use aws_config::{BehaviorVersion, SdkConfig};
use precip_types::scan::{Relationship, Resource, ScanTarget, TargetStatus};
use precip_types::{AccountId, Region};
use precip_types::{Scan, ScanEvent};
use tracing::Instrument;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("calling sts:GetCallerIdentity")]
    CallerIdentity(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("listing regions")]
    Regions(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("listing organization accounts")]
    Organization(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("sts:GetCallerIdentity returned no {0}")]
    IdentityField(&'static str),
    #[error("caller ARN has no partition: {0}")]
    Partition(String),
    #[error("scan task failed: {0}")]
    Join(String),
}

/// Accounts scanned at once. API rate limits are per account, so this
/// bounds only local resources and the STS calls that assume roles.
const ACCOUNT_CONCURRENCY: usize = 8;
/// Regions scanned at once within one account. EC2 request rate limits are
/// per account per region, so regions do not contend with each other.
const REGION_CONCURRENCY: usize = 8;

/// Roles AWS itself creates in member accounts: the first by
/// Organizations when it creates an account, the second by Control Tower.
pub const DEFAULT_MEMBER_ROLES: &[&str] =
    &["OrganizationAccountAccessRole", "AWSControlTowerExecution"];

/// Scan options from the command line.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Options {
    /// Roles to try in each member account, in order. The first one that
    /// can be assumed is used.
    pub member_roles: Vec<String>,
    /// Scan only these account ids. Empty means every active account.
    pub accounts: Vec<AccountId>,
    /// Never scan these account ids.
    pub exclude_accounts: Vec<AccountId>,
    /// Treat the caller's account as a lone account even if it is an
    /// Organization management account.
    pub single_account: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            member_roles: DEFAULT_MEMBER_ROLES
                .iter()
                .map(|r| r.to_string())
                .collect(),
            accounts: Vec::new(),
            exclude_accounts: Vec::new(),
            single_account: false,
        }
    }
}

/// The error and every source below it, joined, so a stored failure says
/// why (throttled, access denied, timeout) and not only which API.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        parts.push(s.to_string());
        cur = s.source();
    }
    parts.dedup();
    parts.join(": ")
}

/// Receives progress events. Dropped receivers are ignored, so a headless
/// scan can pass `None`.
pub type ProgressSender = tokio::sync::mpsc::UnboundedSender<ScanEvent>;

fn emit(progress: &Option<ProgressSender>, event: ScanEvent) {
    if let Some(tx) = progress {
        let _ = tx.send(event);
    }
}

/// Loads the default AWS credential chain, the same one the AWS CLI uses.
pub async fn load_config(profile: Option<&str>) -> SdkConfig {
    // Many regions and accounts are described at once, so throttling is
    // expected. Adaptive mode rate-limits client side and retries with
    // backoff; the default standard mode gives up after three attempts.
    let retry = aws_config::retry::RetryConfig::adaptive().with_max_attempts(8);
    let mut loader =
        aws_config::defaults(BehaviorVersion::latest()).retry_config(retry);
    if let Some(p) = profile {
        loader = loader.profile_name(p);
    }
    loader.load().await
}

/// Scan the whole estate the credentials can reach. In a management
/// account this is the management account plus every selected active
/// member, each through the member role. Otherwise it is the one account.
pub async fn scan_estate(
    config: &SdkConfig,
    opts: &Options,
    progress: Option<ProgressSender>,
) -> Result<Scan, ScanError> {
    let identity = accounts::caller_identity(config).await?;
    let mut scan = Scan {
        id: uuid::Uuid::now_v7().to_string(),
        started: jiff::Timestamp::now(),
        finished: None,
        caller_arn: identity.arn.clone(),
        targets: Vec::new(),
        resources: Vec::new(),
        relationships: Vec::new(),
    };

    let in_org = !opts.single_account
        && accounts::is_management_account(config, &identity.account_id).await;

    let members = if in_org {
        let all = accounts::list_accounts(config).await?;
        for a in &all {
            scan.resources.push(a.to_resource(&identity.partition));
        }
        all
    } else {
        Vec::new()
    };

    let selected: Vec<accounts::MemberAccount> = members
        .into_iter()
        .filter(|a| a.id != identity.account_id)
        .filter(|a| opts.accounts.is_empty() || opts.accounts.contains(&a.id))
        .filter(|a| !opts.exclude_accounts.contains(&a.id))
        .collect();

    tracing::info!(
        account = %identity.account_id,
        organization = in_org,
        members = selected.len(),
        "starting scan"
    );
    emit(
        &progress,
        ScanEvent::AccountsDiscovered { total: selected.len() + 1 },
    );

    // One aggregator for the whole organization, if the management
    // account's Resource Explorer can see member accounts.
    let org_explorer = match explorer::Explorer::discover(config).await {
        Some(e)
            if in_org && e.sees_other_accounts(&identity.account_id).await =>
        {
            Some(e)
        }
        _ => None,
    };

    let limit =
        std::sync::Arc::new(tokio::sync::Semaphore::new(ACCOUNT_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();

    // The caller's own account, with the caller's own credentials.
    {
        let config = config.clone();
        let identity = identity.clone();
        let own_name = scan
            .resources
            .iter()
            .find(|r| r.account_id == identity.account_id)
            .and_then(|r| r.name().map(str::to_string));
        let limit = limit.clone();
        let progress = progress.clone();
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await;
            scan_account(&config, identity, own_name, None, progress).await
        });
    }

    for member in selected {
        let base = config.clone();
        let partition = identity.partition.clone();
        let roles = opts.member_roles.clone();
        let limit = limit.clone();
        let progress = progress.clone();
        let org_explorer = org_explorer.clone();
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await;
            if member.status != "ACTIVE" {
                let reason = format!("account status {}", member.status);
                emit(
                    &progress,
                    ScanEvent::AccountSkipped {
                        account_id: member.id.clone(),
                        reason: reason.clone(),
                    },
                );
                return AccountOutcome::skipped(&member.id, reason);
            }
            // The first call with an assumed role is the one that fails if
            // the role is missing or untrusted, so try each candidate until
            // GetCallerIdentity succeeds.
            let mut errors = Vec::new();
            for role in &roles {
                let config = accounts::assume_member_role(
                    &base, &partition, &member.id, role,
                )
                .await;
                match accounts::caller_identity(&config).await {
                    Ok(identity) => {
                        tracing::info!(account = %member.id, role,
                            "assumed member role");
                        return scan_account(
                            &config,
                            identity,
                            Some(member.name.clone()),
                            org_explorer.as_ref(),
                            progress,
                        )
                        .await;
                    }
                    Err(e) => errors.push(format!("{role}: {e}")),
                }
            }
            tracing::warn!(account = %member.id,
                errors = errors.join("; "), "could not assume any member role");
            let reason = format!("assume role: {}", errors.join("; "));
            emit(
                &progress,
                ScanEvent::AccountSkipped {
                    account_id: member.id.clone(),
                    reason: reason.clone(),
                },
            );
            AccountOutcome::skipped(&member.id, reason)
        });
    }

    let mut outcomes = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => return Err(ScanError::Join(e.to_string())),
        }
    }
    outcomes.sort_by(|a, b| a.account_id.cmp(&b.account_id));
    for o in outcomes {
        scan.targets.extend(o.targets);
        scan.resources.extend(o.resources);
        scan.relationships.extend(o.relationships);
    }

    scan.finished = Some(jiff::Timestamp::now());
    Ok(scan)
}

struct AccountOutcome {
    account_id: AccountId,
    targets: Vec<ScanTarget>,
    resources: Vec<Resource>,
    relationships: Vec<Relationship>,
}

impl AccountOutcome {
    /// An account that could not be scanned at all. Recorded as one failed
    /// target with region `*` so the report counts it.
    fn skipped(account_id: &AccountId, reason: String) -> Self {
        Self {
            account_id: account_id.clone(),
            targets: vec![ScanTarget {
                account_id: account_id.clone(),
                region: Region::every(),
                status: TargetStatus::Failed(reason),
            }],
            resources: Vec::new(),
            relationships: Vec::new(),
        }
    }
}

/// Every enabled region of one account, scanned concurrently.
#[tracing::instrument(skip_all, fields(account = %identity.account_id))]
async fn scan_account(
    config: &SdkConfig,
    identity: accounts::CallerIdentity,
    name: Option<String>,
    org_explorer: Option<&explorer::Explorer>,
    progress: Option<ProgressSender>,
) -> AccountOutcome {
    emit(
        &progress,
        ScanEvent::AccountStarted {
            account_id: identity.account_id.clone(),
            name,
        },
    );
    let regions = match ec2::enabled_regions(config).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not list regions");
            let reason = format!("list regions: {e}");
            emit(
                &progress,
                ScanEvent::AccountSkipped {
                    account_id: identity.account_id.clone(),
                    reason: reason.clone(),
                },
            );
            return AccountOutcome::skipped(&identity.account_id, reason);
        }
    };

    // Skip regions Resource Explorer knows to be empty: ask the
    // organization-wide aggregator about this account, else the account's
    // own aggregator. Without either, scan every enabled region.
    let pruned = match org_explorer {
        Some(e) => {
            e.regions_with_resources(Some(&identity.account_id), &regions).await
        }
        None => match explorer::Explorer::discover(config).await {
            Some(e) => e.regions_with_resources(None, &regions).await,
            None => None,
        },
    };
    let mut skipped: Vec<Region> = Vec::new();
    let regions = match pruned {
        Some(active) => {
            skipped = regions
                .iter()
                .filter(|r| !active.contains(r))
                .cloned()
                .collect();
            active
        }
        None => regions,
    };
    emit(
        &progress,
        ScanEvent::RegionsPlanned {
            account_id: identity.account_id.clone(),
            regions: regions.len(),
            skipped: skipped.len(),
        },
    );

    let limit =
        std::sync::Arc::new(tokio::sync::Semaphore::new(REGION_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    let mut task_regions = std::collections::HashMap::new();
    for region in regions {
        let config = config.clone();
        let identity = identity.clone();
        let limit = limit.clone();
        let progress = progress.clone();
        let task_region = region.clone();
        let handle = tasks.spawn(async move {
            let _permit = limit.acquire_owned().await;
            let outcome = scan_region(&config, identity.clone(), region).await;
            emit(
                &progress,
                ScanEvent::RegionFinished {
                    account_id: identity.account_id.clone(),
                    region: outcome.region.clone(),
                    status: outcome.status.clone(),
                    resources: outcome.resources.len(),
                },
            );
            outcome
        });
        task_regions.insert(handle.id(), task_region);
    }

    let mut outcomes = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => {
                let region = task_regions.remove(&e.id()).unwrap_or_default();
                tracing::error!(%region, error = %e, "region task panicked");
                // The region is reported as failed rather than left out,
                // so the scan does not read as complete.
                let status =
                    TargetStatus::Failed(format!("region scan panicked: {e}"));
                emit(
                    &progress,
                    ScanEvent::RegionFinished {
                        account_id: identity.account_id.clone(),
                        region: region.clone(),
                        status: status.clone(),
                        resources: 0,
                    },
                );
                outcomes.push(RegionOutcome {
                    region,
                    status,
                    resources: Vec::new(),
                    relationships: Vec::new(),
                });
            }
        }
    }
    // Deterministic order regardless of which region finished first.
    outcomes.sort_by(|a, b| a.region.cmp(&b.region));

    let mut out = AccountOutcome {
        account_id: identity.account_id.clone(),
        targets: skipped
            .into_iter()
            .map(|region| ScanTarget {
                account_id: identity.account_id.clone(),
                region,
                status: TargetStatus::Empty,
            })
            .collect(),
        resources: Vec::new(),
        relationships: Vec::new(),
    };
    for o in outcomes {
        out.resources.extend(o.resources);
        out.relationships.extend(o.relationships);
        out.targets.push(ScanTarget {
            account_id: identity.account_id.clone(),
            region: o.region,
            status: o.status,
        });
    }
    emit(
        &progress,
        ScanEvent::AccountFinished {
            account_id: identity.account_id.clone(),
            resources: out.resources.len(),
        },
    );
    out
}

struct RegionOutcome {
    region: Region,
    status: TargetStatus,
    resources: Vec<Resource>,
    relationships: Vec<Relationship>,
}

/// Run every collector for one region. Collectors that do not depend on
/// earlier results run concurrently; the rest run once those finish. A
/// failed collector marks the region partial; all failing marks it failed.
/// Nothing is dropped silently.
#[tracing::instrument(skip_all, fields(account = %identity.account_id, region = %region))]
async fn scan_region(
    config: &SdkConfig,
    identity: accounts::CallerIdentity,
    region: Region,
) -> RegionOutcome {
    let ctx = std::sync::Arc::new(collect::RegionContext {
        config: config
            .to_builder()
            .region(aws_config::Region::new(region.to_string()))
            .build(),
        identity,
        region: region.clone(),
    });
    let collectors = collect::all();
    let total = collectors.len();
    let (second, first): (Vec<_>, Vec<_>) = collectors
        .into_iter()
        .partition(|c| c.stage() == collect::Stage::Second);

    let mut failures: Vec<String> = Vec::new();
    let empty = std::sync::Arc::new(collect::Collected::default());
    let mut so_far = run_stage(first, &ctx, &empty, &mut failures).await;

    // Second-stage collectors may depend on each other (instance types
    // come from launch templates), so they run one at a time in order,
    // each reading everything collected before it.
    for c in second {
        match c.collect(&ctx, &so_far).await {
            Ok(got) => {
                log_collected(c.name(), &got);
                so_far.absorb(got);
            }
            Err(e) => record_failure(&mut failures, c.name(), &e),
        }
    }

    let status = if failures.is_empty() {
        TargetStatus::Scanned
    } else if failures.len() == total {
        TargetStatus::Failed(failures.join(", "))
    } else {
        TargetStatus::Partial(failures.join(", "))
    };
    RegionOutcome {
        region,
        status,
        resources: so_far.resources,
        relationships: so_far.relationships,
    }
}

/// Run a set of collectors concurrently and merge their output in a fixed
/// order so results are deterministic.
async fn run_stage(
    collectors: Vec<std::sync::Arc<dyn collect::Collector>>,
    ctx: &std::sync::Arc<collect::RegionContext>,
    prior: &std::sync::Arc<collect::Collected>,
    failures: &mut Vec<String>,
) -> collect::Collected {
    let mut tasks = tokio::task::JoinSet::new();
    let mut names = std::collections::HashMap::new();
    for (i, c) in collectors.into_iter().enumerate() {
        let ctx = ctx.clone();
        let prior = prior.clone();
        let span = tracing::Span::current();
        let name = c.name();
        let handle = tasks.spawn(
            async move {
                let result = c.collect(&ctx, &prior).await;
                (i, c.name(), result)
            }
            .instrument(span),
        );
        names.insert(handle.id(), name);
    }
    let mut results = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(r) => results.push(r),
            Err(e) => {
                let name = names.get(&e.id()).copied().unwrap_or("collector");
                tracing::error!(api = name, error = %e, "collector task panicked");
                failures.push(format!("{name}: collector panicked: {e}"));
            }
        }
    }
    results.sort_by_key(|(i, _, _)| *i);

    let mut out = collect::Collected::default();
    for (_, name, result) in results {
        match result {
            Ok(got) => {
                log_collected(name, &got);
                out.absorb(got);
            }
            Err(e) => record_failure(failures, name, &e),
        }
    }
    out
}

/// Log what one collector brought back.
fn log_collected(api: &'static str, got: &collect::Collected) {
    tracing::info!(
        api,
        resources = got.resources.len(),
        relationships = got.relationships.len(),
        tag_updates = got.tag_updates.len(),
        "collected"
    );
}

/// Note a failed collector for the region's report.
fn record_failure(
    failures: &mut Vec<String>,
    name: &'static str,
    e: &collect::CollectError,
) {
    tracing::warn!(api = name, error = %e.source, "collector failed");
    failures.push(format!("{}: {}", e.api, error_chain(&*e.source)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use collect::{Collect, CollectError, Collected, RegionContext};

    struct Panics;

    impl Collect for Panics {
        const API: &'static str = "test:Panics";

        async fn collect(
            &self,
            _ctx: &RegionContext,
            _prior: &Collected,
        ) -> Result<Collected, CollectError> {
            panic!("collector bug")
        }
    }

    struct Works;

    impl Collect for Works {
        const API: &'static str = "test:Works";

        async fn collect(
            &self,
            ctx: &RegionContext,
            _prior: &Collected,
        ) -> Result<Collected, CollectError> {
            let mut out = Collected::default();
            out.resources.push(ctx.resource(
                ctx.arn(arn::ArnType::Instance, "i-1"),
                Default::default(),
                precip_types::Body::Ec2Instance(Default::default()),
            ));
            Ok(out)
        }
    }

    #[test]
    fn a_panicking_collector_is_recorded_as_a_failure() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let ctx = std::sync::Arc::new(RegionContext {
            config: SdkConfig::builder().build(),
            identity: accounts::CallerIdentity {
                account_id: "123456789012".into(),
                arn: "arn:aws:iam::123456789012:user/x".into(),
                partition: "aws".into(),
            },
            region: "us-east-1".into(),
        });
        let prior = std::sync::Arc::new(Collected::default());
        let mut failures = Vec::new();
        let got = rt.block_on(run_stage(
            vec![std::sync::Arc::new(Panics), std::sync::Arc::new(Works)],
            &ctx,
            &prior,
            &mut failures,
        ));
        assert_eq!(got.resources.len(), 1);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].starts_with("test:Panics: collector panicked"));
    }
}
