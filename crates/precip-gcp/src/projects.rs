//! Projects, with the folder or organization each sits in. Under an
//! organization or folder they come from the hierarchy's own assets;
//! otherwise from Resource Manager's search, which returns every project
//! the caller can see.

use crate::api::{ApiError, Gcp};
use crate::assets::{self, Asset, FOLDER, ORGANIZATION, PROJECT};
use crate::id::{str_at, text};
use precip_types::scan::Resource;
use precip_types::{AccountId, Arn, Body, Region, body};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};

/// What `Account::cloud` records for every project.
pub(crate) const CLOUD: &str = "gcp";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Project {
    pub number: String,
    pub id: String,
    pub display_name: String,
    pub state: String,
    /// The folder or organization directly above, as `folders/123`.
    pub parent: Option<String>,
    pub parent_name: Option<String>,
}

impl Project {
    /// Projects being deleted answer no reads.
    pub(crate) fn is_active(&self) -> bool {
        self.state == "ACTIVE"
    }

    pub(crate) fn account_id(&self) -> AccountId {
        AccountId::from(self.id.as_str())
    }

    pub(crate) fn to_resource(&self) -> Resource {
        Resource {
            arn: Arn::from(format!(
                "//cloudresourcemanager.googleapis.com/projects/{}",
                self.number
            )),
            account_id: self.account_id(),
            region: Region::global(),
            tags: Default::default(),
            body: Body::Account(body::Account {
                name: self.display_name.clone(),
                status: self.state.clone(),
                parent_id: self.parent.clone(),
                parent_name: self.parent_name.clone(),
                cloud: CLOUD.into(),
            }),
        }
    }
}

/// The partial name of a parent from its full name:
/// `//cloudresourcemanager.googleapis.com/folders/1` is `folders/1`.
fn partial(full: &str) -> Option<String> {
    let rest = full.strip_prefix("//")?;
    text(rest.split_once('/')?.1)
}

/// Every project under `scope`, which is an organization or a folder,
/// with its parent's display name.
pub(crate) async fn under(
    gcp: &Gcp,
    scope: &str,
) -> Result<Vec<Project>, ApiError> {
    let got =
        assets::list(gcp, scope, &[PROJECT, FOLDER, ORGANIZATION]).await?;
    Ok(from_assets(&got))
}

fn from_assets(got: &[Asset]) -> Vec<Project> {
    let mut names: HashMap<String, String> = HashMap::new();
    for a in got {
        if a.asset_type == FOLDER || a.asset_type == ORGANIZATION {
            let d = &a.resource.data;
            if let (Some(name), Some(display)) =
                (str_at(d, "/name"), str_at(d, "/displayName"))
            {
                names.insert(name, display);
            }
        }
    }
    let mut out = Vec::new();
    for a in got.iter().filter(|a| a.asset_type == PROJECT) {
        let d = &a.resource.data;
        let Some(id) = str_at(d, "/projectId") else { continue };
        let parent = partial(&a.resource.parent);
        out.push(Project {
            number: str_at(d, "/projectNumber")
                .or_else(|| a.project_number().map(str::to_string))
                .unwrap_or_default(),
            display_name: str_at(d, "/name")
                .filter(|n| !n.starts_with("projects/"))
                .or_else(|| str_at(d, "/displayName"))
                .unwrap_or_else(|| id.clone()),
            id,
            state: str_at(d, "/lifecycleState")
                .or_else(|| str_at(d, "/state"))
                .unwrap_or_default(),
            parent_name: parent.as_ref().and_then(|p| names.get(p).cloned()),
            parent,
        });
    }
    out
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Found {
    name: String,
    project_id: String,
    display_name: String,
    state: String,
    parent: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SearchPage {
    projects: Vec<Found>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Container {
    display_name: String,
}

/// Every project the caller can see. Parent names need read access to
/// the folders and organization; without it they are left out.
pub(crate) async fn search(gcp: &Gcp) -> Result<Vec<Project>, ApiError> {
    let pages: Vec<SearchPage> = gcp
        .pages(gcp.url("cloudresourcemanager", "/v3/projects:search")?)
        .await?;
    let mut out: Vec<Project> = pages
        .into_iter()
        .flat_map(|p| p.projects)
        .map(|f| Project {
            number: f
                .name
                .strip_prefix("projects/")
                .unwrap_or(&f.name)
                .to_string(),
            display_name: text(&f.display_name)
                .unwrap_or_else(|| f.project_id.clone()),
            id: f.project_id,
            state: f.state,
            parent: text(&f.parent),
            parent_name: None,
        })
        .collect();
    let parents: BTreeSet<String> =
        out.iter().filter_map(|p| p.parent.clone()).collect();
    let mut names = HashMap::new();
    for parent in parents {
        let url = gcp.url("cloudresourcemanager", &format!("/v3/{parent}"))?;
        match gcp.get::<Container>(url).await {
            Ok(c) => {
                if let Some(n) = text(&c.display_name) {
                    names.insert(parent, n);
                }
            }
            Err(e) => tracing::warn!(
                parent,
                error = %precip_collect::error_chain(&e),
                "reading a project's folder or organization"
            ),
        }
    }
    for p in &mut out {
        p.parent_name = p.parent.as_ref().and_then(|x| names.get(x).cloned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::fake::{Script, ok, status};
    use std::sync::Arc;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn hierarchy_assets_give_each_project_its_folders_name() {
        let got: Vec<Asset> = serde_json::from_value(serde_json::json!([
            {"name": "//cloudresourcemanager.googleapis.com/projects/111", "assetType": PROJECT,
             "ancestors": ["projects/111", "folders/5", "organizations/9"],
             "resource": {"parent": "//cloudresourcemanager.googleapis.com/folders/5",
                          "data": {"projectId": "shop-prod", "projectNumber": "111",
                                   "name": "Shop Prod", "lifecycleState": "ACTIVE"}}},
            {"name": "//cloudresourcemanager.googleapis.com/projects/222", "assetType": PROJECT,
             "ancestors": ["projects/222", "organizations/9"],
             "resource": {"parent": "//cloudresourcemanager.googleapis.com/organizations/9",
                          "data": {"projectId": "old", "lifecycleState": "DELETE_REQUESTED"}}},
            {"name": "//cloudresourcemanager.googleapis.com/folders/5", "assetType": FOLDER,
             "resource": {"data": {"name": "folders/5", "displayName": "Production"}}},
            {"name": "x", "assetType": PROJECT, "resource": {"data": {}}},
        ]))
        .unwrap();
        let projects = from_assets(&got);
        assert_eq!(projects.len(), 2, "a project with no id is skipped");
        let shop = &projects[0];
        assert_eq!(
            (
                shop.id.as_str(),
                shop.number.as_str(),
                shop.display_name.as_str()
            ),
            ("shop-prod", "111", "Shop Prod")
        );
        assert_eq!(shop.parent.as_deref(), Some("folders/5"));
        assert_eq!(shop.parent_name.as_deref(), Some("Production"));
        assert!(shop.is_active());
        let old = &projects[1];
        assert_eq!(old.number, "222", "the number falls back to the ancestors");
        assert_eq!(old.display_name, "old");
        assert!(!old.is_active());
        assert_eq!(old.parent_name, None);
    }

    #[test]
    fn search_names_parents_it_can_read_and_skips_the_rest() {
        let script = Arc::new(Script::routes(vec![
            (
                "projects:search",
                ok(serde_json::json!({"projects": [
                    {"name": "projects/1", "projectId": "a", "displayName": "A",
                     "state": "ACTIVE", "parent": "folders/5"},
                    {"name": "projects/2", "projectId": "b", "state": "ACTIVE",
                     "parent": "organizations/9"},
                ]})),
            ),
            ("/v3/folders/5", ok(serde_json::json!({"displayName": "Prod"}))),
            (
                "/v3/organizations/9",
                status(
                    403,
                    serde_json::json!({"error": {"status": "PERMISSION_DENIED"}}),
                ),
            ),
        ]));
        let gcp = Gcp::fake(script);
        let projects = block_on(search(&gcp)).unwrap();
        assert_eq!(projects[0].parent_name.as_deref(), Some("Prod"));
        assert_eq!(projects[1].parent_name, None);
        assert_eq!(projects[1].display_name, "b");
        let r = projects[0].to_resource();
        assert_eq!(r.account_id, "a");
        assert_eq!(r.region, "global");
        let Body::Account(acct) = &r.body else { panic!() };
        assert_eq!(acct.cloud, "gcp");
        assert_eq!(acct.parent_name.as_deref(), Some("Prod"));
    }
}
