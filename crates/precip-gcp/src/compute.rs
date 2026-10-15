//! VM instances with their disks, images, managed instance groups and
//! static external IP addresses.

use crate::assets::{
    ADDRESS, Asset, DISK, GLOBAL_ADDRESS, GROUP_MANAGER, IMAGE, INSTANCE,
    REGION_DISK, TEMPLATE,
};
use crate::collect::Batch;
use crate::id::{architecture, i32_at, items_at, name, rid, str_at, strs_at};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};
use serde_json::Value;
use std::collections::HashMap;

const API: &str = "cloudasset.assets.listResource (compute)";

pub(crate) struct Compute;

impl Collect for Compute {
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

/// The managed instance group that created an instance, from the
/// `created-by` metadata the group sets.
pub(crate) fn group_manager(ctx: &Batch, instance: &Asset) -> Option<Arn> {
    items_at(&instance.resource.data, "/metadata/items")
        .iter()
        .find(|i| i.get("key").and_then(Value::as_str) == Some("created-by"))
        .and_then(|i| str_at(i, "/value"))
        .filter(|v| v.contains("/instanceGroupManagers/"))
        .map(|v| ctx.by_id(&v))
}

/// Whether licenses or guest OS features name Windows. Every public
/// Windows image carries a `windows-cloud` license.
fn is_windows(v: &Value) -> bool {
    strs_at(v, "/licenses").iter().any(|l| l.contains("/windows-cloud/"))
        || has_feature(v, "WINDOWS")
}

fn has_feature(v: &Value, feature: &str) -> bool {
    items_at(v, "/guestOsFeatures")
        .iter()
        .any(|f| f.get("type").and_then(Value::as_str) == Some(feature))
}

/// `Windows` or `Linux` when licenses or features say, else unknown.
fn platform(v: &Value) -> Option<String> {
    if is_windows(v) {
        Some("Windows".into())
    } else if !strs_at(v, "/licenses").is_empty() {
        Some("Linux".into())
    } else {
        None
    }
}

fn boot_mode(v: &Value) -> Option<String> {
    has_feature(v, "UEFI_COMPATIBLE").then(|| "uefi".to_string())
}

pub(crate) fn assemble(ctx: &Batch) -> Collected {
    let inv = &ctx.inventory;
    let mut out = Collected::default();

    let disks: HashMap<Arn, &Asset> =
        inv.of_types(&[DISK, REGION_DISK]).map(|d| (rid(&d.name), d)).collect();
    let templates: HashMap<Arn, String> = inv
        .of(TEMPLATE)
        .iter()
        .filter_map(|t| {
            let ty = str_at(&t.resource.data, "/properties/machineType")?;
            Some((rid(&t.name), name(&ty).to_string()))
        })
        .collect();

    for vm in inv.of(INSTANCE) {
        let d = &vm.resource.data;
        let arn = rid(&vm.name);
        let nics = items_at(d, "/networkInterfaces");
        let subnets: Vec<String> = nics
            .iter()
            .filter_map(|n| str_at(n, "/subnetwork"))
            .map(|s| rid(&s).to_string())
            .collect();
        let first = nics.first();
        let public_ip = nics.iter().find_map(|n| {
            items_at(n, "/accessConfigs")
                .iter()
                .find_map(|c| str_at(c, "/natIP"))
        });
        let attached = items_at(d, "/disks");
        let scratch = attached
            .iter()
            .filter(|a| str_at(a, "/type").as_deref() == Some("SCRATCH"))
            .count();
        let boot = attached
            .iter()
            .find(|a| a.get("boot").and_then(Value::as_bool) == Some(true));
        let boot_disk = boot
            .and_then(|b| str_at(b, "/source"))
            .and_then(|s| disks.get(&rid(&s)).copied());
        if let Some(group) = group_manager(ctx, vm) {
            out.relate(arn.clone(), Edge::MemberOfScaleGroup, group);
        }
        out.resources.push(
            ctx.resource(
                vm,
                Body::Instance(body::Instance {
                    name: Some(name(&vm.name).to_string()),
                    instance_type: str_at(d, "/machineType")
                        .map(|t| name(&t).to_string()),
                    architecture: boot
                        .and_then(|b| str_at(b, "/architecture"))
                        .and_then(|a| architecture(&a)),
                    platform: boot.and_then(platform),
                    state: str_at(d, "/status").map(|s| s.to_lowercase()),
                    image_id: boot_disk
                        .and_then(|b| str_at(&b.resource.data, "/sourceImage"))
                        .map(|i| rid(&i).to_string()),
                    subnets,
                    network_id: first
                        .and_then(|n| str_at(n, "/network"))
                        .map(|s| rid(&s).to_string()),
                    boot_disk: boot
                        .and_then(|b| str_at(b, "/source"))
                        .map(|s| rid(&s).to_string()),
                    boot_mode: boot.and_then(boot_mode),
                    launch_time: str_at(d, "/creationTimestamp"),
                    volume_count: attached.len() - scratch,
                    local_disk_count: scratch,
                    network_interface_count: nics.len(),
                    public_ip,
                }),
            ),
        );
    }

    for d in inv.of_types(&[DISK, REGION_DISK]) {
        let data = &d.resource.data;
        let arn = rid(&d.name);
        let users: Vec<String> = strs_at(data, "/users")
            .iter()
            .map(|u| rid(u).to_string())
            .collect();
        for u in &users {
            out.relate(arn.clone(), Edge::AttachedTo, Arn::from(u.as_str()));
        }
        out.resources.push(ctx.resource(
            d,
            Body::Volume(body::Volume {
                name: Some(name(&d.name).to_string()),
                size_gib: i32_at(data, "/sizeGb"),
                volume_type:
                    str_at(data, "/type").map(|t| name(&t).to_string()),
                iops: i32_at(data, "/provisionedIops"),
                throughput: i32_at(data, "/provisionedThroughput"),
                // Persistent disks are always encrypted at rest; a KMS key
                // means a customer-managed one.
                encrypted: Some(true),
                encryption_key_id: str_at(
                    data,
                    "/diskEncryptionKey/kmsKeyName",
                ),
                state: str_at(data, "/status").map(|s| s.to_lowercase()),
                attached_to: users,
            }),
        ));
    }

    for image in inv.of(IMAGE) {
        let data = &image.resource.data;
        out.resources.push(
            ctx.resource(
                image,
                Body::Image(body::Image {
                    name: Some(name(&image.name).to_string()),
                    owner_id: Some(ctx.account(image).to_string()),
                    platform: platform(data),
                    architecture: str_at(data, "/architecture")
                        .and_then(|a| architecture(&a)),
                    boot_mode: boot_mode(data),
                    state: str_at(data, "/status").map(|s| s.to_lowercase()),
                    ..Default::default()
                }),
            ),
        );
    }

    for group in inv.of(GROUP_MANAGER) {
        let data = &group.resource.data;
        let arn = rid(&group.name);
        let observed = out
            .relationships
            .iter()
            .filter(|r| r.edge == Edge::MemberOfScaleGroup && r.to_arn == arn)
            .count();
        let mut template_ids: Vec<String> = items_at(data, "/versions")
            .iter()
            .filter_map(|v| str_at(v, "/instanceTemplate"))
            .chain(str_at(data, "/instanceTemplate"))
            .map(|t| rid(&t).to_string())
            .collect();
        template_ids.sort();
        template_ids.dedup();
        let mut instance_types: Vec<String> = template_ids
            .iter()
            .filter_map(|t| templates.get(&Arn::from(t.as_str())).cloned())
            .collect();
        instance_types.sort();
        instance_types.dedup();
        out.resources.push(ctx.resource(
            group,
            Body::ScaleGroup(body::ScaleGroup {
                name: Some(name(&group.name).to_string()),
                desired_capacity: i32_at(data, "/targetSize"),
                observed_instances: observed,
                launch_template_id: template_ids.into_iter().next(),
                instance_types,
                ..Default::default()
            }),
        ));
    }

    for address in inv.of_types(&[ADDRESS, GLOBAL_ADDRESS]) {
        let data = &address.resource.data;
        // Internal addresses are reserved private IPs, not public ones.
        if str_at(data, "/addressType").as_deref() == Some("INTERNAL") {
            continue;
        }
        let arn = rid(&address.name);
        let instance = strs_at(data, "/users")
            .iter()
            .map(|u| rid(u))
            .find(|u| u.as_str().contains("/instances/"));
        if let Some(vm) = &instance {
            out.relate(arn.clone(), Edge::AttachedTo, vm.clone());
        }
        out.resources.push(ctx.resource(
            address,
            Body::PublicIp(body::PublicIp {
                name: Some(name(&address.name).to_string()),
                public_ip: str_at(data, "/address"),
                instance: instance.as_ref().map(ToString::to_string),
                network_interface_id: None,
                domain: None,
            }),
        ));
    }
    out
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
                    "machineType": format!("{SELF}/zones/us-east1-b/machineTypes/n2-standard-4"),
                    "status": "RUNNING",
                    "creationTimestamp": "2026-01-02T03:04:05.000-07:00",
                    "labels": {"app": "shop"},
                    "disks": [
                        {"boot": true, "type": "PERSISTENT", "source": format!("{SELF}/zones/us-east1-b/disks/web-1"),
                         "architecture": "X86_64",
                         "licenses": ["https://www.googleapis.com/compute/v1/projects/windows-cloud/global/licenses/windows-server-2022-dc"],
                         "guestOsFeatures": [{"type": "UEFI_COMPATIBLE"}]},
                        {"type": "PERSISTENT", "source": format!("{SELF}/zones/us-east1-b/disks/data")},
                        {"type": "SCRATCH"},
                    ],
                    "networkInterfaces": [{
                        "network": format!("{SELF}/global/networks/prod"),
                        "subnetwork": format!("{SELF}/regions/us-east1/subnetworks/web"),
                        "accessConfigs": [{"natIP": "34.1.2.3"}],
                    }],
                    "metadata": {"items": [{"key": "created-by", "value": "projects/111/zones/us-east1-b/instanceGroupManagers/web"}]},
                })
            ),
            asset(
                DISK,
                &format!("{P}/zones/us-east1-b/disks/web-1"),
                "us-east1-b",
                serde_json::json!({
                    "sizeGb": "100", "type": format!("{SELF}/zones/us-east1-b/diskTypes/pd-balanced"),
                    "users": [format!("{SELF}/zones/us-east1-b/instances/web-1")],
                    "sourceImage": "https://www.googleapis.com/compute/v1/projects/windows-cloud/global/images/windows-server-2022",
                    "status": "READY",
                })
            ),
            asset(
                REGION_DISK,
                &format!("{P}/regions/us-east1/disks/spare"),
                "us-east1",
                serde_json::json!({
                    "sizeGb": "10", "diskEncryptionKey": {"kmsKeyName": "projects/k/locations/l/keyRings/r/cryptoKeys/c"},
                })
            ),
            asset(
                TEMPLATE,
                &format!("{P}/global/instanceTemplates/web-v2"),
                "global",
                serde_json::json!({
                    "properties": {"machineType": "n2-standard-4"},
                })
            ),
            asset(
                GROUP_MANAGER,
                &format!("{P}/zones/us-east1-b/instanceGroupManagers/web"),
                "us-east1-b",
                serde_json::json!({
                    "targetSize": 3,
                    "instanceTemplate": format!("{SELF}/global/instanceTemplates/web-v2"),
                    "versions": [{"instanceTemplate": format!("{SELF}/global/instanceTemplates/web-v2")}],
                })
            ),
            asset(
                IMAGE,
                &format!("{P}/global/images/golden"),
                "global",
                serde_json::json!({
                    "architecture": "ARM64", "status": "READY",
                    "licenses": ["https://www.googleapis.com/compute/v1/projects/debian-cloud/global/licenses/debian-12"],
                })
            ),
            asset(
                ADDRESS,
                &format!("{P}/regions/us-east1/addresses/web-ip"),
                "us-east1",
                serde_json::json!({
                    "address": "34.9.9.9", "addressType": "EXTERNAL",
                    "users": [format!("{SELF}/zones/us-east1-b/instances/web-1")],
                })
            ),
            asset(
                ADDRESS,
                &format!("{P}/regions/us-east1/addresses/private"),
                "us-east1",
                serde_json::json!({
                    "address": "10.0.0.5", "addressType": "INTERNAL",
                })
            ),
        ])
    }

    fn instance(out: &Collected) -> &body::Instance {
        let r = out.of_kind(ResourceKind::Instance).next().unwrap();
        let Body::Instance(i) = &r.body else { panic!() };
        i
    }

    #[test]
    fn an_instance_takes_its_type_disks_network_and_os_from_its_config() {
        let out = assemble(&batch(assets()));
        let r = out.of_kind(ResourceKind::Instance).next().unwrap();
        assert_eq!(r.account_id, "shop");
        assert_eq!(r.region, "us-east1");
        assert_eq!(r.tags["app"], "shop");
        let i = instance(&out);
        assert_eq!(i.name.as_deref(), Some("web-1"));
        assert_eq!(i.instance_type.as_deref(), Some("n2-standard-4"));
        assert_eq!(i.state.as_deref(), Some("running"));
        assert_eq!(i.platform.as_deref(), Some("Windows"));
        assert_eq!(i.architecture.as_deref(), Some("x86_64"));
        assert_eq!(i.boot_mode.as_deref(), Some("uefi"));
        assert_eq!(
            i.image_id.as_deref(),
            Some(
                "//compute.googleapis.com/projects/windows-cloud/global/images/windows-server-2022"
            )
        );
        assert_eq!((i.volume_count, i.local_disk_count), (2, 1));
        assert_eq!(i.public_ip.as_deref(), Some("34.1.2.3"));
        assert_eq!(
            i.network_id.as_deref(),
            Some(format!("{P}/global/networks/prod").as_str())
        );
        assert_eq!(
            i.subnets,
            [format!("{P}/regions/us-east1/subnetworks/web")]
        );
        assert_eq!(
            i.boot_disk.as_deref(),
            Some(format!("{P}/zones/us-east1-b/disks/web-1").as_str())
        );
    }

    #[test]
    fn group_members_are_counted_and_take_the_templates_machine_type() {
        let out = assemble(&batch(assets()));
        let group = format!("{P}/zones/us-east1-b/instanceGroupManagers/web");
        assert!(
            out.relationships
                .iter()
                .any(|r| r.edge == Edge::MemberOfScaleGroup
                    && r.to_arn == group.as_str())
        );
        let g = out.of_kind(ResourceKind::ScaleGroup).next().unwrap();
        let Body::ScaleGroup(g) = &g.body else { panic!() };
        assert_eq!((g.desired_capacity, g.observed_instances), (Some(3), 1));
        assert_eq!(g.instance_types, ["n2-standard-4"]);
        assert_eq!(
            g.launch_template_id.as_deref(),
            Some(format!("{P}/global/instanceTemplates/web-v2").as_str())
        );
    }

    #[test]
    fn disks_attach_to_their_users_and_keep_size_type_and_key() {
        let out = assemble(&batch(assets()));
        let vm = format!("{P}/zones/us-east1-b/instances/web-1");
        let attached: Vec<&str> = out
            .relationships
            .iter()
            .filter(|r| r.edge == Edge::AttachedTo && r.to_arn == vm.as_str())
            .map(|r| r.from_arn.as_str())
            .collect();
        assert_eq!(attached.len(), 2, "the boot disk and the static IP");
        let vols: Vec<&body::Volume> = out
            .of_kind(ResourceKind::Volume)
            .filter_map(|r| {
                if let Body::Volume(v) = &r.body { Some(v) } else { None }
            })
            .collect();
        assert_eq!(vols[0].size_gib, Some(100));
        assert_eq!(vols[0].volume_type.as_deref(), Some("pd-balanced"));
        assert!(vols[1].attached_to.is_empty());
        assert!(vols[1].encryption_key_id.is_some());
    }

    #[test]
    fn external_addresses_are_public_ips_and_internal_ones_are_skipped() {
        let out = assemble(&batch(assets()));
        let ips: Vec<&body::PublicIp> = out
            .of_kind(ResourceKind::PublicIp)
            .filter_map(|r| {
                if let Body::PublicIp(p) = &r.body { Some(p) } else { None }
            })
            .collect();
        assert_eq!(ips.len(), 1);
        assert_eq!(ips[0].public_ip.as_deref(), Some("34.9.9.9"));
        assert_eq!(
            ips[0].instance.as_deref(),
            Some(format!("{P}/zones/us-east1-b/instances/web-1").as_str())
        );
    }

    #[test]
    fn images_carry_os_and_architecture_and_unlicensed_disks_say_nothing() {
        let out = assemble(&batch(assets()));
        let Body::Image(i) =
            &out.of_kind(ResourceKind::Image).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(i.platform.as_deref(), Some("Linux"));
        assert_eq!(i.architecture.as_deref(), Some("arm64"));
        assert_eq!(i.owner_id.as_deref(), Some("shop"));
        assert_eq!(platform(&serde_json::json!({})), None);
        assert_eq!(boot_mode(&serde_json::json!({})), None);
    }
}
