//! Load balancers. A backend service is the balancer and each instance
//! group or network endpoint group behind it a target group; instances
//! reach a group through the managed instance group that created them.
//! Legacy target pools name their instances directly.
//!
//! Members of unmanaged instance groups are not in the inventory, so
//! their groups count no instances.

use crate::assets::{
    Asset, BACKEND_SERVICE, GROUP_MANAGER, INSTANCE, REGION_BACKEND_SERVICE,
    TARGET_POOL,
};
use crate::collect::Batch;
use crate::compute::group_manager;
use crate::id::{items_at, name, project, region, rid, str_at, strs_at};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::scan::{Edge, Resource};
use precip_types::{AccountId, Arn, Body, Region, body};
use std::collections::{BTreeMap, HashMap};

const API: &str = "cloudasset.assets.listResource (load balancers)";

pub(crate) struct LoadBalancers;

impl Collect for LoadBalancers {
    type Context = Batch;
    const API: &'static str = API;

    async fn collect(
        &self,
        ctx: &Batch,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        Ok(assemble(ctx))
    }
}

/// `internet-facing` or `internal`, as AWS names schemes.
fn scheme(load_balancing_scheme: Option<&str>) -> Option<String> {
    let s = load_balancing_scheme?;
    Some(
        if s.starts_with("INTERNAL") { "internal" } else { "internet-facing" }
            .into(),
    )
}

/// A resource named by a reference, placed in the project and region its
/// name gives.
fn located(id: &Arn, body: Body) -> Resource {
    let s = id.as_str();
    let place = s
        .split_once("/zones/")
        .or_else(|| s.split_once("/regions/"))
        .and_then(|(_, rest)| rest.split('/').next())
        .map_or_else(|| "global".to_string(), region);
    Resource {
        arn: id.clone(),
        account_id: AccountId::from(project(s).unwrap_or_default()),
        region: Region::from(place),
        tags: Default::default(),
        body,
    }
}

pub(crate) fn assemble(ctx: &Batch) -> Collected {
    let inv = &ctx.inventory;
    let mut out = Collected::default();

    // Instance group (as a backend names it) to the instances in it.
    let group_of_manager: HashMap<Arn, Arn> = inv
        .of(GROUP_MANAGER)
        .iter()
        .filter_map(|m| {
            let g = str_at(&m.resource.data, "/instanceGroup")?;
            Some((rid(&m.name), rid(&g)))
        })
        .collect();
    let mut members: HashMap<Arn, Vec<Arn>> = HashMap::new();
    for vm in inv.of(INSTANCE) {
        let group =
            group_manager(ctx, vm).and_then(|m| group_of_manager.get(&m));
        if let Some(group) = group {
            members.entry(group.clone()).or_default().push(rid(&vm.name));
        }
    }

    // A group can sit behind more than one backend service.
    let mut groups: BTreeMap<Arn, (Vec<String>, Option<String>)> =
        BTreeMap::new();
    for service in inv.of_types(&[BACKEND_SERVICE, REGION_BACKEND_SERVICE]) {
        let d = &service.resource.data;
        let arn = rid(&service.name);
        out.resources.push(ctx.resource(
            service,
            Body::LoadBalancer(body::LoadBalancer {
                name: Some(name(&service.name).to_string()),
                lb_type: Some("backend_service".into()),
                scheme: scheme(str_at(d, "/loadBalancingScheme").as_deref()),
                network_id: str_at(d, "/network").map(|n| rid(&n).to_string()),
                dns_name: None,
                security_groups: Vec::new(),
            }),
        ));
        for backend in items_at(d, "/backends") {
            let Some(group) = str_at(backend, "/group").map(|g| rid(&g)) else {
                continue;
            };
            out.relate(group.clone(), Edge::AttachedTo, arn.clone());
            let entry = groups.entry(group).or_default();
            entry.0.push(arn.to_string());
            entry.1 = entry.1.take().or_else(|| str_at(d, "/protocol"));
        }
    }
    for (group, (balancers, protocol)) in groups {
        let instances = members.get(&group).map_or(&[][..], Vec::as_slice);
        for i in instances {
            out.relate(i.clone(), Edge::TargetOf, group.clone());
        }
        let target_type = if group.as_str().contains("/networkEndpointGroups/")
        {
            "network_endpoint_group"
        } else {
            "instance_group"
        };
        out.resources.push(located(
            &group,
            Body::TargetGroup(body::TargetGroup {
                name: Some(name(group.as_str()).to_string()),
                target_type: Some(target_type.into()),
                protocol,
                load_balancers: balancers,
                instance_targets: instances.len(),
                ..Default::default()
            }),
        ));
    }

    for pool in inv.of(TARGET_POOL) {
        target_pool(ctx, pool, &mut out);
    }
    out
}

/// A target pool as a balancer with one target group of its instances.
fn target_pool(ctx: &Batch, pool: &Asset, out: &mut Collected) {
    let arn = rid(&pool.name);
    let group = Arn::from(format!("{arn}#instances"));
    let instances = strs_at(&pool.resource.data, "/instances");
    out.resources.push(ctx.resource(
        pool,
        Body::LoadBalancer(body::LoadBalancer {
            name: Some(name(&pool.name).to_string()),
            lb_type: Some("target_pool".into()),
            scheme: Some("internet-facing".into()),
            ..Default::default()
        }),
    ));
    out.relate(group.clone(), Edge::AttachedTo, arn.clone());
    for i in &instances {
        out.relate(rid(i), Edge::TargetOf, group.clone());
    }
    out.resources.push(ctx.child(
        pool,
        group.as_str(),
        Body::TargetGroup(body::TargetGroup {
            name: Some(name(&pool.name).to_string()),
            target_type: Some("instance".into()),
            load_balancers: vec![arn.to_string()],
            instance_targets: instances.len(),
            ..Default::default()
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::test::{asset, batch};
    use precip_types::scan::ResourceKind;

    const P: &str = "//compute.googleapis.com/projects/shop";
    const SELF: &str = "https://www.googleapis.com/compute/v1/projects/shop";

    fn assets() -> serde_json::Value {
        serde_json::json!([
            asset(
                INSTANCE,
                &format!("{P}/zones/us-east1-b/instances/web-1"),
                "us-east1-b",
                serde_json::json!({
                    "metadata": {"items": [{"key": "created-by", "value": "projects/111/zones/us-east1-b/instanceGroupManagers/web"}]},
                })
            ),
            asset(
                INSTANCE,
                &format!("{P}/zones/us-east1-b/instances/lone"),
                "us-east1-b",
                serde_json::json!({})
            ),
            asset(
                GROUP_MANAGER,
                &format!("{P}/zones/us-east1-b/instanceGroupManagers/web"),
                "us-east1-b",
                serde_json::json!({
                    "instanceGroup": format!("{SELF}/zones/us-east1-b/instanceGroups/web"),
                })
            ),
            asset(
                BACKEND_SERVICE,
                &format!("{P}/global/backendServices/web"),
                "global",
                serde_json::json!({
                    "loadBalancingScheme": "EXTERNAL_MANAGED", "protocol": "HTTP",
                    "backends": [
                        {"group": format!("{SELF}/zones/us-east1-b/instanceGroups/web")},
                        {"group": format!("{SELF}/regions/us-east1/networkEndpointGroups/run-neg")},
                    ],
                })
            ),
            asset(
                REGION_BACKEND_SERVICE,
                &format!("{P}/regions/us-east1/backendServices/web-int"),
                "us-east1",
                serde_json::json!({
                    "loadBalancingScheme": "INTERNAL", "network": format!("{SELF}/global/networks/prod"),
                    "backends": [{"group": format!("{SELF}/zones/us-east1-b/instanceGroups/web")}],
                })
            ),
            asset(
                TARGET_POOL,
                &format!("{P}/regions/us-east1/targetPools/legacy"),
                "us-east1",
                serde_json::json!({
                    "instances": [format!("{SELF}/zones/us-east1-b/instances/lone")],
                })
            ),
        ])
    }

    fn body_of<'a>(out: &'a Collected, arn: &str) -> &'a Body {
        &out.resources
            .iter()
            .find(|r| r.arn == arn)
            .unwrap_or_else(|| panic!("no {arn}"))
            .body
    }

    #[test]
    fn backend_services_are_balancers_over_their_groups_instances() {
        let out = assemble(&batch(assets()));
        let Body::LoadBalancer(lb) =
            body_of(&out, &format!("{P}/global/backendServices/web"))
        else {
            panic!()
        };
        assert_eq!(lb.scheme.as_deref(), Some("internet-facing"));
        let Body::LoadBalancer(internal) = body_of(
            &out,
            &format!("{P}/regions/us-east1/backendServices/web-int"),
        ) else {
            panic!()
        };
        assert_eq!(internal.scheme.as_deref(), Some("internal"));
        assert_eq!(
            internal.network_id.as_deref(),
            Some(format!("{P}/global/networks/prod").as_str())
        );

        let group = format!("{P}/zones/us-east1-b/instanceGroups/web");
        let Body::TargetGroup(tg) = body_of(&out, &group) else { panic!() };
        assert_eq!(tg.load_balancers.len(), 2, "one group behind two services");
        assert_eq!(tg.instance_targets, 1);
        assert_eq!(tg.protocol.as_deref(), Some("HTTP"));
        let r = out.resources.iter().find(|r| r.arn == group.as_str()).unwrap();
        assert_eq!(
            (r.account_id.as_str(), r.region.as_str()),
            ("shop", "us-east1")
        );
        assert!(out.relationships.iter().any(|r| r.edge == Edge::TargetOf
            && r.from_arn
                == format!("{P}/zones/us-east1-b/instances/web-1").as_str()
            && r.to_arn == group.as_str()));
        let Body::TargetGroup(neg) = body_of(
            &out,
            &format!("{P}/regions/us-east1/networkEndpointGroups/run-neg"),
        ) else {
            panic!()
        };
        assert_eq!(neg.target_type.as_deref(), Some("network_endpoint_group"));
        assert_eq!(neg.instance_targets, 0);
    }

    #[test]
    fn target_pools_name_their_instances() {
        let out = assemble(&batch(assets()));
        let pool = format!("{P}/regions/us-east1/targetPools/legacy");
        let Body::TargetGroup(tg) = body_of(&out, &format!("{pool}#instances"))
        else {
            panic!()
        };
        assert_eq!(tg.instance_targets, 1);
        assert!(out.relationships.iter().any(|r| r.edge == Edge::TargetOf
            && r.from_arn
                == format!("{P}/zones/us-east1-b/instances/lone").as_str()));
        assert_eq!(out.of_kind(ResourceKind::LoadBalancer).count(), 3);
    }

    #[test]
    fn schemes_follow_aws_names() {
        assert_eq!(
            scheme(Some("INTERNAL_MANAGED")).as_deref(),
            Some("internal")
        );
        assert_eq!(
            scheme(Some("EXTERNAL")).as_deref(),
            Some("internet-facing")
        );
        assert_eq!(scheme(None), None);
    }
}
