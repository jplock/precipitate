//! Managed services with no Oxide equivalent, read from the same
//! inventory as everything else.

use crate::assets::{Asset, MANAGED};
use crate::collect::Batch;
use crate::id::{int_at, items_at, name, rid, str_at};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::body::{Managed as ManagedBody, Service};
use precip_types::{Body, scan::Resource};
use serde_json::Value;

const API: &str = "cloudasset.assets.listResource (managed services)";

pub(crate) struct Managed;

impl Collect for Managed {
    type Context = Batch;
    const API: &'static str = API;

    async fn collect(
        &self,
        ctx: &Batch,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        Ok(Collected {
            resources: ctx
                .inventory
                .of_types(MANAGED)
                .filter_map(|a| resource(ctx, a))
                .collect(),
            ..Default::default()
        })
    }
}

/// `POSTGRES_15` as `postgres` and `15`; `MYSQL_8_0` as `mysql` and `8.0`.
fn engine_and_version(v: &str) -> (Option<String>, Option<String>) {
    let lower = v.to_lowercase();
    match lower.split_once('_') {
        Some((engine, version)) => {
            (Some(engine.to_string()), Some(version.replace('_', ".")))
        }
        None if lower.is_empty() => (None, None),
        None => (Some(lower), None),
    }
}

fn gib_to_bytes(gib: Option<i64>) -> Option<i64> {
    gib.and_then(|g| g.checked_mul(1 << 30))
}

/// Memory as Cloud Functions writes it (`256M`, `1Gi`, `512Mi`) in MiB.
fn memory_mib(s: &str) -> Option<i32> {
    let s = s.trim();
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n: f64 = s[..digits].parse().ok()?;
    let mib = match &s[digits..] {
        "" => n / f64::from(1 << 20),
        "k" => n * 1000.0 / f64::from(1 << 20),
        "Ki" => n / 1024.0,
        "M" => n * 1e6 / f64::from(1 << 20),
        "Mi" => n,
        "G" => n * 1e9 / f64::from(1 << 20),
        "Gi" => n * 1024.0,
        _ => return None,
    };
    i32::try_from(mib.round() as i64).ok()
}

fn network(d: &Value, pointer: &str) -> Option<String> {
    str_at(d, pointer).map(|n| rid(&n).to_string())
}

fn service(asset: &Asset) -> Option<Service> {
    let d = &asset.resource.data;
    let product = |s: &str| s.to_string();
    Some(match asset.asset_type.as_str() {
        "sqladmin.googleapis.com/Instance" => {
            let (engine, engine_version) = engine_and_version(
                &str_at(d, "/databaseVersion").unwrap_or_default(),
            );
            Service::Database {
                product: product("cloud_sql"),
                engine,
                engine_version,
                instance_class: str_at(d, "/settings/tier"),
                allocated_storage_gib: int_at(d, "/settings/dataDiskSizeGb")
                    .and_then(|g| i32::try_from(g).ok()),
                high_availability: str_at(d, "/settings/availabilityType")
                    .map(|a| a == "REGIONAL"),
                network_id: network(
                    d,
                    "/settings/ipConfiguration/privateNetwork",
                ),
            }
        }
        "alloydb.googleapis.com/Cluster" => {
            let (_, engine_version) = engine_and_version(
                &str_at(d, "/databaseVersion").unwrap_or_default(),
            );
            Service::Database {
                product: product("alloydb"),
                engine: Some("postgres".into()),
                engine_version,
                instance_class: None,
                allocated_storage_gib: None,
                high_availability: None,
                network_id: network(d, "/networkConfig/network")
                    .or_else(|| network(d, "/network")),
            }
        }
        "spanner.googleapis.com/Instance" => Service::Database {
            product: product("spanner"),
            engine: Some("spanner".into()),
            engine_version: None,
            instance_class: str_at(d, "/config").map(|c| name(&c).to_string()),
            allocated_storage_gib: None,
            high_availability: None,
            network_id: None,
        },
        "container.googleapis.com/Cluster" => {
            Service::Kubernetes { product: product("gke") }
        }
        "run.googleapis.com/Service" => {
            // A second-generation function is also a Cloud Run service;
            // it is counted once, as the function.
            let managed_by = str_at(d, "/metadata/labels/goog-managed-by")
                .or_else(|| str_at(d, "/labels/goog-managed-by"));
            if managed_by.as_deref() == Some("cloudfunctions") {
                return None;
            }
            Service::Containers { product: product("cloud_run") }
        }
        "cloudfunctions.googleapis.com/CloudFunction" => Service::Functions {
            product: product("cloud_functions"),
            runtime: str_at(d, "/runtime"),
            memory_mib: int_at(d, "/availableMemoryMb")
                .and_then(|m| i32::try_from(m).ok()),
            network_id: None,
        },
        "cloudfunctions.googleapis.com/Function" => Service::Functions {
            product: product("cloud_functions"),
            runtime: str_at(d, "/buildConfig/runtime"),
            memory_mib: str_at(d, "/serviceConfig/availableMemory")
                .and_then(|m| memory_mib(&m)),
            network_id: None,
        },
        "redis.googleapis.com/Instance" => Service::Cache {
            product: product("memorystore_redis"),
            engine: Some("redis".into()),
            node_type: match (str_at(d, "/tier"), int_at(d, "/memorySizeGb")) {
                (Some(t), Some(gb)) => Some(format!("{t} {gb}GB")),
                (t, _) => t,
            },
            nodes: if str_at(d, "/tier").as_deref() == Some("STANDARD_HA") {
                int_at(d, "/replicaCount")
                    .map_or(Some(2), |r| i32::try_from(r + 1).ok())
            } else {
                Some(1)
            },
        },
        "redis.googleapis.com/Cluster" => Service::Cache {
            product: product("memorystore_redis_cluster"),
            engine: Some("redis".into()),
            node_type: str_at(d, "/nodeType"),
            nodes: int_at(d, "/shardCount")
                .map(|s| s * (1 + int_at(d, "/replicaCount").unwrap_or(0)))
                .and_then(|n| i32::try_from(n).ok()),
        },
        "bigtableadmin.googleapis.com/Instance" => {
            Service::KeyValue { product: product("bigtable") }
        }
        "file.googleapis.com/Instance" => Service::FileShare {
            product: product("filestore"),
            size_bytes: items_at(d, "/fileShares")
                .iter()
                .filter_map(|s| int_at(s, "/capacityGb"))
                .reduce(|a, b| a + b)
                .and_then(|gb| gib_to_bytes(Some(gb))),
        },
        _ => return None,
    })
}

fn resource(ctx: &Batch, asset: &Asset) -> Option<Resource> {
    let service = service(asset)?;
    Some(ctx.resource(
        asset,
        Body::ManagedService(ManagedBody {
            name: Some(name(&asset.name).to_string()),
            service,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::test::{asset, batch};

    fn one(asset_type: &str, data: Value) -> Asset {
        serde_json::from_value(asset(
            asset_type,
            &format!("//{asset_type}/projects/shop/locations/us-east1/things/orders-prod"),
            "us-east1",
            data,
        ))
        .unwrap()
    }

    #[test]
    fn cloud_sql_keeps_engine_tier_storage_ha_and_network() {
        let a = one(
            "sqladmin.googleapis.com/Instance",
            serde_json::json!({
                "databaseVersion": "MYSQL_8_0",
                "settings": {"tier": "db-custom-2-7680", "dataDiskSizeGb": "100",
                             "availabilityType": "REGIONAL",
                             "ipConfiguration": {"privateNetwork": "projects/shop/global/networks/prod"}},
            }),
        );
        let Some(Service::Database {
            product,
            engine,
            engine_version,
            instance_class,
            allocated_storage_gib,
            high_availability,
            network_id,
        }) = service(&a)
        else {
            panic!("not a database")
        };
        assert_eq!(product, "cloud_sql");
        assert_eq!(engine.as_deref(), Some("mysql"));
        assert_eq!(engine_version.as_deref(), Some("8.0"));
        assert_eq!(instance_class.as_deref(), Some("db-custom-2-7680"));
        assert_eq!(allocated_storage_gib, Some(100));
        assert_eq!(high_availability, Some(true));
        assert_eq!(
            network_id.as_deref(),
            Some("//compute.googleapis.com/projects/shop/global/networks/prod")
        );
    }

    #[test]
    fn functions_read_memory_in_either_generations_form() {
        let v1 = one(
            "cloudfunctions.googleapis.com/CloudFunction",
            serde_json::json!({"runtime": "python312", "availableMemoryMb": 256}),
        );
        let v2 = one(
            "cloudfunctions.googleapis.com/Function",
            serde_json::json!({"buildConfig": {"runtime": "go122"},
                               "serviceConfig": {"availableMemory": "1Gi"}}),
        );
        for (a, runtime, mib) in [(v1, "python312", 256), (v2, "go122", 1024)] {
            let Some(Service::Functions { runtime: r, memory_mib, .. }) =
                service(&a)
            else {
                panic!()
            };
            assert_eq!((r.as_deref(), memory_mib), (Some(runtime), Some(mib)));
        }
        assert_eq!(memory_mib("256M"), Some(244));
        assert_eq!(memory_mib("512Mi"), Some(512));
        assert_eq!(memory_mib("lots"), None);
    }

    #[test]
    fn a_functions_own_run_service_is_not_counted_twice() {
        let fn_service = one(
            "run.googleapis.com/Service",
            serde_json::json!({"metadata": {"labels": {"goog-managed-by": "cloudfunctions"}}}),
        );
        assert!(service(&fn_service).is_none());
        let app = one("run.googleapis.com/Service", serde_json::json!({}));
        assert_eq!(
            service(&app).map(|s| s.product().to_string()).as_deref(),
            Some("cloud_run")
        );
    }

    #[test]
    fn caches_and_file_shares_size_themselves() {
        let ha = one(
            "redis.googleapis.com/Instance",
            serde_json::json!({"tier": "STANDARD_HA", "memorySizeGb": 5, "replicaCount": 2}),
        );
        let Some(Service::Cache { node_type, nodes, .. }) = service(&ha) else {
            panic!()
        };
        assert_eq!(
            (node_type.as_deref(), nodes),
            (Some("STANDARD_HA 5GB"), Some(3))
        );
        let cluster = one(
            "redis.googleapis.com/Cluster",
            serde_json::json!({"shardCount": 3, "replicaCount": 1, "nodeType": "REDIS_HIGHMEM_MEDIUM"}),
        );
        let Some(Service::Cache { nodes, .. }) = service(&cluster) else {
            panic!()
        };
        assert_eq!(nodes, Some(6));
        let fs = one(
            "file.googleapis.com/Instance",
            serde_json::json!({"fileShares": [{"capacityGb": "1024"}]}),
        );
        let Some(Service::FileShare { size_bytes, .. }) = service(&fs) else {
            panic!()
        };
        assert_eq!(size_bytes, Some(1024 << 30));
    }

    #[test]
    fn managed_resources_are_named_labelled_and_unknown_types_skipped() {
        let b = batch(serde_json::json!([
            asset(
                "bigtableadmin.googleapis.com/Instance",
                "//bigtableadmin.googleapis.com/projects/shop/instances/orders-prod",
                "us-east1",
                serde_json::json!({"labels": {"env": "prod"}})
            ),
            asset(
                "storage.googleapis.com/Bucket",
                "//storage.googleapis.com/b",
                "us",
                serde_json::json!({})
            ),
        ]));
        let r: Vec<Resource> = b
            .inventory
            .of_types(MANAGED)
            .filter_map(|a| resource(&b, a))
            .collect();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name(), Some("orders-prod"));
        assert_eq!(r[0].tags["env"], "prod");
        assert_eq!(engine_and_version(""), (None, None));
        assert_eq!(
            engine_and_version("SPANNER"),
            (Some("spanner".into()), None)
        );
    }
}
