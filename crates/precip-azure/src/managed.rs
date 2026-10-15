//! Managed services with no Oxide equivalent. One query returns every
//! type; each row's properties are read for the few fields precip keeps.

use crate::collect::{BASE, Base, Batch};
use crate::id::{name, parent, text};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::body::{Managed as ManagedBody, Service};
use precip_types::{Body, scan::Resource};
use serde::Deserialize;
use serde_json::Value;

const API: &str = "Microsoft.ResourceGraph/resources/read (managed services)";

pub(crate) struct Managed;

impl Collect for Managed {
    type Context = Batch;
    const API: &'static str = API;

    async fn collect(
        &self,
        ctx: &Batch,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let rows: Vec<Row> = ctx.graph(API, &query()).await?;
        Ok(Collected {
            resources: rows.iter().filter_map(Row::resource).collect(),
            ..Default::default()
        })
    }
}

const TYPES: &[&str] = &[
    "microsoft.sql/servers/databases",
    "microsoft.sql/managedinstances",
    "microsoft.dbforpostgresql/flexibleservers",
    "microsoft.dbformysql/flexibleservers",
    "microsoft.containerservice/managedclusters",
    "microsoft.app/containerapps",
    "microsoft.containerinstance/containergroups",
    "microsoft.web/sites",
    "microsoft.cache/redis",
    "microsoft.documentdb/databaseaccounts",
    "microsoft.netapp/netappaccounts/capacitypools/volumes",
];

fn query() -> String {
    let types: Vec<String> = TYPES.iter().map(|t| format!("'{t}'")).collect();
    format!(
        "resources
| where type in~ ({})
| where not(type =~ 'microsoft.sql/servers/databases' and name =~ 'master')
| project {BASE}, type = tolower(type), kind, sku, properties",
        types.join(", ")
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Row {
    #[serde(flatten)]
    pub base: Base,
    #[serde(rename = "type")]
    pub kind_of: String,
    pub kind: Value,
    pub sku: Value,
    pub properties: Value,
}

fn str_at(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer).and_then(Value::as_str).and_then(text)
}

fn int_at(v: &Value, pointer: &str) -> Option<i64> {
    let at = v.pointer(pointer)?;
    at.as_i64().or_else(|| at.as_str()?.parse().ok())
}

fn gib(bytes: Option<i64>) -> Option<i32> {
    bytes.and_then(|b| i32::try_from(b / (1 << 30)).ok())
}

/// The network of a subnet id found at `pointer`.
fn network_at(v: &Value, pointer: &str) -> Option<String> {
    parent(&str_at(v, pointer)?, "subnets")
}

impl Row {
    fn service(&self) -> Option<Service> {
        let p = &self.properties;
        let product = |s: &str| s.to_string();
        let database = |product: &str,
                        engine: &str,
                        storage_gib: Option<i32>,
                        ha: Option<bool>,
                        net: Option<String>| {
            Service::Database {
                product: product.to_string(),
                engine: Some(engine.to_string()),
                engine_version: str_at(p, "/version"),
                instance_class: str_at(&self.sku, "/name"),
                allocated_storage_gib: storage_gib,
                high_availability: ha,
                network_id: net,
            }
        };
        let flexible_ha = || {
            str_at(p, "/highAvailability/mode")
                .map(|m| !m.eq_ignore_ascii_case("disabled"))
        };
        let flexible_storage = || {
            int_at(p, "/storage/storageSizeGB")
                .and_then(|g| i32::try_from(g).ok())
        };
        Some(match self.kind_of.as_str() {
            "microsoft.sql/servers/databases" => database(
                "sql_database",
                "sqlserver",
                gib(int_at(p, "/maxSizeBytes")),
                p.pointer("/zoneRedundant").and_then(Value::as_bool),
                None,
            ),
            "microsoft.sql/managedinstances" => database(
                "sql_managed_instance",
                "sqlserver",
                int_at(p, "/storageSizeInGB")
                    .and_then(|g| i32::try_from(g).ok()),
                p.pointer("/zoneRedundant").and_then(Value::as_bool),
                network_at(p, "/subnetId"),
            ),
            "microsoft.dbforpostgresql/flexibleservers" => database(
                "postgresql_flexible",
                "postgres",
                flexible_storage(),
                flexible_ha(),
                network_at(p, "/network/delegatedSubnetResourceId"),
            ),
            "microsoft.dbformysql/flexibleservers" => database(
                "mysql_flexible",
                "mysql",
                flexible_storage(),
                flexible_ha(),
                network_at(p, "/network/delegatedSubnetResourceId"),
            ),
            "microsoft.containerservice/managedclusters" => {
                Service::Kubernetes { product: product("aks") }
            }
            "microsoft.app/containerapps" => {
                Service::Containers { product: product("container_apps") }
            }
            "microsoft.containerinstance/containergroups" => {
                Service::Containers { product: product("container_instances") }
            }
            "microsoft.web/sites" => {
                let kind = self.kind.as_str().unwrap_or_default();
                if kind.contains("functionapp") {
                    Service::Functions {
                        product: product("functions"),
                        runtime: str_at(p, "/siteConfig/linuxFxVersion"),
                        memory_mib: None,
                        network_id: network_at(p, "/virtualNetworkSubnetId"),
                    }
                } else {
                    Service::Containers { product: product("app_service") }
                }
            }
            "microsoft.cache/redis" => Service::Cache {
                product: product("azure_cache_for_redis"),
                engine: Some("redis".into()),
                node_type: match (
                    str_at(p, "/sku/name"),
                    str_at(p, "/sku/family"),
                    int_at(p, "/sku/capacity"),
                ) {
                    (Some(n), Some(f), Some(c)) => Some(format!("{n} {f}{c}")),
                    (n, _, _) => n,
                },
                nodes: int_at(p, "/shardCount")
                    .and_then(|n| i32::try_from(n).ok()),
            },
            "microsoft.documentdb/databaseaccounts" => {
                Service::KeyValue { product: product("cosmos_db") }
            }
            "microsoft.netapp/netappaccounts/capacitypools/volumes" => {
                Service::FileShare {
                    product: product("netapp_files"),
                    size_bytes: int_at(p, "/usageThreshold"),
                }
            }
            _ => return None,
        })
    }

    fn resource(&self) -> Option<Resource> {
        let service = self.service()?;
        Some(self.base.resource(Body::ManagedService(ManagedBody {
            name: Some(name(&self.base.id).to_string()),
            service,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind_of: &str, extra: Value) -> Row {
        let mut v = serde_json::json!({
            "id": format!("/subscriptions/s1/resourceGroups/shop/providers/{kind_of}/orders-prod"),
            "name": "orders-prod",
            "location": "eastus",
            "subscriptionId": "s1",
            "type": kind_of,
        });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn databases_keep_engine_size_storage_and_network() {
        let r = row(
            "microsoft.dbforpostgresql/flexibleservers",
            serde_json::json!({
                "sku": {"name": "Standard_D2ds_v4"},
                "properties": {
                    "version": "16",
                    "storage": {"storageSizeGB": 128},
                    "highAvailability": {"mode": "ZoneRedundant"},
                    "network": {"delegatedSubnetResourceId": "/subscriptions/s1/resourceGroups/net/providers/Microsoft.Network/virtualNetworks/hub/subnets/db"},
                },
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
        }) = r.service()
        else {
            panic!("not a database")
        };
        assert_eq!(product, "postgresql_flexible");
        assert_eq!(engine.as_deref(), Some("postgres"));
        assert_eq!(engine_version.as_deref(), Some("16"));
        assert_eq!(instance_class.as_deref(), Some("Standard_D2ds_v4"));
        assert_eq!(allocated_storage_gib, Some(128));
        assert_eq!(high_availability, Some(true));
        assert_eq!(
            network_id.as_deref(),
            Some(
                "/subscriptions/s1/resourcegroups/net/providers/microsoft.network/virtualnetworks/hub"
            )
        );
    }

    #[test]
    fn sql_database_size_is_in_bytes() {
        let r = row(
            "microsoft.sql/servers/databases",
            serde_json::json!({"properties": {"maxSizeBytes": 34_359_738_368_i64, "zoneRedundant": false}}),
        );
        let Some(Service::Database {
            allocated_storage_gib,
            high_availability,
            ..
        }) = r.service()
        else {
            panic!()
        };
        assert_eq!(
            (allocated_storage_gib, high_availability),
            (Some(32), Some(false))
        );
    }

    #[test]
    fn web_sites_split_into_functions_and_app_service() {
        let f = row(
            "microsoft.web/sites",
            serde_json::json!({"kind": "functionapp,linux"}),
        );
        assert!(matches!(f.service(), Some(Service::Functions { .. })));
        let w = row("microsoft.web/sites", serde_json::json!({"kind": "app"}));
        assert_eq!(
            w.service().map(|s| s.product().to_string()).as_deref(),
            Some("app_service")
        );
    }

    #[test]
    fn redis_names_its_tier_and_unknown_types_are_skipped() {
        let r = row(
            "microsoft.cache/redis",
            serde_json::json!({"properties": {"sku": {"name": "Premium", "family": "P", "capacity": "1"}, "shardCount": 2}}),
        );
        let Some(Service::Cache { node_type, nodes, .. }) = r.service() else {
            panic!()
        };
        assert_eq!(node_type.as_deref(), Some("Premium P1"));
        assert_eq!(nodes, Some(2));
        assert!(
            row("microsoft.storage/storageaccounts", Value::Null)
                .resource()
                .is_none()
        );
    }

    #[test]
    fn managed_resources_are_named_by_their_last_id_segment() {
        let r = row("microsoft.documentdb/databaseaccounts", Value::Null)
            .resource()
            .unwrap();
        assert_eq!(r.name(), Some("orders-prod"));
        assert_eq!(r.region, "eastus");
    }
}
