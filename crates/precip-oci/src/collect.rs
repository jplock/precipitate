//! The context every OCI collector reads from, and the registry.

use crate::api::{Oci, PAGE, Rows, Service};
use crate::{compute, network};
use precip_collect::Collector;
use precip_collect::http::HttpError;
use precip_types::scan::Resource;
use precip_types::{AccountId, Arn, Body, Region};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// One compartment in one region.
pub(crate) struct Pair {
    pub oci: Oci,
    pub region: String,
    pub compartment: String,
    pub domains: Vec<String>,
}

impl Pair {
    /// Every row of a Core Services list call in this compartment, with
    /// `extra` query parameters.
    pub(crate) async fn list<T: DeserializeOwned>(
        &self,
        path: &str,
        extra: &[(&str, &str)],
    ) -> Result<Vec<T>, HttpError> {
        let mut query =
            vec![("compartmentId", self.compartment.as_str()), ("limit", PAGE)];
        query.extend_from_slice(extra);
        let url = self.oci.url(Service::Core, &self.region, path, &query)?;
        self.oci.list(url, None, Rows::Array).await
    }

    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, HttpError> {
        self.oci
            .get(self.oci.url(Service::Core, &self.region, path, &[])?)
            .await
    }

    /// A resource in this compartment and region, tagged from `v`.
    pub(crate) fn resource(&self, id: &str, v: &Value, body: Body) -> Resource {
        Resource {
            arn: Arn::from(id),
            account_id: AccountId::from(self.compartment.as_str()),
            region: Region::from(self.region.as_str()),
            tags: tags(v),
            body,
        }
    }
}

/// Free-form tags as they are, and defined tags as `<namespace>.<key>`.
/// Values that are not strings are dropped.
pub(crate) fn tags(v: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(free) = v.get("freeformTags").and_then(Value::as_object) {
        for (k, val) in free {
            if let Some(s) = val.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    if let Some(defined) = v.get("definedTags").and_then(Value::as_object) {
        for (ns, keys) in defined {
            for (k, val) in keys.as_object().into_iter().flatten() {
                if let Some(s) = val.as_str() {
                    out.insert(format!("{ns}.{k}"), s.to_string());
                }
            }
        }
    }
    out
}

/// A string field, or `None` when missing or blank.
pub(crate) fn str_at(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The strings of an array at `pointer`.
pub(crate) fn strings(v: &Value, pointer: &str) -> Vec<String> {
    v.pointer(pointer)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter().filter_map(Value::as_str).map(str::to_string).collect()
        })
        .unwrap_or_default()
}

pub(crate) fn num_at(v: &Value, pointer: &str) -> Option<f64> {
    v.pointer(pointer).and_then(Value::as_f64)
}

/// Whether a lifecycle state means the resource is gone.
pub(crate) fn is_gone(v: &Value) -> bool {
    matches!(
        str_at(v, "/lifecycleState").as_deref(),
        Some("TERMINATED" | "TERMINATING" | "DETACHED" | "DETACHING")
    )
}

/// Every collector.
pub(crate) fn all() -> Vec<Arc<dyn Collector<Pair>>> {
    vec![Arc::new(compute::Compute), Arc::new(network::Network)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_form_and_defined_tags_are_flattened() {
        let t = tags(&serde_json::json!({
            "freeformTags": {"app": "shop", "n": 1},
            "definedTags": {"Oracle-Tags": {"CreatedBy": "ana"}, "Ops": {"Tier": "web", "Cost": 3}},
        }));
        assert_eq!(
            t,
            BTreeMap::from([
                ("app".into(), "shop".into()),
                ("Oracle-Tags.CreatedBy".into(), "ana".into()),
                ("Ops.Tier".into(), "web".into()),
            ])
        );
        assert!(tags(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn terminated_and_detached_resources_are_gone() {
        assert!(is_gone(&serde_json::json!({"lifecycleState": "TERMINATED"})));
        assert!(is_gone(&serde_json::json!({"lifecycleState": "DETACHED"})));
        assert!(!is_gone(&serde_json::json!({"lifecycleState": "RUNNING"})));
        assert!(!is_gone(&serde_json::json!({})));
    }
}
