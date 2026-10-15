//! Read-only Oracle Cloud Infrastructure discovery. precip lists the
//! tenancy's compartments and subscribed regions, asks Resource Search
//! which compartments have resources in each region, and lists instances,
//! volumes and networks in those compartment and region pairs.

mod api;
mod collect;
mod compute;
mod config;
mod network;
mod search;
mod sign;
mod tenancy;

use api::Oci;
pub use config::ConfigError;
use precip_collect::http::HttpError;
use precip_collect::{Collected, ProgressSender, emit, scan_regions, selected};
use precip_types::scan::{ScanTarget, TargetStatus};
use precip_types::{Provider, Region, Scan, ScanEvent};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tenancy::{Compartment, RegionPlan};
use tokio::sync::Semaphore;

/// Compartments scanned at once, and regions at once in each.
const COMPARTMENTS_AT_ONCE: usize = 8;
const REGIONS_AT_ONCE: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("loading the OCI configuration")]
    Config(#[source] ConfigError),
    #[error("starting the HTTP client")]
    Client(#[source] HttpError),
    #[error("listing compartments")]
    Compartments(#[source] HttpError),
    #[error("listing subscribed regions")]
    Regions(#[source] HttpError),
    #[error("no active compartments match the selection")]
    NoCompartments,
    #[error("no subscribed regions match the selection")]
    NoRegions,
}

/// Which compartments and regions to scan. Saved with each scan so a
/// later scan with no flags repeats the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// Only these compartments, by OCID or name. Empty means every one.
    pub compartments: Vec<String>,
    pub exclude_compartments: Vec<String>,
    /// Only these subscribed regions. Empty means every one.
    pub regions: Vec<Region>,
}

/// A signed-in client for one tenancy.
#[derive(Debug, Clone)]
pub struct Credential {
    oci: Oci,
    tenancy: String,
    /// The profile's region, where identity calls go.
    region: String,
    realm: String,
    caller: String,
}

/// Sign in with a profile of the OCI CLI's configuration file: `profile`,
/// else `OCI_CLI_PROFILE`, else `DEFAULT`, from `OCI_CLI_CONFIG_FILE` or
/// `~/.oci/config`. A profile with `security_token_file` signs in with
/// the session `oci session authenticate` created.
pub fn load_credential(profile: Option<&str>) -> Result<Credential, ScanError> {
    let name = config::profile_name(profile);
    let p = config::load(&config::default_path(), &name)
        .map_err(ScanError::Config)?;
    let (signer, caller) =
        config::signer(&p, &name).map_err(ScanError::Config)?;
    let domain = config::domain(&p.tenancy).map_err(ScanError::Config)?;
    let realm = config::realm(&p.tenancy).map_err(ScanError::Config)?;
    Ok(Credential {
        oci: Oci::new(signer, domain).map_err(ScanError::Client)?,
        tenancy: p.tenancy,
        region: p.region,
        realm,
        caller,
    })
}

/// Scan every selected compartment in every selected region.
pub async fn scan_estate(
    credential: &Credential,
    opts: &Options,
    progress: Option<ProgressSender>,
) -> Result<Scan, ScanError> {
    let c = credential;
    let mut scan = Scan {
        id: uuid::Uuid::now_v7().to_string(),
        provider: Provider::Oci,
        started: jiff::Timestamp::now(),
        finished: None,
        caller: c.caller.clone(),
        targets: Vec::new(),
        resources: Vec::new(),
        relationships: Vec::new(),
    };
    let all = tenancy::compartments(&c.oci, &c.region, &c.tenancy)
        .await
        .map_err(ScanError::Compartments)?;
    let compartments = select(all, opts);
    if compartments.is_empty() {
        return Err(ScanError::NoCompartments);
    }
    let regions: Vec<String> = tenancy::regions(&c.oci, &c.region, &c.tenancy)
        .await
        .map_err(ScanError::Regions)?
        .into_iter()
        .filter(|r| {
            opts.regions.is_empty()
                || opts.regions.iter().any(|o| o.as_str() == r)
        })
        .collect();
    if regions.is_empty() {
        return Err(ScanError::NoRegions);
    }
    emit(
        &progress,
        ScanEvent::AccountsDiscovered { total: compartments.len() },
    );
    scan.resources.extend(compartments.iter().map(|x| x.to_resource(&c.realm)));

    let mut plans = Vec::new();
    for r in &regions {
        plans.push(tenancy::plan(&c.oci, r, &c.tenancy).await);
    }
    let plans = Arc::new(plans);
    let at_once = Arc::new(Semaphore::new(COMPARTMENTS_AT_ONCE));
    let mut tasks = tokio::task::JoinSet::new();
    for compartment in compartments {
        let oci = c.oci.clone();
        let plans = plans.clone();
        let progress = progress.clone();
        let at_once = at_once.clone();
        tasks.spawn(async move {
            let _permit = at_once.acquire().await;
            scan_compartment(oci, compartment, &plans, &progress).await
        });
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((targets, got)) => {
                scan.targets.extend(targets);
                scan.resources.extend(got.resources);
                scan.relationships.extend(got.relationships);
            }
            Err(e) => {
                tracing::error!(error = %e, "compartment scan task failed")
            }
        }
    }
    scan.targets.sort_by(|a, b| {
        (&a.account_id, &a.region).cmp(&(&b.account_id, &b.region))
    });
    scan.finished = Some(jiff::Timestamp::now());
    tracing::info!(
        targets = scan.targets.len(),
        resources = scan.resources.len(),
        "OCI scan finished"
    );
    Ok(scan)
}

/// One compartment's regions: skipped where Resource Search found
/// nothing, failed where the region's availability domains are unknown,
/// otherwise collected.
async fn scan_compartment(
    oci: Oci,
    compartment: Compartment,
    plans: &[RegionPlan],
    progress: &Option<ProgressSender>,
) -> (Vec<ScanTarget>, Collected) {
    let account = compartment.account_id();
    emit(
        progress,
        ScanEvent::AccountStarted {
            account_id: account.clone(),
            name: Some(compartment.name.clone()),
        },
    );
    let empty = |p: &RegionPlan| {
        p.occupied.as_ref().is_some_and(|o| !o.contains(&compartment.id))
    };
    let (skipped, planned): (Vec<&RegionPlan>, Vec<&RegionPlan>) =
        plans.iter().partition(|p| empty(p));
    let domains: HashMap<String, Result<Vec<String>, String>> =
        planned.iter().map(|p| (p.region.clone(), p.domains.clone())).collect();
    scan_regions(
        progress,
        &account,
        planned.iter().map(|p| Region::from(p.region.as_str())).collect(),
        skipped.iter().map(|p| Region::from(p.region.as_str())).collect(),
        REGIONS_AT_ONCE,
        |region| {
            let oci = oci.clone();
            let compartment = compartment.id.clone();
            let domains = domains.get(region.as_str()).cloned();
            async move {
                match domains {
                    Some(Ok(domains)) => {
                        let ctx = collect::Pair {
                            oci,
                            region: region.to_string(),
                            compartment,
                            domains,
                        };
                        precip_collect::run(collect::all(), Arc::new(ctx)).await
                    }
                    Some(Err(why)) => {
                        (Collected::default(), TargetStatus::Failed(why))
                    }
                    None => (Collected::default(), TargetStatus::Empty),
                }
            }
        },
    )
    .await
}

/// name.
fn select(all: Vec<Compartment>, opts: &Options) -> Vec<Compartment> {
    let mut out: Vec<Compartment> = all
        .into_iter()
        .filter(|c| {
            c.state == "ACTIVE"
                && selected(
                    &[c.id.as_str(), c.name.as_str()],
                    &opts.compartments,
                    &opts.exclude_compartments,
                )
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_collect::http::fake::{Script, ok, status};

    fn compartment(id: &str, name: &str) -> Compartment {
        Compartment {
            id: id.into(),
            name: name.into(),
            state: "ACTIVE".into(),
            parent: None,
            parent_name: None,
        }
    }

    #[test]
    fn selection_matches_ocids_or_names_and_skips_inactive() {
        let all = || {
            let mut gone = compartment("c.old", "old");
            gone.state = "DELETED".into();
            vec![compartment("c.b", "b"), compartment("c.a", "a"), gone]
        };
        let names = |v: Vec<Compartment>| -> Vec<String> {
            v.into_iter().map(|c| c.name).collect()
        };
        assert_eq!(names(select(all(), &Options::default())), ["a", "b"]);
        let only = Options {
            compartments: vec!["c.b".into(), "old".into()],
            ..Default::default()
        };
        assert_eq!(names(select(all(), &only)), ["b"]);
        let except = Options {
            exclude_compartments: vec!["a".into()],
            ..Default::default()
        };
        assert_eq!(names(select(all(), &except)), ["b"]);
    }

    fn credential(script: Arc<Script>) -> Credential {
        Credential {
            oci: Oci::fake(script),
            tenancy: "t".into(),
            region: "us-ashburn-1".into(),
            realm: "oc1".into(),
            caller: "ocid1.user.oc1..u".into(),
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt")
            .block_on(f)
    }

    /// Two regions, two compartments: Resource Search finds only `shop`
    /// in Ashburn, so every other pair is recorded empty, and Phoenix's
    /// search failing means `shop` is scanned there anyway.
    #[test]
    fn a_scan_collects_occupied_pairs_and_records_the_rest_empty() {
        let script = Arc::new(Script::routes(vec![
            (
                "/compartments",
                ok(
                    serde_json::json!([{"id": "c.shop", "compartmentId": "t", "name": "shop", "lifecycleState": "ACTIVE"}]),
                ),
            ),
            (
                "/tenancies/t/regionSubscriptions",
                ok(serde_json::json!([
                    {"regionName": "us-ashburn-1", "status": "READY"},
                    {"regionName": "us-phoenix-1", "status": "READY"},
                ])),
            ),
            ("/tenancies/t", ok(serde_json::json!({"name": "acme"}))),
            ("availabilityDomains", ok(serde_json::json!([{"name": "AD-1"}]))),
            (
                "query.us-ashburn-1",
                ok(
                    serde_json::json!({"items": [{"compartmentId": "c.shop", "lifecycleState": "RUNNING"}]}),
                ),
            ),
            (
                "query.us-phoenix-1",
                status(
                    404,
                    serde_json::json!({"code": "NotAuthorizedOrNotFound"}),
                ),
            ),
            (
                "iaas.us-ashburn-1.oraclecloud.com/20160918/instances?compartmentId=c.shop",
                ok(serde_json::json!([
                    {"id": "i.web", "shape": "VM.Standard.E4.Flex", "lifecycleState": "RUNNING",
                     "shapeConfig": {"ocpus": 1.0, "memoryInGBs": 8.0, "processorDescription": "AMD"}},
                ])),
            ),
            ("/20160918/", ok(serde_json::json!([]))),
        ]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let scan = block_on(scan_estate(
            &credential(script.clone()),
            &Options::default(),
            Some(tx),
        ))
        .unwrap();
        assert_eq!(scan.provider, Provider::Oci);
        assert_eq!(scan.caller, "ocid1.user.oc1..u");
        let targets: Vec<(&str, &str, &TargetStatus)> = scan
            .targets
            .iter()
            .map(|t| (t.account_id.as_str(), t.region.as_str(), &t.status))
            .collect();
        assert_eq!(
            targets,
            [
                ("c.shop", "us-ashburn-1", &TargetStatus::Scanned),
                ("c.shop", "us-phoenix-1", &TargetStatus::Scanned),
                ("t", "us-ashburn-1", &TargetStatus::Empty),
                ("t", "us-phoenix-1", &TargetStatus::Scanned),
            ]
        );
        let mut kinds: Vec<&str> =
            scan.resources.iter().map(|r| r.kind().as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, ["account", "account", "instance", "instance_type"]);
        assert!(
            !script.urls().iter().any(|u| u.contains("iaas.us-ashburn-1")
                && u.contains("compartmentId=t&")),
            "the empty pair is not listed"
        );
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert!(matches!(
            events.first(),
            Some(ScanEvent::AccountsDiscovered { total: 2 })
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            ScanEvent::RegionsPlanned { skipped: 1, .. }
        )));
    }

    #[test]
    fn unreadable_domains_fail_the_region_and_bad_selections_are_errors() {
        let script = Arc::new(Script::routes(vec![
            ("/compartments", ok(serde_json::json!([]))),
            (
                "/tenancies/t/regionSubscriptions",
                ok(
                    serde_json::json!([{"regionName": "us-ashburn-1", "status": "READY"}]),
                ),
            ),
            (
                "availabilityDomains",
                status(
                    404,
                    serde_json::json!({"code": "NotAuthorizedOrNotFound"}),
                ),
            ),
        ]));
        let c = credential(script);
        let scan =
            block_on(scan_estate(&c, &Options::default(), None)).unwrap();
        let TargetStatus::Failed(why) = &scan.targets[0].status else {
            panic!("{:?}", scan.targets)
        };
        assert!(why.contains("availability domains"), "{why}");
        let nobody =
            Options { compartments: vec!["nope".into()], ..Default::default() };
        assert!(matches!(
            block_on(scan_estate(&c, &nobody, None)),
            Err(ScanError::NoCompartments)
        ));
        let nowhere = Options {
            regions: vec!["eu-paris-1".into()],
            ..Default::default()
        };
        assert!(matches!(
            block_on(scan_estate(&c, &nowhere, None)),
            Err(ScanError::NoRegions)
        ));
    }

    #[test]
    fn saved_options_round_trip() {
        let o = Options {
            compartments: vec!["shop".into()],
            regions: vec!["us-ashburn-1".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<Options>(&json).unwrap(), o);
        assert_eq!(
            serde_json::from_str::<Options>("{}").unwrap(),
            Options::default()
        );
    }
}
