//! The context every Azure collector reads from, and the registry.

use crate::arm::Arm;
use crate::id::{rid, tags};
use crate::{compute, groups, managed, network, skus};
use precip_collect::{CollectError, Collector};
use precip_types::scan::Resource;
use precip_types::{AccountId, Body, Region};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

/// One batch of subscriptions, scanned together: Resource Graph answers
/// for up to 1,000 subscriptions per query.
pub(crate) struct Batch {
    pub arm: Arm,
    pub subscriptions: Vec<String>,
}

impl Batch {
    /// Run a Resource Graph query over the batch, with a failure named
    /// after `api`.
    pub(crate) async fn graph<T: DeserializeOwned>(
        &self,
        api: &'static str,
        query: &str,
    ) -> Result<Vec<T>, CollectError> {
        self.arm
            .graph(&self.subscriptions, query)
            .await
            .map_err(CollectError::new(api))
    }
}

/// The columns every Resource Graph row projects first.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Base {
    pub id: String,
    pub name: String,
    pub location: String,
    pub subscription_id: String,
    pub tags: serde_json::Value,
}

/// The KQL `project` list for [`Base`].
pub(crate) const BASE: &str = "id, name, location, subscriptionId, tags";

impl Base {
    pub(crate) fn resource(&self, body: Body) -> Resource {
        Resource {
            arn: rid(&self.id),
            account_id: AccountId::from(self.subscription_id.to_lowercase()),
            region: Region::from(self.location.to_lowercase()),
            tags: tags(&self.tags),
            body,
        }
    }
}

/// Every collector. Second-stage collectors run after the rest, in this
/// order.
pub(crate) fn all() -> Vec<Arc<dyn Collector<Batch>>> {
    vec![
        Arc::new(compute::Compute),
        Arc::new(network::Network),
        Arc::new(network::LoadBalancers),
        Arc::new(managed::Managed),
        // Second stage, in order: each reads what ran before it.
        Arc::new(skus::Skus),
        Arc::new(groups::ResourceGroups),
    ]
}
