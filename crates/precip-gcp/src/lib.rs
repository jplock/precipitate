//! Read-only Google Cloud discovery. With an organization or folder,
//! precip lists every resource under it from Cloud Asset Inventory in one
//! paged call and splits the result by project. Without one, it finds
//! every project the caller can see and lists each project the same way.
//! Asset Inventory covers every region at once, so a project is one scan
//! target.

mod api;
mod assets;
mod balancers;
mod collect;
mod compute;
mod gke;
mod id;
mod machines;
mod managed;
mod network;
mod projects;

use api::Gcp;
use assets::Inventory;
use google_cloud_auth::build_errors::Error as BuildError;
use google_cloud_auth::credentials::{Builder, Credentials};
use precip_collect::http::{HttpError, Request};
use precip_collect::{
    Collected, ProgressSender, emit, error_chain, finish_batch, selected,
};
use precip_types::scan::{ScanTarget, TargetStatus};
use precip_types::{AccountId, Provider, Scan, ScanEvent};
use projects::Project;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// The permission a failed inventory listing is reported under.
const LIST_API: &str = "cloudasset.assets.listResource";
/// Projects listed at once when each is its own scope.
const PROJECTS_AT_ONCE: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("loading Google Cloud credentials")]
    Credential(#[source] BuildError),
    #[error("starting the HTTP client")]
    Client(#[source] HttpError),
    #[error("listing projects")]
    Projects(#[source] HttpError),
    #[error("no active projects match the selection")]
    NoProjects,
}

/// Which projects to scan. Saved with each scan so a later scan with no
/// flags repeats the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// Only projects under this organization, by number. The whole
    /// organization is listed in one pass.
    pub organization: Option<String>,
    /// Only projects under this folder, at any depth, by number.
    pub folder: Option<String>,
    /// Only these projects, by id. Empty means every one.
    pub projects: Vec<AccountId>,
    pub exclude_projects: Vec<AccountId>,
}

impl Options {
    /// The organization or folder to list under, such as `folders/123`.
    /// A folder is the narrower of the two.
    fn scope(&self) -> Option<String> {
        let named = |kind: &str, v: &str| {
            let v = v.trim();
            if v.starts_with(&format!("{kind}/")) {
                v.to_string()
            } else {
                format!("{kind}/{v}")
            }
        };
        self.folder.as_deref().map(|f| named("folders", f)).or_else(|| {
            self.organization.as_deref().map(|o| named("organizations", o))
        })
    }
}

/// The credential a scan signs in with.
#[derive(Debug, Clone)]
pub struct Credential(Credentials);

/// Application Default Credentials: `GOOGLE_APPLICATION_CREDENTIALS` (a
/// service account key, workload identity federation or impersonation),
/// else `gcloud auth application-default login`, else the metadata server
/// on Google Cloud. `GOOGLE_CLOUD_QUOTA_PROJECT` names the project the
/// calls are billed and rate-limited against.
pub fn load_credential() -> Result<Credential, ScanError> {
    Builder::default().build().map(Credential).map_err(ScanError::Credential)
}

/// Scan every selected project.
pub async fn scan_estate(
    credential: &Credential,
    opts: &Options,
    progress: Option<ProgressSender>,
) -> Result<Scan, ScanError> {
    let gcp = Gcp::new(credential.0.clone()).map_err(ScanError::Client)?;
    scan_with(&gcp, opts, progress).await
}

async fn scan_with(
    gcp: &Gcp,
    opts: &Options,
    progress: Option<ProgressSender>,
) -> Result<Scan, ScanError> {
    let mut scan = Scan {
        id: uuid::Uuid::now_v7().to_string(),
        provider: Provider::Gcp,
        started: jiff::Timestamp::now(),
        finished: None,
        caller: caller(gcp).await,
        targets: Vec::new(),
        resources: Vec::new(),
        relationships: Vec::new(),
    };

    let scope = opts.scope();
    let all = match &scope {
        Some(s) => projects::under(gcp, s).await,
        None => projects::search(gcp).await,
    }
    .map_err(ScanError::Projects)?;
    let selected = select(all, opts);
    if selected.is_empty() {
        return Err(ScanError::NoProjects);
    }
    emit(&progress, ScanEvent::AccountsDiscovered { total: selected.len() });
    scan.resources.extend(selected.iter().map(Project::to_resource));

    let batches: Vec<(String, Vec<Project>)> = match scope {
        Some(s) => vec![(s, selected)],
        None => selected
            .into_iter()
            .map(|p| (format!("projects/{}", p.id), vec![p]))
            .collect(),
    };
    let at_once = Arc::new(Semaphore::new(PROJECTS_AT_ONCE));
    let mut tasks = tokio::task::JoinSet::new();
    for (scope, projects) in batches {
        let gcp = gcp.clone();
        let progress = progress.clone();
        let at_once = at_once.clone();
        tasks.spawn(async move {
            let _permit = at_once.acquire().await;
            scan_batch(gcp, &scope, &projects, &progress).await
        });
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((targets, got)) => {
                scan.targets.extend(targets);
                scan.resources.extend(got.resources);
                scan.relationships.extend(got.relationships);
            }
            Err(e) => tracing::error!(error = %e, "project scan task failed"),
        }
    }
    scan.targets.sort_by(|a, b| a.account_id.cmp(&b.account_id));
    scan.finished = Some(jiff::Timestamp::now());
    let failed = scan
        .targets
        .iter()
        .filter(|t| matches!(t.status, TargetStatus::Failed(_)))
        .count();
    tracing::info!(
        projects = scan.targets.len(),
        failed,
        resources = scan.resources.len(),
        "Google Cloud scan finished"
    );
    Ok(scan)
}

/// List one scope's assets, keep those in `projects`, and run the
/// collectors over them. A failed listing fails every project in it.
async fn scan_batch(
    gcp: Gcp,
    scope: &str,
    projects: &[Project],
    progress: &Option<ProgressSender>,
) -> (Vec<ScanTarget>, Collected) {
    for p in projects {
        emit(
            progress,
            ScanEvent::AccountStarted {
                account_id: p.account_id(),
                name: Some(p.display_name.clone()),
            },
        );
    }
    let project_ids: HashMap<String, String> =
        projects.iter().map(|p| (p.number.clone(), p.id.clone())).collect();
    let (got, status) = match assets::list(
        &gcp,
        scope,
        &assets::resource_types(),
    )
    .await
    {
        Ok(listed) => {
            // Under an organization or folder, projects left out of the
            // selection are listed too.
            let kept = listed.into_iter().filter(|a| {
                a.project_number().is_some_and(|n| project_ids.contains_key(n))
            });
            let inventory = Inventory::new(kept);
            tracing::info!(scope, assets = inventory.len(), "listed assets");
            let ctx = collect::Batch { gcp, inventory, project_ids };
            precip_collect::run(collect::all(), Arc::new(ctx)).await
        }
        Err(e) => {
            tracing::warn!(scope, error = %error_chain(&e), "listing assets");
            (
                Collected::default(),
                TargetStatus::Failed(format!(
                    "{LIST_API}: {}",
                    error_chain(&e)
                )),
            )
        }
    };
    let accounts: Vec<AccountId> =
        projects.iter().map(Project::account_id).collect();
    let targets = finish_batch(progress, &accounts, &got, &status);
    (targets, got)
}

/// The active projects `opts` selects, sorted by id.
fn select(all: Vec<Project>, opts: &Options) -> Vec<Project> {
    let mut out: Vec<Project> = all
        .into_iter()
        .filter(|p| {
            p.is_active()
                && selected(
                    &[p.id.as_str(), p.number.as_str()],
                    &opts.projects,
                    &opts.exclude_projects,
                )
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Who the token was issued to, from Google's token information endpoint:
/// the account's email, else the client id. This is a label for the scan,
/// so a failure is logged and the scan goes on.
async fn caller(gcp: &Gcp) -> String {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Info {
        email: String,
        azp: String,
    }
    let info = async {
        let token = gcp.token().await?;
        let mut url = gcp.url("oauth2", "/tokeninfo")?;
        let form = url
            .query_pairs_mut()
            .append_pair("access_token", &token)
            .finish()
            .query()
            .unwrap_or_default()
            .to_string();
        url.set_query(None);
        let body = gcp
            .send(&Request::post(
                url,
                form.into_bytes(),
                "application/x-www-form-urlencoded",
            ))
            .await?;
        Ok::<Info, HttpError>(serde_json::from_slice(&body)?)
    }
    .await;
    match info {
        Ok(i) if !i.email.is_empty() => i.email,
        Ok(i) if !i.azp.is_empty() => i.azp,
        Ok(_) => "unknown".into(),
        Err(e) => {
            tracing::warn!(error = %error_chain(&e), "reading who the token belongs to");
            "unknown".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_collect::http::fake::{Canned, Script, ok, status};

    fn project(id: &str, number: &str, state: &str) -> Project {
        Project {
            number: number.into(),
            id: id.into(),
            display_name: id.into(),
            state: state.into(),
            parent: None,
            parent_name: None,
        }
    }

    #[test]
    fn selection_keeps_active_listed_projects_by_id_or_number() {
        let all = || {
            vec![
                project("b", "2", "ACTIVE"),
                project("a", "1", "ACTIVE"),
                project("gone", "3", "DELETE_REQUESTED"),
            ]
        };
        let ids = |v: Vec<Project>| -> Vec<String> {
            v.into_iter().map(|p| p.id).collect()
        };
        assert_eq!(ids(select(all(), &Options::default())), ["a", "b"]);
        let only = Options {
            projects: vec!["2".into(), "gone".into()],
            ..Default::default()
        };
        assert_eq!(ids(select(all(), &only)), ["b"]);
        let except = Options {
            exclude_projects: vec!["a".into()],
            ..Default::default()
        };
        assert_eq!(ids(select(all(), &except)), ["b"]);
    }

    #[test]
    fn the_scope_is_the_folder_else_the_organization() {
        assert_eq!(Options::default().scope(), None);
        let org =
            Options { organization: Some("9".into()), ..Default::default() };
        assert_eq!(org.scope().as_deref(), Some("organizations/9"));
        let both = Options {
            organization: Some("organizations/9".into()),
            folder: Some("5".into()),
            ..Default::default()
        };
        assert_eq!(both.scope().as_deref(), Some("folders/5"));
    }

    #[test]
    fn saved_options_round_trip() {
        let o = Options {
            organization: Some("9".into()),
            projects: vec!["shop".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&o).unwrap();
        assert_eq!(serde_json::from_str::<Options>(&json).unwrap(), o);
        assert_eq!(
            serde_json::from_str::<Options>("{}").unwrap(),
            Options::default()
        );
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    fn hierarchy() -> Canned {
        ok(serde_json::json!({"assets": [
            {"name": "//cloudresourcemanager.googleapis.com/projects/111",
             "assetType": "cloudresourcemanager.googleapis.com/Project",
             "ancestors": ["projects/111", "organizations/9"],
             "resource": {"parent": "//cloudresourcemanager.googleapis.com/organizations/9",
                          "data": {"projectId": "shop", "projectNumber": "111", "name": "Shop",
                                   "lifecycleState": "ACTIVE"}}},
            {"name": "//cloudresourcemanager.googleapis.com/projects/222",
             "assetType": "cloudresourcemanager.googleapis.com/Project",
             "ancestors": ["projects/222", "organizations/9"],
             "resource": {"data": {"projectId": "skip", "projectNumber": "222",
                                   "lifecycleState": "ACTIVE"}}},
            {"name": "//cloudresourcemanager.googleapis.com/organizations/9",
             "assetType": "cloudresourcemanager.googleapis.com/Organization",
             "resource": {"data": {"name": "organizations/9", "displayName": "example.com"}}},
        ]}))
    }

    /// A whole scan of an organization against canned answers: one VM and
    /// its disk in the selected project, one VM in an excluded one.
    #[test]
    fn an_organization_scan_lists_once_and_splits_by_project() {
        let p = "//compute.googleapis.com/projects";
        let script = Arc::new(Script::routes(vec![
            ("tokeninfo", ok(serde_json::json!({"email": "ana@example.com"}))),
            ("Organization", hierarchy()),
            (
                "assets?",
                ok(serde_json::json!({"assets": [
                    {"name": format!("{p}/shop/zones/us-east1-b/instances/web-1"),
                     "assetType": "compute.googleapis.com/Instance",
                     "ancestors": ["projects/111", "organizations/9"],
                     "resource": {"location": "us-east1-b", "data": {
                         "machineType": "https://www.googleapis.com/compute/v1/projects/shop/zones/us-east1-b/machineTypes/e2-medium"}}},
                    {"name": format!("{p}/shop/zones/us-east1-b/disks/web-1"),
                     "assetType": "compute.googleapis.com/Disk",
                     "ancestors": ["projects/111", "organizations/9"],
                     "resource": {"location": "us-east1-b", "data": {
                         "sizeGb": "10",
                         "users": ["https://www.googleapis.com/compute/v1/projects/shop/zones/us-east1-b/instances/web-1"]}}},
                    {"name": format!("{p}/skip/zones/us-east1-b/instances/x"),
                     "assetType": "compute.googleapis.com/Instance",
                     "ancestors": ["projects/222", "organizations/9"],
                     "resource": {"location": "us-east1-b", "data": {}}},
                ]})),
            ),
            (
                "machineTypes",
                ok(
                    serde_json::json!({"items": {"zones/us-east1-b": {"machineTypes": [
                        {"name": "e2-medium", "guestCpus": 2, "memoryMb": 4096},
                    ]}}}),
                ),
            ),
        ]));
        let gcp = Gcp::fake(script.clone());
        let opts = Options {
            organization: Some("9".into()),
            exclude_projects: vec!["skip".into()],
            ..Default::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let scan = block_on(scan_with(&gcp, &opts, Some(tx))).unwrap();

        assert_eq!(scan.provider, Provider::Gcp);
        assert_eq!(scan.caller, "ana@example.com");
        assert_eq!(scan.targets.len(), 1);
        assert_eq!(scan.targets[0].account_id, "shop");
        assert_eq!(scan.targets[0].status, TargetStatus::Scanned);
        let mut kinds: Vec<&str> =
            scan.resources.iter().map(|r| r.kind().as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, ["account", "instance", "instance_type", "volume"]);
        let Some(account) =
            scan.resources.iter().find(|r| r.kind().as_str() == "account")
        else {
            panic!()
        };
        let precip_types::Body::Account(a) = &account.body else { panic!() };
        assert_eq!(a.parent_name.as_deref(), Some("example.com"));
        assert!(
            scan.relationships.iter().any(|r| r.edge.as_str() == "attached_to")
        );

        let listings =
            script.urls().iter().filter(|u| u.contains("/assets?")).count();
        assert_eq!(listings, 2, "the hierarchy, then every resource at once");
        let tokeninfo = script
            .sent()
            .into_iter()
            .find(|s| s.url.contains("tokeninfo"))
            .unwrap();
        assert!(
            !tokeninfo.url.contains("access_token"),
            "the token goes in the body"
        );
        assert_eq!(tokeninfo.body.as_deref(), Some("access_token=token"));

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert!(matches!(
            events.first(),
            Some(ScanEvent::AccountsDiscovered { total: 1 })
        ));
        assert!(matches!(
            events.last(),
            Some(ScanEvent::AccountFinished { resources: 3, .. })
        ));
    }

    #[test]
    fn a_failed_listing_fails_its_project_and_not_the_others() {
        let script = Arc::new(Script::routes(vec![
            ("tokeninfo", status(400, serde_json::json!({}))),
            (
                "projects:search",
                ok(serde_json::json!({"projects": [
                    {"name": "projects/1", "projectId": "a", "state": "ACTIVE"},
                    {"name": "projects/2", "projectId": "b", "state": "ACTIVE"},
                ]})),
            ),
            (
                "projects/a/assets",
                status(
                    403,
                    serde_json::json!({"error": {"status": "PERMISSION_DENIED", "message": "Cloud Asset API has not been used"}}),
                ),
            ),
            ("projects/b/assets", ok(serde_json::json!({}))),
        ]));
        let gcp = Gcp::fake(script);
        let scan =
            block_on(scan_with(&gcp, &Options::default(), None)).unwrap();
        assert_eq!(scan.caller, "unknown");
        let status: Vec<(&str, &TargetStatus)> = scan
            .targets
            .iter()
            .map(|t| (t.account_id.as_str(), &t.status))
            .collect();
        let TargetStatus::Failed(why) = status[0].1 else {
            panic!("{status:?}")
        };
        assert!(
            why.starts_with(LIST_API) && why.contains("PERMISSION_DENIED"),
            "{why}"
        );
        assert_eq!(status[1], ("b", &TargetStatus::Scanned));
    }

    #[test]
    fn no_matching_project_is_an_error_and_a_failed_search_is_reported() {
        let empty = Arc::new(Script::routes(vec![(
            "projects:search",
            ok(serde_json::json!({"projects": []})),
        )]));
        let err =
            block_on(scan_with(&Gcp::fake(empty), &Options::default(), None))
                .unwrap_err();
        assert!(matches!(err, ScanError::NoProjects));
        let denied = Arc::new(Script::routes(vec![(
            "projects:search",
            status(
                403,
                serde_json::json!({"error": {"status": "PERMISSION_DENIED"}}),
            ),
        )]));
        let err =
            block_on(scan_with(&Gcp::fake(denied), &Options::default(), None))
                .unwrap_err();
        assert!(matches!(err, ScanError::Projects(_)));
    }
}
