//! GKE clusters as deployment groups. A cluster's node pools run on
//! managed instance groups named after the pool (`gke-orders-pool-…-grp`),
//! so without this their nodes would take the group's name as their
//! workload. The cluster, its groups and their instances join one group
//! whose root is the cluster.

use crate::assets::GKE_CLUSTER;
use crate::collect::Batch;
use crate::id::{items_at, name, rid, str_at, strs_at};
use precip_collect::{Collect, CollectError, Collected, Stage};
use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};
use std::collections::HashSet;

const API: &str = "cloudasset.assets.listResource (GKE node groups)";

pub(crate) struct Clusters;

impl Collect for Clusters {
    type Context = Batch;
    const API: &'static str = API;
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &Batch,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        Ok(assemble(ctx, prior))
    }
}

fn assemble(ctx: &Batch, prior: &Collected) -> Collected {
    let mut out = Collected::default();
    for cluster in ctx.inventory.of(GKE_CLUSTER) {
        let arn = rid(&cluster.name);
        let group = Arn::from(format!("{arn}#nodes"));
        out.resources.push(ctx.child(
            cluster,
            group.as_str(),
            Body::DeploymentGroup(body::DeploymentGroup {
                name: Some(name(&cluster.name).to_string()),
                parent_id: None,
                root_id: Some(arn.to_string()),
                status: str_at(&cluster.resource.data, "/status"),
            }),
        ));
        out.relate(arn, Edge::InDeploymentGroup, group.clone());
        let managers: HashSet<Arn> =
            items_at(&cluster.resource.data, "/nodePools")
                .iter()
                .flat_map(|p| strs_at(p, "/instanceGroupUrls"))
                .map(rid)
                .collect();
        for m in &managers {
            out.relate(m.clone(), Edge::InDeploymentGroup, group.clone());
        }
        for r in &prior.relationships {
            if r.edge == Edge::MemberOfScaleGroup
                && managers.contains(&r.to_arn)
            {
                out.relate(
                    r.from_arn.clone(),
                    Edge::InDeploymentGroup,
                    group.clone(),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::test::{asset, batch};

    #[test]
    fn a_clusters_node_groups_and_nodes_join_its_deployment_group() {
        let cluster = "//container.googleapis.com/projects/shop/locations/us-east1/clusters/orders";
        let mig = "//compute.googleapis.com/projects/shop/zones/us-east1-b/instanceGroupManagers/gke-orders-pool-1-grp";
        let ctx = batch(serde_json::json!([asset(
            GKE_CLUSTER,
            cluster,
            "us-east1",
            serde_json::json!({
                "status": "RUNNING",
                "nodePools": [{"instanceGroupUrls": [
                    "https://www.googleapis.com/compute/v1/projects/shop/zones/us-east1-b/instanceGroupManagers/gke-orders-pool-1-grp",
                ]}],
            })
        )]));
        let mut prior = Collected::default();
        prior.relate(
            Arn::from("node-1"),
            Edge::MemberOfScaleGroup,
            Arn::from(mig),
        );
        prior.relate(
            Arn::from("other"),
            Edge::MemberOfScaleGroup,
            Arn::from("web-mig"),
        );
        let out = assemble(&ctx, &prior);
        let Body::DeploymentGroup(g) = &out.resources[0].body else { panic!() };
        assert_eq!(g.name.as_deref(), Some("orders"));
        assert_eq!(g.root_id.as_deref(), Some(cluster));
        assert_eq!(out.resources[0].arn, format!("{cluster}#nodes").as_str());
        let members: HashSet<&str> =
            out.relationships.iter().map(|r| r.from_arn.as_str()).collect();
        assert_eq!(members, HashSet::from([cluster, mig, "node-1"]));
    }
}
