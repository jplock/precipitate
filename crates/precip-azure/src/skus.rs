//! vCPU and memory for every VM size in use, from the Resource SKUs API.
//! One call per region, made with a subscription that has VMs there; the
//! capabilities of a size do not differ between subscriptions.

use crate::collect::Batch;
use crate::id::architecture;
use precip_collect::{Collect, CollectError, Collected, Stage};
use precip_types::scan::Resource;
use precip_types::{AccountId, Arn, Body, Region, body};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const API: &str = "Microsoft.Compute/skus/read";
const API_VERSION: &str = "2021-07-01";

pub(crate) struct Skus;

impl Collect for Skus {
    type Context = Batch;
    const API: &'static str = API;
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &Batch,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut out = Collected::default();
        for (region, (subscription, sizes)) in sizes_in_use(prior) {
            let skus: Vec<Sku> = ctx
                .arm
                .list(&format!(
                    "/subscriptions/{subscription}/providers/Microsoft.Compute/skus?api-version={API_VERSION}&$filter=location eq '{region}'"
                ))
                .await
                .map_err(CollectError::new(API))?;
            out.resources.extend(instance_types(
                &skus,
                &region,
                &subscription,
                &sizes,
            ));
        }
        Ok(out)
    }
}

/// For each region, a subscription with VMs there and the sizes in use.
/// Scale sets count: a set scaled to zero still names its size.
fn sizes_in_use(
    prior: &Collected,
) -> BTreeMap<String, (String, BTreeSet<String>)> {
    let mut out: BTreeMap<String, (String, BTreeSet<String>)> = BTreeMap::new();
    for r in &prior.resources {
        let sizes: Vec<&str> = if let Body::Instance(i) = &r.body {
            i.instance_type.as_deref().into_iter().collect()
        } else if let Body::ScaleGroup(g) = &r.body {
            g.instance_types.iter().map(String::as_str).collect()
        } else {
            continue;
        };
        for size in sizes {
            out.entry(r.region.to_string())
                .or_insert_with(|| (r.account_id.to_string(), BTreeSet::new()))
                .1
                .insert(size.to_string());
        }
    }
    out
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Sku {
    pub resource_type: String,
    pub name: String,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Capability {
    pub name: String,
    pub value: String,
}

impl Sku {
    fn capability(&self, name: &str) -> Option<&str> {
        self.capabilities
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.value.as_str())
    }
}

/// The sizes in `wanted` as instance types, spelled as the VMs spell them
/// so sizing finds them. The id leaves out the subscription so every
/// subscription's lookup of a size is one resource.
fn instance_types(
    skus: &[Sku],
    region: &str,
    subscription: &str,
    wanted: &BTreeSet<String>,
) -> Vec<Resource> {
    let by_name: BTreeMap<String, &Sku> = skus
        .iter()
        .filter(|s| s.resource_type.eq_ignore_ascii_case("virtualMachines"))
        .map(|s| (s.name.to_lowercase(), s))
        .collect();
    let mut out = Vec::new();
    for size in wanted {
        let Some(sku) = by_name.get(&size.to_lowercase()) else {
            tracing::warn!(
                size,
                region,
                "VM size not in the region's SKU list"
            );
            continue;
        };
        let number =
            |c: &str| sku.capability(c).and_then(|v| v.parse::<f64>().ok());
        let whole = |c: &str| number(c).map(|n| n.round() as i64);
        out.push(Resource {
            arn: Arn::from(format!(
                "/providers/microsoft.compute/locations/{region}/vmsizes/{}",
                size.to_lowercase()
            )),
            account_id: AccountId::from(subscription),
            region: Region::from(region),
            tags: BTreeMap::new(),
            body: Body::InstanceType(body::InstanceType {
                name: size.clone(),
                vcpus: whole("vCPUs").and_then(|v| i32::try_from(v).ok()),
                memory_mib: number("MemoryGB")
                    .map(|gb| (gb * 1024.0).round() as i64),
                architectures: sku
                    .capability("CpuArchitectureType")
                    .and_then(architecture)
                    .map(|a| vec![a]),
                bare_metal: Some(false),
                gpus: whole("GPUs")
                    .and_then(|g| usize::try_from(g).ok())
                    .unwrap_or(0),
                accelerators: 0,
                local_storage_gb: whole("MaxResourceVolumeMB")
                    .map(|mb| mb / 1024),
                network_performance: None,
            }),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sku(name: &str, caps: &[(&str, &str)]) -> Sku {
        Sku {
            resource_type: "virtualMachines".into(),
            name: name.into(),
            capabilities: caps
                .iter()
                .map(|(n, v)| Capability {
                    name: (*n).into(),
                    value: (*v).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn sizes_in_use_resolve_to_vcpu_memory_and_architecture() {
        let skus = [
            sku(
                "Standard_D4ps_v5",
                &[
                    ("vCPUs", "4"),
                    ("MemoryGB", "16"),
                    ("CpuArchitectureType", "Arm64"),
                    ("MaxResourceVolumeMB", "0"),
                ],
            ),
            sku(
                "Standard_NC6",
                &[
                    ("vCPUs", "6"),
                    ("MemoryGB", "56"),
                    ("GPUs", "1"),
                    ("CpuArchitectureType", "x64"),
                ],
            ),
            Sku {
                resource_type: "disks".into(),
                name: "Premium_LRS".into(),
                capabilities: vec![],
            },
        ];
        let wanted = BTreeSet::from([
            "standard_d4ps_v5".to_string(),
            "Standard_NC6".into(),
            "Gone".into(),
        ]);
        let got = instance_types(&skus, "eastus", "s1", &wanted);
        assert_eq!(got.len(), 2, "unknown sizes are skipped");
        let Body::InstanceType(arm) = &got
            .iter()
            .find(|r| r.arn.as_str().ends_with("standard_d4ps_v5"))
            .unwrap()
            .body
        else {
            panic!()
        };
        assert_eq!(arm.name, "standard_d4ps_v5", "spelled as the VM spells it");
        assert_eq!((arm.vcpus, arm.memory_mib), (Some(4), Some(16384)));
        assert_eq!(
            arm.architectures.as_deref(),
            Some(&["arm64".to_string()][..])
        );
        let Body::InstanceType(gpu) = &got
            .iter()
            .find(|r| r.arn.as_str().ends_with("standard_nc6"))
            .unwrap()
            .body
        else {
            panic!()
        };
        assert_eq!(gpu.gpus, 1);
    }

    #[test]
    fn fractional_memory_rounds_to_whole_mib() {
        let got = instance_types(
            &[sku("Standard_B1ls", &[("vCPUs", "1"), ("MemoryGB", "0.5")])],
            "eastus",
            "s1",
            &BTreeSet::from(["Standard_B1ls".to_string()]),
        );
        let Body::InstanceType(t) = &got[0].body else { panic!() };
        assert_eq!(t.memory_mib, Some(512));
    }

    #[test]
    fn each_region_is_looked_up_once_with_scale_set_sizes_included() {
        let r = |region: &str, sub: &str, body: Body| Resource {
            arn: Arn::from(format!("{region}{sub}")),
            account_id: AccountId::from(sub),
            region: Region::from(region),
            tags: BTreeMap::new(),
            body,
        };
        let vm = |t: &str| {
            Body::Instance(body::Instance {
                instance_type: Some(t.into()),
                ..Default::default()
            })
        };
        let prior = Collected {
            resources: vec![
                r("eastus", "s1", vm("A")),
                r("eastus", "s2", vm("B")),
                r(
                    "westus",
                    "s2",
                    Body::ScaleGroup(body::ScaleGroup {
                        instance_types: vec!["C".into()],
                        ..Default::default()
                    }),
                ),
            ],
            ..Default::default()
        };
        let got = sizes_in_use(&prior);
        assert_eq!(
            got["eastus"],
            ("s1".to_string(), BTreeSet::from(["A".to_string(), "B".into()]))
        );
        assert_eq!(got["westus"].1, BTreeSet::from(["C".to_string()]));
    }
}
