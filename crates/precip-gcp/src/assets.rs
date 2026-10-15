//! Cloud Asset Inventory. One `ListAssets` call, paged, returns every
//! resource of the listed types under a scope (an organization, a folder
//! or a project) with its full configuration, so a scope costs a few calls
//! however many projects and regions it covers.

use crate::api::{ApiError, Gcp};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

/// Rows per page; the service maximum.
const PAGE: u32 = 1000;

pub(crate) const INSTANCE: &str = "compute.googleapis.com/Instance";
pub(crate) const DISK: &str = "compute.googleapis.com/Disk";
pub(crate) const REGION_DISK: &str = "compute.googleapis.com/RegionDisk";
pub(crate) const IMAGE: &str = "compute.googleapis.com/Image";
pub(crate) const GROUP_MANAGER: &str =
    "compute.googleapis.com/InstanceGroupManager";
pub(crate) const TEMPLATE: &str = "compute.googleapis.com/InstanceTemplate";
pub(crate) const ADDRESS: &str = "compute.googleapis.com/Address";
pub(crate) const GLOBAL_ADDRESS: &str = "compute.googleapis.com/GlobalAddress";
pub(crate) const NETWORK: &str = "compute.googleapis.com/Network";
pub(crate) const SUBNETWORK: &str = "compute.googleapis.com/Subnetwork";
pub(crate) const FIREWALL: &str = "compute.googleapis.com/Firewall";
pub(crate) const ROUTE: &str = "compute.googleapis.com/Route";
pub(crate) const ROUTER: &str = "compute.googleapis.com/Router";
pub(crate) const BACKEND_SERVICE: &str =
    "compute.googleapis.com/BackendService";
pub(crate) const REGION_BACKEND_SERVICE: &str =
    "compute.googleapis.com/RegionBackendService";
pub(crate) const TARGET_POOL: &str = "compute.googleapis.com/TargetPool";
pub(crate) const GKE_CLUSTER: &str = "container.googleapis.com/Cluster";
pub(crate) const PROJECT: &str = "cloudresourcemanager.googleapis.com/Project";
pub(crate) const FOLDER: &str = "cloudresourcemanager.googleapis.com/Folder";
pub(crate) const ORGANIZATION: &str =
    "cloudresourcemanager.googleapis.com/Organization";

/// Every managed service type the managed collector reads.
pub(crate) const MANAGED: &[&str] = &[
    "sqladmin.googleapis.com/Instance",
    "alloydb.googleapis.com/Cluster",
    "spanner.googleapis.com/Instance",
    GKE_CLUSTER,
    "run.googleapis.com/Service",
    "cloudfunctions.googleapis.com/CloudFunction",
    "cloudfunctions.googleapis.com/Function",
    "redis.googleapis.com/Instance",
    "redis.googleapis.com/Cluster",
    "bigtableadmin.googleapis.com/Instance",
    "file.googleapis.com/Instance",
];

/// Every resource type a scan reads, besides the hierarchy.
pub(crate) fn resource_types() -> Vec<&'static str> {
    let mut types = vec![
        INSTANCE,
        DISK,
        REGION_DISK,
        IMAGE,
        GROUP_MANAGER,
        TEMPLATE,
        ADDRESS,
        GLOBAL_ADDRESS,
        NETWORK,
        SUBNETWORK,
        FIREWALL,
        ROUTE,
        ROUTER,
        BACKEND_SERVICE,
        REGION_BACKEND_SERVICE,
        TARGET_POOL,
    ];
    types.extend(MANAGED);
    types
}

/// One resource as Cloud Asset Inventory returns it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Asset {
    /// The full resource name.
    pub name: String,
    pub asset_type: String,
    pub resource: AssetResource,
    /// The project, folders and organization above the resource, nearest
    /// first, such as `projects/123`. Projects are named by number.
    pub ancestors: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct AssetResource {
    /// The resource as its own API returns it.
    pub data: Value,
    /// A zone, a region or `global`.
    pub location: String,
    /// The full name of the project, folder or organization above.
    pub parent: String,
}

impl Asset {
    /// The number of the project the resource is in.
    pub(crate) fn project_number(&self) -> Option<&str> {
        self.ancestors.iter().find_map(|a| a.strip_prefix("projects/"))
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Page {
    assets: Vec<Asset>,
}

/// Every asset of `types` under `scope`, such as `organizations/1` or
/// `projects/shop`.
pub(crate) async fn list(
    gcp: &Gcp,
    scope: &str,
    types: &[&str],
) -> Result<Vec<Asset>, ApiError> {
    let mut url = gcp.url("cloudasset", &format!("/v1/{scope}/assets"))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("contentType", "RESOURCE");
        q.append_pair("pageSize", &PAGE.to_string());
        for t in types {
            q.append_pair("assetTypes", t);
        }
    }
    let pages: Vec<Page> = gcp.pages(url).await?;
    Ok(pages.into_iter().flat_map(|p| p.assets).collect())
}

/// A scope's assets by type.
#[derive(Debug, Default)]
pub(crate) struct Inventory {
    by_type: HashMap<String, Vec<Asset>>,
}

impl Inventory {
    pub(crate) fn new(assets: impl IntoIterator<Item = Asset>) -> Self {
        let mut by_type: HashMap<String, Vec<Asset>> = HashMap::new();
        for a in assets {
            by_type.entry(a.asset_type.clone()).or_default().push(a);
        }
        Self { by_type }
    }

    /// The assets of one type.
    pub(crate) fn of(&self, asset_type: &str) -> &[Asset] {
        self.by_type.get(asset_type).map_or(&[], Vec::as_slice)
    }

    /// The assets of several types, in the order given.
    pub(crate) fn of_types<'a>(
        &'a self,
        types: &'a [&str],
    ) -> impl Iterator<Item = &'a Asset> {
        types.iter().flat_map(|t| self.of(t))
    }

    pub(crate) fn len(&self) -> usize {
        self.by_type.values().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::fake::{Script, ok};
    use std::sync::Arc;

    #[test]
    fn a_scope_is_listed_with_its_types_and_resource_content() {
        let script = Arc::new(Script::queue(vec![
            ok(serde_json::json!({
                "assets": [{"name": "//compute.googleapis.com/projects/p/zones/z/instances/a",
                            "assetType": INSTANCE, "ancestors": ["projects/1", "organizations/9"],
                            "resource": {"location": "z", "data": {"name": "a"}}}],
                "nextPageToken": "n",
            })),
            ok(
                serde_json::json!({"assets": [{"name": "b", "assetType": DISK}]}),
            ),
        ]));
        let gcp = Gcp::fake(script.clone());
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(list(&gcp, "organizations/9", &[INSTANCE, DISK]))
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].project_number(), Some("1"));
        assert_eq!(got[0].resource.data["name"], "a");
        let sent = script.sent();
        assert!(sent[0].starts_with(
            "https://gcp.test/cloudasset/v1/organizations/9/assets?contentType=RESOURCE&pageSize=1000"
        ));
        assert!(
            sent[0].contains("assetTypes=compute.googleapis.com%2FInstance")
        );
        assert!(sent[1].ends_with("pageToken=n"));
        let inv = Inventory::new(got);
        assert_eq!(inv.of(INSTANCE).len(), 1);
        assert_eq!(inv.of(ROUTE).len(), 0);
        assert_eq!(inv.of_types(&[INSTANCE, DISK]).count(), 2);
        assert_eq!(inv.len(), 2);
    }
}
