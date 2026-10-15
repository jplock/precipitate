//! vCPU and memory for every machine type in use. Predefined types come
//! from Compute Engine's aggregated machine type list, filtered to the
//! names in use, through a project that has instances; a type's shape is
//! the same in every project and zone. Custom types spell their shape in
//! their name.

use crate::collect::Batch;
use crate::id::{architecture, int_at, items_at, region, str_at};
use precip_collect::{Collect, CollectError, Collected, Stage};
use precip_types::scan::Resource;
use precip_types::{AccountId, Arn, Body, Region, body};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const API: &str = "compute.machineTypes.list";
/// Names per filter, to keep the URL short.
const NAMES_PER_CALL: usize = 20;

pub(crate) struct MachineTypes;

impl Collect for MachineTypes {
    type Context = Batch;
    const API: &'static str = API;
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &Batch,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let wanted = in_use(prior);
        let mut out = Collected::default();
        let mut predefined = BTreeSet::new();
        for ((region, ty), project) in &wanted {
            match custom(ty) {
                Some(t) => {
                    out.resources.push(instance_type(region, project, t))
                }
                None => {
                    predefined.insert(ty.clone());
                }
            }
        }
        let Some(project) = wanted.values().next() else { return Ok(out) };
        let names: Vec<String> = predefined.into_iter().collect();
        let mut listed: BTreeMap<(String, String), body::InstanceType> =
            BTreeMap::new();
        for chunk in names.chunks(NAMES_PER_CALL) {
            let filter: Vec<String> =
                chunk.iter().map(|n| format!("(name = \"{n}\")")).collect();
            let mut url = ctx
                .gcp
                .url(
                    "compute",
                    &format!(
                        "/compute/v1/projects/{project}/aggregated/machineTypes"
                    ),
                )
                .map_err(CollectError::new(API))?;
            url.query_pairs_mut()
                .append_pair("filter", &filter.join(" OR "))
                .append_pair("returnPartialSuccess", "true");
            let pages: Vec<Aggregated> =
                ctx.gcp.pages(url).await.map_err(CollectError::new(API))?;
            for page in pages {
                for (scope, list) in page.items {
                    let Some(zone) = scope.strip_prefix("zones/") else {
                        continue;
                    };
                    for mt in list.machine_types {
                        // Every zone of a region offers the same shape;
                        // the first zone listed answers for the region.
                        if let Some(t) = from_api(&mt) {
                            listed
                                .entry((region(zone), t.name.clone()))
                                .or_insert(t);
                        }
                    }
                }
            }
        }
        for ((r, ty), project) in &wanted {
            if custom(ty).is_some() {
                continue;
            }
            match listed.get(&(r.clone(), ty.clone())) {
                Some(t) => {
                    out.resources.push(instance_type(r, project, t.clone()))
                }
                None => tracing::warn!(
                    machine_type = ty,
                    region = r,
                    "machine type not offered in the region"
                ),
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Aggregated {
    items: BTreeMap<String, ScopedList>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ScopedList {
    machine_types: Vec<Value>,
}

/// Each (region, machine type) in use, with a project that uses it.
/// Managed instance groups count: a group scaled to zero still names its
/// type.
fn in_use(prior: &Collected) -> BTreeMap<(String, String), String> {
    let mut out = BTreeMap::new();
    for r in &prior.resources {
        let types: Vec<&str> = if let Body::Instance(i) = &r.body {
            i.instance_type.as_deref().into_iter().collect()
        } else if let Body::ScaleGroup(g) = &r.body {
            g.instance_types.iter().map(String::as_str).collect()
        } else {
            continue;
        };
        for t in types {
            out.entry((r.region.to_string(), t.to_string()))
                .or_insert_with(|| r.account_id.to_string());
        }
    }
    out
}

/// A custom type's shape from its name: `custom-4-16384` and
/// `n2-custom-4-16384-ext` have 4 vCPUs and 16,384 MiB.
fn custom(name: &str) -> Option<body::InstanceType> {
    let parts: Vec<&str> = name.split('-').collect();
    let at = parts.iter().position(|p| *p == "custom")?;
    let vcpus: i32 = parts.get(at + 1)?.parse().ok()?;
    let memory: i64 = parts.get(at + 2)?.parse().ok()?;
    Some(body::InstanceType {
        name: name.to_string(),
        vcpus: Some(vcpus),
        memory_mib: Some(memory),
        // Only x86 machine series take custom shapes.
        architectures: Some(vec!["x86_64".into()]),
        bare_metal: Some(false),
        ..Default::default()
    })
}

fn from_api(mt: &Value) -> Option<body::InstanceType> {
    let name = str_at(mt, "/name")?;
    let gpus: i64 = items_at(mt, "/accelerators")
        .iter()
        .filter_map(|a| int_at(a, "/guestAcceleratorCount"))
        .sum();
    let local_ssds = int_at(mt, "/bundledLocalSsds/partitionCount");
    Some(body::InstanceType {
        vcpus: int_at(mt, "/guestCpus").and_then(|v| i32::try_from(v).ok()),
        memory_mib: int_at(mt, "/memoryMb"),
        architectures: str_at(mt, "/architecture")
            .and_then(|a| architecture(&a))
            .map(|a| vec![a]),
        bare_metal: Some(name.ends_with("-metal")),
        gpus: usize::try_from(gpus).unwrap_or(0),
        accelerators: 0,
        // Each bundled local SSD partition is 375 GB.
        local_storage_gb: local_ssds.map(|n| n * 375),
        network_performance: None,
        name,
    })
}

/// A machine type in a region. The id leaves out the project so every
/// project's lookup of a type is one resource.
fn instance_type(
    region: &str,
    project: &str,
    t: body::InstanceType,
) -> Resource {
    Resource {
        arn: Arn::from(format!(
            "//compute.googleapis.com/regions/{region}/machineTypes/{}",
            t.name
        )),
        account_id: AccountId::from(project),
        region: Region::from(region),
        tags: BTreeMap::new(),
        body: Body::InstanceType(t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::test::batch_with;
    use precip_collect::http::fake::{Script, ok, status};
    use precip_types::scan::ResourceKind;
    use std::sync::Arc;

    fn r(region: &str, project: &str, body: Body) -> Resource {
        Resource {
            arn: Arn::from(format!("{region}/{project}/{}", body.kind())),
            account_id: AccountId::from(project),
            region: Region::from(region),
            tags: BTreeMap::new(),
            body,
        }
    }

    fn vm(t: &str) -> Body {
        Body::Instance(body::Instance {
            instance_type: Some(t.into()),
            ..Default::default()
        })
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn types_in_use_are_looked_up_once_and_custom_ones_parsed() {
        let script = Arc::new(Script::queue(vec![ok(serde_json::json!({
            "items": {
                "zones/us-east1-b": {"machineTypes": [
                    {"name": "n2-standard-4", "guestCpus": 4, "memoryMb": 16384, "architecture": "X86_64"},
                    {"name": "a2-highgpu-1g", "guestCpus": 12, "memoryMb": 87040,
                     "accelerators": [{"guestAcceleratorType": "nvidia-tesla-a100", "guestAcceleratorCount": 1}]},
                ]},
                "zones/us-east1-c": {"machineTypes": [
                    {"name": "n2-standard-4", "guestCpus": 4, "memoryMb": 16384},
                ]},
                "zones/europe-west4-a": {"warning": {"code": "NO_RESULTS_ON_PAGE"}},
            },
        }))]));
        let ctx = batch_with(serde_json::json!([]), script.clone());
        let prior = Collected {
            resources: vec![
                r("us-east1", "shop", vm("n2-standard-4")),
                r("us-east1", "other", vm("n2-standard-4")),
                r("us-east1", "shop", vm("n2-custom-6-23040")),
                r(
                    "us-east1",
                    "shop",
                    Body::ScaleGroup(body::ScaleGroup {
                        instance_types: vec!["a2-highgpu-1g".into()],
                        ..Default::default()
                    }),
                ),
                r("asia-east1", "shop", vm("gone-type")),
            ],
            ..Default::default()
        };
        let out = block_on(MachineTypes.collect(&ctx, &prior)).unwrap();
        let sent = script.urls();
        assert_eq!(sent.len(), 1, "one filtered call");
        assert!(sent[0].contains(
            "/compute/v1/projects/shop/aggregated/machineTypes?filter="
        ));
        assert!(!sent[0].contains("custom"), "custom types are not looked up");

        let types: BTreeMap<String, &body::InstanceType> = out
            .of_kind(ResourceKind::InstanceType)
            .filter_map(|r| {
                if let Body::InstanceType(t) = &r.body {
                    Some((r.arn.to_string(), t))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(types.len(), 3, "the unknown type is skipped");
        let n2 = types["//compute.googleapis.com/regions/us-east1/machineTypes/n2-standard-4"];
        assert_eq!((n2.vcpus, n2.memory_mib), (Some(4), Some(16384)));
        assert_eq!(
            n2.architectures.as_deref(),
            Some(&["x86_64".to_string()][..])
        );
        let gpu = types["//compute.googleapis.com/regions/us-east1/machineTypes/a2-highgpu-1g"];
        assert_eq!(gpu.gpus, 1);
        let c = types["//compute.googleapis.com/regions/us-east1/machineTypes/n2-custom-6-23040"];
        assert_eq!((c.vcpus, c.memory_mib), (Some(6), Some(23040)));
    }

    #[test]
    fn nothing_in_use_makes_no_call_and_a_failed_call_fails_the_collector() {
        let script = Arc::new(Script::queue(vec![status(
            403,
            serde_json::json!({"error": {"status": "PERMISSION_DENIED", "message": "no"}}),
        )]));
        let ctx = batch_with(serde_json::json!([]), script.clone());
        let none = block_on(MachineTypes.collect(&ctx, &Collected::default()))
            .unwrap();
        assert!(none.resources.is_empty());
        assert!(script.urls().is_empty());
        let prior = Collected {
            resources: vec![r("us-east1", "shop", vm("e2-medium"))],
            ..Default::default()
        };
        let err = block_on(MachineTypes.collect(&ctx, &prior)).unwrap_err();
        assert_eq!(err.api, API);
    }

    #[test]
    fn custom_names_parse_and_others_do_not() {
        assert_eq!(custom("custom-2-4096").map(|t| t.vcpus), Some(Some(2)));
        assert_eq!(
            custom("n2-custom-4-16384-ext").map(|t| t.memory_mib),
            Some(Some(16384))
        );
        assert!(custom("e2-medium").is_none());
        assert!(custom("e2-custom-small-1024").is_none());
        let metal = from_api(&serde_json::json!({"name": "c3-highcpu-192-metal", "bundledLocalSsds": {"partitionCount": 2}})).unwrap();
        assert_eq!(metal.bare_metal, Some(true));
        assert_eq!(metal.local_storage_gb, Some(750));
        assert!(from_api(&serde_json::json!({})).is_none());
    }
}
