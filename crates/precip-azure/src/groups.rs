//! Resource groups, as deployment groups every resource in them belongs
//! to. A group another resource manages, such as an AKS cluster's node
//! group, takes that resource as its root so its contents join the
//! cluster's workload.

use crate::collect::{BASE, Base, Batch};
use crate::id::{resource_group, rid, text};
use precip_collect::{Collect, CollectError, Collected, Stage};
use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};
use serde::Deserialize;
use std::collections::HashSet;

const API: &str = "Microsoft.ResourceGraph/resources/read (resource groups)";

pub(crate) struct ResourceGroups;

impl Collect for ResourceGroups {
    type Context = Batch;
    const API: &'static str = API;
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &Batch,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let rows: Vec<Row> = ctx.graph(API, &query()).await?;
        Ok(assemble(&rows, prior))
    }
}

fn query() -> String {
    format!(
        "resourcecontainers
| where type =~ 'microsoft.resources/subscriptions/resourcegroups'
| project {BASE},
    state = tostring(properties.provisioningState),
    managedBy = tostring(managedBy)"
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Row {
    #[serde(flatten)]
    pub base: Base,
    pub state: String,
    pub managed_by: String,
}

fn assemble(rows: &[Row], prior: &Collected) -> Collected {
    let mut out = Collected::default();
    let mut groups = HashSet::new();
    for g in rows {
        let arn = rid(&g.base.id);
        groups.insert(arn.clone());
        out.resources.push(g.base.resource(Body::DeploymentGroup(
            body::DeploymentGroup {
                name: Some(g.base.name.clone()),
                parent_id: None,
                root_id: text(&g.managed_by).map(|m| rid(&m).to_string()),
                status: text(&g.state),
            },
        )));
    }
    for r in &prior.resources {
        let Some(group) = resource_group(r.arn.as_str()).map(Arn::from) else {
            continue;
        };
        if groups.contains(&group) {
            out.relate(r.arn.clone(), Edge::InDeploymentGroup, group);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_types::scan::Resource;
    use precip_types::{AccountId, Region};

    #[test]
    fn resources_join_their_group_and_managed_groups_name_their_root() {
        let rows: Vec<Row> = serde_json::from_value(serde_json::json!([
            {"id": "/subscriptions/s1/resourceGroups/Shop", "name": "Shop", "location": "eastus",
             "subscriptionId": "s1", "state": "Succeeded", "managedBy": ""},
            {"id": "/subscriptions/s1/resourceGroups/MC_shop_aks_eastus", "name": "MC_shop_aks_eastus",
             "location": "eastus", "subscriptionId": "s1",
             "managedBy": "/subscriptions/s1/resourceGroups/shop/providers/Microsoft.ContainerService/managedClusters/shop-aks"},
        ]))
        .unwrap();
        let res = |id: &str| Resource {
            arn: Arn::from(id),
            account_id: AccountId::from("s1"),
            region: Region::from("eastus"),
            tags: Default::default(),
            body: Body::Instance(Default::default()),
        };
        let prior = Collected {
            resources: vec![
                res(
                    "/subscriptions/s1/resourcegroups/shop/providers/microsoft.compute/virtualmachines/web",
                ),
                res(
                    "/subscriptions/s1/resourcegroups/gone/providers/microsoft.compute/virtualmachines/x",
                ),
                res("/providers/microsoft.compute/locations/eastus/vmsizes/d2"),
            ],
            ..Default::default()
        };
        let out = assemble(&rows, &prior);
        assert_eq!(
            out.relationships.len(),
            1,
            "only resources in a known group"
        );
        assert_eq!(
            out.relationships[0].to_arn,
            "/subscriptions/s1/resourcegroups/shop"
        );
        let Body::DeploymentGroup(mc) = &out.resources[1].body else {
            panic!()
        };
        assert_eq!(
            mc.root_id.as_deref(),
            Some(
                "/subscriptions/s1/resourcegroups/shop/providers/microsoft.containerservice/managedclusters/shop-aks"
            )
        );
    }
}
