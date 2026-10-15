//! The context every Google Cloud collector reads from, and the registry.

use crate::api::Gcp;
use crate::assets::{Asset, Inventory};
use crate::id::{labels, project, region, rid};
use crate::{balancers, compute, gke, machines, managed, network};
use precip_collect::Collector;
use precip_types::scan::Resource;
use precip_types::{AccountId, Arn, Body, Region};
use std::collections::HashMap;
use std::sync::Arc;

/// One scope's assets, already listed: every collector but machine types
/// reads only these.
pub(crate) struct Batch {
    pub gcp: Gcp,
    pub inventory: Inventory,
    /// Project number to project id, for the projects in the batch.
    pub project_ids: HashMap<String, String>,
}

impl Batch {
    /// The project an asset is in, by id.
    pub(crate) fn account(&self, asset: &Asset) -> AccountId {
        let id = asset
            .project_number()
            .and_then(|n| self.project_ids.get(n))
            .map(String::as_str)
            .or_else(|| project(&asset.name))
            .unwrap_or_default();
        AccountId::from(id)
    }

    /// An asset as a resource, tagged with its labels. Cloud Run keeps
    /// them under `metadata`.
    pub(crate) fn resource(&self, asset: &Asset, body: Body) -> Resource {
        let data = &asset.resource.data;
        Resource {
            arn: rid(&asset.name),
            account_id: self.account(asset),
            region: Region::from(region(&asset.resource.location)),
            tags: labels(
                data.get("labels").or_else(|| data.pointer("/metadata/labels")),
            ),
            body,
        }
    }

    /// A resource nested in an asset (a peering, a firewall rule, a NAT),
    /// which takes its project and region from the asset but not its
    /// labels.
    pub(crate) fn child(
        &self,
        asset: &Asset,
        id: &str,
        body: Body,
    ) -> Resource {
        Resource {
            arn: Arn::from(id),
            account_id: self.account(asset),
            region: Region::from(region(&asset.resource.location)),
            tags: Default::default(),
            body,
        }
    }

    /// A name whose project may be given by number, such as an instance's
    /// `created-by` metadata, with the number replaced by the id.
    pub(crate) fn by_id(&self, name: &str) -> Arn {
        let full = rid(name);
        let Some(number) = project(full.as_str()) else { return full };
        match self.project_ids.get(number) {
            Some(id) => Arn::from(full.as_str().replacen(
                &format!("/projects/{number}/"),
                &format!("/projects/{id}/"),
                1,
            )),
            None => full,
        }
    }
}

/// Every collector. Second-stage collectors run after the rest, in this
/// order.
pub(crate) fn all() -> Vec<Arc<dyn Collector<Batch>>> {
    vec![
        Arc::new(compute::Compute),
        Arc::new(network::Network),
        Arc::new(balancers::LoadBalancers),
        Arc::new(managed::Managed),
        // Second stage, in order: each reads what ran before it.
        Arc::new(machines::MachineTypes),
        Arc::new(gke::Clusters),
    ]
}

#[cfg(test)]
pub(crate) mod test {
    //! Batches built from JSON assets.

    use super::*;
    use crate::api::fake::Script;

    pub(crate) fn batch(assets: serde_json::Value) -> Batch {
        batch_with(assets, Arc::new(Script::default()))
    }

    pub(crate) fn batch_with(
        assets: serde_json::Value,
        script: Arc<Script>,
    ) -> Batch {
        let assets: Vec<Asset> =
            serde_json::from_value(assets).expect("assets parse");
        Batch {
            gcp: Gcp::fake(script),
            inventory: Inventory::new(assets),
            project_ids: HashMap::from([(
                "111".to_string(),
                "shop".to_string(),
            )]),
        }
    }

    /// An asset of `asset_type` named `name` in project `shop` (number
    /// 111) at `location`.
    pub(crate) fn asset(
        asset_type: &str,
        name: &str,
        location: &str,
        data: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "assetType": asset_type,
            "ancestors": ["projects/111", "organizations/9"],
            "resource": {"location": location, "data": data},
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test::{asset, batch};
    use crate::assets::INSTANCE;

    #[test]
    fn projects_are_named_by_id_and_numbers_in_names_are_replaced() {
        let b = batch(serde_json::json!([asset(
            INSTANCE,
            "//compute.googleapis.com/projects/shop/zones/us-east1-b/instances/a",
            "us-east1-b",
            serde_json::json!({"labels": {"app": "shop"}}),
        )]));
        let a = &b.inventory.of(INSTANCE)[0];
        let r = b.resource(a, precip_types::Body::Instance(Default::default()));
        assert_eq!(r.account_id, "shop");
        assert_eq!(r.region, "us-east1");
        assert_eq!(r.tags["app"], "shop");
        assert_eq!(
            b.by_id("projects/111/zones/us-east1-b/instanceGroupManagers/web")
                .as_str(),
            "//compute.googleapis.com/projects/shop/zones/us-east1-b/instanceGroupManagers/web"
        );
        assert_eq!(
            b.by_id("projects/222/zones/z/instanceGroupManagers/x").as_str(),
            "//compute.googleapis.com/projects/222/zones/z/instanceGroupManagers/x",
            "an unknown number is kept"
        );
    }

    #[test]
    fn an_asset_without_ancestors_takes_its_project_from_its_name() {
        let mut v = asset(
            INSTANCE,
            "//compute.googleapis.com/projects/other/zones/z/instances/a",
            "z",
            serde_json::json!({}),
        );
        v["ancestors"] = serde_json::json!([]);
        let b = batch(serde_json::json!([v]));
        assert_eq!(b.account(&b.inventory.of(INSTANCE)[0]), "other");
    }
}
