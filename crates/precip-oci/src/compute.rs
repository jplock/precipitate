//! Instances with their shapes, images, VNICs, security groups and
//! reserved public IPs, and boot and block volumes with their
//! attachments.
//!
//! An instance is in each network security group its VNICs name and each
//! security list its subnets name, as `has_security_group`.
//!
//! A flexible shape's size is set per instance, so every instance's shape
//! configuration becomes an instance type, named after the shape and, for
//! flexible shapes, the OCPUs and memory chosen.

use crate::collect::{Pair, is_gone, num_at, str_at, strings};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::scan::{Edge, Resource};
use precip_types::{AccountId, Arn, Body, Region, body};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};

const API: &str = "core (compute and block volumes)";

pub(crate) struct Compute;

impl Collect for Compute {
    type Context = Pair;
    const API: &'static str = API;

    async fn collect(
        &self,
        ctx: &Pair,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut f = Fetched {
            instances: ctx
                .list("/20160918/instances", &[])
                .await
                .map_err(CollectError::new(API))?,
            vnics: ctx
                .list("/20160918/vnicAttachments", &[])
                .await
                .map_err(CollectError::new(API))?,
            volume_attachments: ctx
                .list("/20160918/volumeAttachments", &[])
                .await
                .map_err(CollectError::new(API))?,
            volumes: ctx
                .list("/20160918/volumes", &[])
                .await
                .map_err(CollectError::new(API))?,
            boot_volumes: ctx
                .list("/20160918/bootVolumes", &[])
                .await
                .map_err(CollectError::new(API))?,
            ..Default::default()
        };
        // Boot volume attachments are listed one availability domain at a
        // time.
        for ad in &ctx.domains {
            let got: Vec<Value> = ctx
                .list(
                    "/20160918/bootVolumeAttachments",
                    &[("availabilityDomain", ad)],
                )
                .await
                .map_err(CollectError::new(API))?;
            f.boot_attachments.extend(got);
        }
        f.public_ips = ctx
            .list(
                "/20160918/publicIps",
                &[("scope", "REGION"), ("lifetime", "RESERVED")],
            )
            .await
            .map_err(CollectError::new(API))?;
        // A subnet, VNIC or image can be in a compartment the caller
        // cannot read; the instance then goes without what it holds.
        let live = f.vnics.iter().filter(|v| !is_gone(v));
        let vnics: BTreeSet<String> =
            live.clone().filter_map(|v| str_at(v, "/vnicId")).collect();
        let subnets: BTreeSet<String> =
            live.filter_map(|v| str_at(v, "/subnetId")).collect();
        for (kind, ids, into) in [
            ("subnets", subnets, &mut f.subnets),
            ("vnics", vnics, &mut f.vnic_details),
        ] {
            for id in ids {
                match ctx.get::<Value>(&format!("/20160918/{kind}/{id}")).await
                {
                    Ok(v) => {
                        into.insert(id, v);
                    }
                    Err(e) => tracing::warn!(
                        id,
                        error = %precip_collect::error_chain(&e),
                        "reading {kind}"
                    ),
                }
            }
        }
        let images: BTreeSet<String> = f
            .instances
            .iter()
            .filter(|i| !is_gone(i))
            .filter_map(|i| str_at(i, "/imageId"))
            .collect();
        for id in images {
            match ctx.get::<Value>(&format!("/20160918/images/{id}")).await {
                Ok(img) => {
                    f.images.insert(id, img);
                }
                Err(e) => {
                    tracing::warn!(image = id, error = %precip_collect::error_chain(&e), "reading image")
                }
            }
        }
        Ok(assemble(ctx, &f))
    }
}

#[derive(Debug, Default)]
pub(crate) struct Fetched {
    pub instances: Vec<Value>,
    pub vnics: Vec<Value>,
    pub volume_attachments: Vec<Value>,
    pub boot_attachments: Vec<Value>,
    pub volumes: Vec<Value>,
    pub boot_volumes: Vec<Value>,
    /// Reserved public IPs in the compartment.
    pub public_ips: Vec<Value>,
    /// Subnets and VNICs by id, as read one at a time.
    pub subnets: HashMap<String, Value>,
    pub vnic_details: HashMap<String, Value>,
    /// Image by id.
    pub images: HashMap<String, Value>,
}

/// `2` for 2.0 and `1.5` for 1.5, for names.
fn number(n: f64) -> String {
    format!("{n}")
}

fn architecture(shape: &str, cfg: &Value) -> Option<String> {
    let processor = str_at(cfg, "/processorDescription").unwrap_or_default();
    let ampere = processor.contains("Ampere")
        || [".A1.", ".A2.", ".A4."].iter().any(|s| shape.contains(s));
    if ampere {
        Some("arm64".into())
    } else if !processor.is_empty() {
        Some("x86_64".into())
    } else {
        None
    }
}

/// An instance's shape as an instance type, and its name.
fn shape(instance: &Value) -> Option<body::InstanceType> {
    let shape = str_at(instance, "/shape")?;
    let cfg = instance.get("shapeConfig").cloned().unwrap_or_default();
    let ocpus = num_at(&cfg, "/ocpus");
    let memory_gb = num_at(&cfg, "/memoryInGBs");
    let arch = architecture(&shape, &cfg);
    let name = match (shape.ends_with(".Flex"), ocpus, memory_gb) {
        (true, Some(o), Some(m)) => {
            format!("{shape}/{}ocpu-{}gb", number(o), number(m))
        }
        (true | false, _, _) => shape.clone(),
    };
    // An OCPU is one core: two vCPUs on x86, one on Ampere.
    let per_ocpu = if arch.as_deref() == Some("arm64") { 1.0 } else { 2.0 };
    let vcpus = num_at(&cfg, "/vcpus").or_else(|| ocpus.map(|o| o * per_ocpu));
    Some(body::InstanceType {
        name,
        vcpus: vcpus.map(|v| v.round() as i32),
        memory_mib: memory_gb.map(|g| (g * 1024.0).round() as i64),
        architectures: arch.map(|a| vec![a]),
        bare_metal: Some(shape.starts_with("BM.")),
        gpus: num_at(&cfg, "/gpus").map_or(0, |g| g as usize),
        accelerators: 0,
        local_storage_gb: num_at(&cfg, "/localDisksTotalSizeInGBs")
            .map(|g| g.round() as i64),
        network_performance: num_at(&cfg, "/networkingBandwidthInGbps")
            .map(|g| format!("{} Gbps", number(g))),
    })
}

fn boot_mode(firmware: Option<&str>) -> Option<String> {
    match firmware? {
        "BIOS" => Some("legacy-bios".into()),
        "UEFI_64" => Some("uefi".into()),
        _ => None,
    }
}

/// `Windows` for Windows images, else `Linux` when the image names an OS.
fn platform(image: &Value) -> Option<String> {
    let os = str_at(image, "/operatingSystem")?;
    Some(if os.starts_with("Windows") { "Windows" } else { "Linux" }.into())
}

pub(crate) fn assemble(ctx: &Pair, f: &Fetched) -> Collected {
    let mut out = Collected::default();
    let live = |rows: &'_ [Value]| -> Vec<Value> {
        rows.iter().filter(|r| !is_gone(r)).cloned().collect()
    };
    let vnics = live(&f.vnics);
    let attachments: Vec<(Value, &str)> = live(&f.volume_attachments)
        .into_iter()
        .map(|a| (a, "volumeId"))
        .chain(
            live(&f.boot_attachments).into_iter().map(|a| (a, "bootVolumeId")),
        )
        .collect();

    // Volume to the instances it is attached to.
    let mut attached: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut volumes_of: HashMap<String, usize> = HashMap::new();
    for (a, field) in &attachments {
        let (Some(volume), Some(instance)) =
            (str_at(a, &format!("/{field}")), str_at(a, "/instanceId"))
        else {
            continue;
        };
        *volumes_of.entry(instance.clone()).or_default() += 1;
        attached.entry(volume).or_default().push(instance);
    }

    let boot = live(&f.boot_attachments);
    // Public address to the instance holding it.
    let mut addresses: HashMap<String, String> = HashMap::new();
    let mut types = BTreeMap::new();
    for i in f.instances.iter().filter(|i| !is_gone(i)) {
        let Some(id) = str_at(i, "/id") else { continue };
        let arn = Arn::from(id.as_str());
        let mut own: Vec<&Value> = vnics
            .iter()
            .filter(|v| {
                str_at(v, "/instanceId").as_deref() == Some(id.as_str())
            })
            .collect();
        own.sort_by_key(|v| {
            v.get("nicIndex").and_then(Value::as_i64).unwrap_or(0)
        });
        let subnets: Vec<String> =
            own.iter().filter_map(|v| str_at(v, "/subnetId")).collect();
        let details: Vec<&Value> = own
            .iter()
            .filter_map(|v| f.vnic_details.get(&str_at(v, "/vnicId")?))
            .collect();
        let mut groups: BTreeSet<String> = BTreeSet::new();
        for d in &details {
            groups.extend(strings(d, "/nsgIds"));
        }
        for s in &subnets {
            if let Some(subnet) = f.subnets.get(s) {
                groups.extend(strings(subnet, "/securityListIds"));
            }
        }
        for g in groups {
            out.relate(arn.clone(), Edge::HasSecurityGroup, Arn::from(g));
        }
        let public_ip = details.iter().find_map(|d| str_at(d, "/publicIp"));
        if let Some(ip) = &public_ip {
            addresses.insert(ip.clone(), id.clone());
        }
        let network =
            subnets.first().and_then(|s| str_at(f.subnets.get(s)?, "/vcnId"));
        let boot_disk = boot
            .iter()
            .find(|a| str_at(a, "/instanceId").as_deref() == Some(id.as_str()))
            .and_then(|a| str_at(a, "/bootVolumeId"));
        let image = str_at(i, "/imageId").and_then(|img| f.images.get(&img));
        let ty = shape(i);
        let local =
            num_at(i, "/shapeConfig/localDisks").map_or(0, |n| n as usize);
        out.resources.push(ctx.resource(
            &id,
            i,
            Body::Instance(body::Instance {
                name: str_at(i, "/displayName"),
                instance_type: ty.as_ref().map(|t| t.name.clone()),
                architecture:
                    ty.as_ref().and_then(|t| {
                        t.architectures.as_ref()?.first().cloned()
                    }),
                platform: image.and_then(platform),
                state: str_at(i, "/lifecycleState").map(|s| s.to_lowercase()),
                image_id: str_at(i, "/imageId"),
                subnets,
                network_id: network,
                boot_disk,
                boot_mode: boot_mode(
                    str_at(i, "/launchOptions/firmware").as_deref(),
                ),
                launch_time: str_at(i, "/timeCreated"),
                volume_count: volumes_of.get(&id).copied().unwrap_or(0),
                local_disk_count: local,
                network_interface_count: own.len(),
                public_ip,
            }),
        ));
        if let Some(t) = ty {
            types.entry(t.name.clone()).or_insert(t);
        }
    }

    // The id leaves out the compartment so every compartment's instances
    // of one size share it.
    for (name, t) in types {
        out.resources.push(Resource {
            arn: Arn::from(format!("oci:{}:shape/{name}", ctx.region)),
            account_id: AccountId::from(ctx.compartment.as_str()),
            region: Region::from(ctx.region.as_str()),
            tags: BTreeMap::new(),
            body: Body::InstanceType(t),
        });
    }

    for ip in f.public_ips.iter().filter(|p| !is_gone(p)) {
        let Some(id) = str_at(ip, "/id") else { continue };
        let address = str_at(ip, "/ipAddress");
        let instance = address.as_ref().and_then(|a| addresses.get(a)).cloned();
        if let Some(i) = &instance {
            out.relate(
                Arn::from(id.as_str()),
                Edge::AttachedTo,
                Arn::from(i.as_str()),
            );
        }
        out.resources.push(ctx.resource(
            &id,
            ip,
            Body::PublicIp(body::PublicIp {
                name: str_at(ip, "/displayName"),
                public_ip: address,
                instance,
                network_interface_id: None,
                domain: None,
            }),
        ));
    }

    for (id, img) in &f.images {
        out.resources.push(ctx.resource(
            id,
            img,
            Body::Image(body::Image {
                name: str_at(img, "/displayName"),
                owner_id: str_at(img, "/compartmentId"),
                platform: platform(img),
                boot_mode: boot_mode(
                    str_at(img, "/launchOptions/firmware").as_deref(),
                ),
                state: str_at(img, "/lifecycleState").map(|s| s.to_lowercase()),
                public: Some(str_at(img, "/compartmentId").is_none()),
                ..Default::default()
            }),
        ));
    }

    for (v, kind) in f
        .volumes
        .iter()
        .map(|v| (v, "block"))
        .chain(f.boot_volumes.iter().map(|v| (v, "boot")))
    {
        if is_gone(v) {
            continue;
        }
        let Some(id) = str_at(v, "/id") else { continue };
        let users = attached.get(&id).cloned().unwrap_or_default();
        for u in &users {
            out.relate(
                Arn::from(id.as_str()),
                Edge::AttachedTo,
                Arn::from(u.as_str()),
            );
        }
        out.resources.push(ctx.resource(
            &id,
            v,
            Body::Volume(body::Volume {
                name: str_at(v, "/displayName"),
                size_gib: num_at(v, "/sizeInGBs").map(|g| g as i32),
                volume_type: Some(kind.into()),
                iops: None,
                throughput: None,
                // Block volumes are always encrypted at rest; a key means a
                // customer-managed one.
                encrypted: Some(true),
                encryption_key_id: str_at(v, "/kmsKeyId"),
                state: str_at(v, "/lifecycleState").map(|s| s.to_lowercase()),
                attached_to: users,
            }),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Oci;
    use precip_collect::http::fake::Script;
    use precip_types::scan::ResourceKind;
    use std::sync::Arc;

    fn pair() -> Pair {
        Pair {
            oci: Oci::fake(Arc::new(Script::default())),
            region: "us-ashburn-1".into(),
            compartment: "c.shop".into(),
            domains: vec!["Uocm:US-ASHBURN-AD-1".into()],
        }
    }

    fn fetched() -> Fetched {
        Fetched {
            instances: vec![
                serde_json::json!({"id": "i.web", "displayName": "web-1", "shape": "VM.Standard.E4.Flex",
                    "shapeConfig": {"ocpus": 2.0, "memoryInGBs": 16.0, "vcpus": 4,
                                    "processorDescription": "2.55 GHz AMD EPYC 7J13", "networkingBandwidthInGbps": 2.0},
                    "lifecycleState": "RUNNING", "imageId": "img.win", "timeCreated": "2026-01-02T03:04:05Z",
                    "launchOptions": {"firmware": "UEFI_64"}, "freeformTags": {"app": "shop"}}),
                serde_json::json!({"id": "i.arm", "displayName": "arm-1", "shape": "VM.Standard.A1.Flex",
                    "shapeConfig": {"ocpus": 4.0, "memoryInGBs": 24.0}, "lifecycleState": "STOPPED"}),
                serde_json::json!({"id": "i.dense", "shape": "BM.DenseIO.E4.128",
                    "shapeConfig": {"ocpus": 128.0, "memoryInGBs": 2048.0, "localDisks": 8, "localDisksTotalSizeInGBs": 54400.0,
                                    "processorDescription": "AMD"}, "lifecycleState": "RUNNING"}),
                serde_json::json!({"id": "i.gone", "shape": "VM.Standard2.1", "lifecycleState": "TERMINATED"}),
            ],
            vnics: vec![
                serde_json::json!({"instanceId": "i.web", "subnetId": "s.app", "vnicId": "vnic.2", "nicIndex": 1, "lifecycleState": "ATTACHED"}),
                serde_json::json!({"instanceId": "i.web", "subnetId": "s.web", "vnicId": "vnic.1", "nicIndex": 0, "lifecycleState": "ATTACHED"}),
                serde_json::json!({"instanceId": "i.web", "subnetId": "s.old", "lifecycleState": "DETACHED"}),
            ],
            volume_attachments: vec![
                serde_json::json!({"instanceId": "i.web", "volumeId": "v.data", "lifecycleState": "ATTACHED"}),
            ],
            boot_attachments: vec![
                serde_json::json!({"instanceId": "i.web", "bootVolumeId": "bv.web", "lifecycleState": "ATTACHED"}),
            ],
            volumes: vec![
                serde_json::json!({"id": "v.data", "displayName": "data", "sizeInGBs": 500, "kmsKeyId": "key.1", "lifecycleState": "AVAILABLE"}),
                serde_json::json!({"id": "v.spare", "displayName": "spare", "sizeInGBs": 50, "lifecycleState": "AVAILABLE"}),
                serde_json::json!({"id": "v.gone", "lifecycleState": "TERMINATED"}),
            ],
            boot_volumes: vec![
                serde_json::json!({"id": "bv.web", "displayName": "web-1 (Boot Volume)", "sizeInGBs": 50, "lifecycleState": "AVAILABLE"}),
            ],
            public_ips: vec![
                serde_json::json!({"id": "ip.web", "displayName": "web-ip", "ipAddress": "129.1.1.1", "lifecycleState": "ASSIGNED"}),
                serde_json::json!({"id": "ip.free", "ipAddress": "129.2.2.2", "lifecycleState": "AVAILABLE"}),
            ],
            subnets: HashMap::from([(
                "s.web".into(),
                serde_json::json!({"vcnId": "vcn.prod", "securityListIds": ["sl.web"]}),
            )]),
            vnic_details: HashMap::from([(
                "vnic.1".into(),
                serde_json::json!({"publicIp": "129.1.1.1", "nsgIds": ["nsg.web"]}),
            )]),
            images: HashMap::from([(
                "img.win".into(),
                serde_json::json!({"displayName": "Windows-Server-2022", "operatingSystem": "Windows", "lifecycleState": "AVAILABLE"}),
            )]),
        }
    }

    fn instance<'a>(out: &'a Collected, id: &str) -> &'a body::Instance {
        let r = out
            .resources
            .iter()
            .find(|r| r.arn == id)
            .unwrap_or_else(|| panic!("no {id}"));
        let Body::Instance(i) = &r.body else { panic!() };
        i
    }

    fn instance_type<'a>(
        out: &'a Collected,
        name: &str,
    ) -> &'a body::InstanceType {
        out.of_kind(ResourceKind::InstanceType)
            .find_map(|r| {
                if let Body::InstanceType(t) = &r.body {
                    (t.name == name).then_some(t)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("no type {name}"))
    }

    #[test]
    fn an_instance_takes_its_network_volumes_os_and_flexible_size() {
        let out = assemble(&pair(), &fetched());
        let r = out.resources.iter().find(|r| r.arn == "i.web").unwrap();
        assert_eq!(
            (r.account_id.as_str(), r.region.as_str()),
            ("c.shop", "us-ashburn-1")
        );
        assert_eq!(r.tags["app"], "shop");
        let i = instance(&out, "i.web");
        assert_eq!(i.name.as_deref(), Some("web-1"));
        assert_eq!(
            i.instance_type.as_deref(),
            Some("VM.Standard.E4.Flex/2ocpu-16gb")
        );
        assert_eq!(i.architecture.as_deref(), Some("x86_64"));
        assert_eq!(i.platform.as_deref(), Some("Windows"));
        assert_eq!(i.boot_mode.as_deref(), Some("uefi"));
        assert_eq!(
            i.subnets,
            ["s.web", "s.app"],
            "primary first, detached left out"
        );
        assert_eq!(i.boot_disk.as_deref(), Some("bv.web"));
        assert_eq!(i.public_ip.as_deref(), Some("129.1.1.1"));
        assert_eq!(i.network_id.as_deref(), Some("vcn.prod"));
        assert_eq!((i.volume_count, i.network_interface_count), (2, 2));
        let t = instance_type(&out, "VM.Standard.E4.Flex/2ocpu-16gb");
        assert_eq!((t.vcpus, t.memory_mib), (Some(4), Some(16384)));
        assert_eq!(t.network_performance.as_deref(), Some("2 Gbps"));
        assert!(out.resources.iter().all(|r| r.arn != "i.gone"));
    }

    #[test]
    fn ampere_ocpus_are_one_vcpu_and_bare_metal_local_disks_count() {
        let out = assemble(&pair(), &fetched());
        let arm = instance_type(&out, "VM.Standard.A1.Flex/4ocpu-24gb");
        assert_eq!((arm.vcpus, arm.memory_mib), (Some(4), Some(24576)));
        assert_eq!(
            arm.architectures.as_deref(),
            Some(&["arm64".to_string()][..])
        );
        assert_eq!(instance(&out, "i.arm").state.as_deref(), Some("stopped"));
        let dense = instance_type(&out, "BM.DenseIO.E4.128");
        assert_eq!((dense.vcpus, dense.bare_metal), (Some(256), Some(true)));
        assert_eq!(dense.local_storage_gb, Some(54400));
        assert_eq!(instance(&out, "i.dense").local_disk_count, 8);
        let ty = out.of_kind(ResourceKind::InstanceType).next().unwrap();
        assert!(ty.arn.as_str().starts_with("oci:us-ashburn-1:shape/"));
    }

    #[test]
    fn volumes_attach_to_their_instances_and_gone_ones_are_dropped() {
        let out = assemble(&pair(), &fetched());
        let mut edges: Vec<(&str, &str)> = out
            .relationships
            .iter()
            .filter(|r| r.edge == Edge::AttachedTo)
            .map(|r| (r.from_arn.as_str(), r.to_arn.as_str()))
            .collect();
        edges.sort_unstable();
        assert_eq!(
            edges,
            [("bv.web", "i.web"), ("ip.web", "i.web"), ("v.data", "i.web")]
        );
        let vols: Vec<&body::Volume> = out
            .of_kind(ResourceKind::Volume)
            .filter_map(|r| {
                if let Body::Volume(v) = &r.body { Some(v) } else { None }
            })
            .collect();
        assert_eq!(vols.len(), 3);
        assert_eq!(vols[0].encryption_key_id.as_deref(), Some("key.1"));
        assert_eq!(vols[2].volume_type.as_deref(), Some("boot"));
        let mut groups: Vec<&str> = out
            .relationships
            .iter()
            .filter(|r| r.edge == Edge::HasSecurityGroup)
            .map(|r| r.to_arn.as_str())
            .collect();
        groups.sort_unstable();
        assert_eq!(
            groups,
            ["nsg.web", "sl.web"],
            "the VNIC's group and the subnet's list"
        );
    }

    #[test]
    fn reserved_public_ips_attach_to_the_instance_holding_their_address() {
        let out = assemble(&pair(), &fetched());
        let ips: Vec<&body::PublicIp> = out
            .of_kind(ResourceKind::PublicIp)
            .filter_map(|r| {
                if let Body::PublicIp(p) = &r.body { Some(p) } else { None }
            })
            .collect();
        assert_eq!(ips.len(), 2);
        assert_eq!(ips[0].instance.as_deref(), Some("i.web"));
        assert_eq!(ips[1].instance, None);
        assert!(out.relationships.iter().any(|r| r.edge == Edge::AttachedTo
            && r.from_arn == "ip.web"
            && r.to_arn == "i.web"));
    }

    #[test]
    fn images_record_their_os_and_whether_oracle_publishes_them() {
        let out = assemble(&pair(), &fetched());
        let Body::Image(i) =
            &out.of_kind(ResourceKind::Image).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(i.platform.as_deref(), Some("Windows"));
        assert_eq!(i.public, Some(true));
        assert_eq!(
            platform(&serde_json::json!({"operatingSystem": "Oracle Linux"}))
                .as_deref(),
            Some("Linux")
        );
        assert_eq!(platform(&serde_json::json!({})), None);
        assert_eq!(boot_mode(Some("BIOS")).as_deref(), Some("legacy-bios"));
        assert_eq!(boot_mode(Some("OTHER")), None);
    }

    #[test]
    fn a_shape_without_configuration_keeps_its_name() {
        let t = shape(&serde_json::json!({"shape": "VM.Standard2.1"})).unwrap();
        assert_eq!(
            (t.name.as_str(), t.vcpus, t.architectures),
            ("VM.Standard2.1", None, None)
        );
        assert!(shape(&serde_json::json!({})).is_none());
    }
}
