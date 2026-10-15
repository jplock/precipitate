//! Subscriptions the caller can read, with the management group each sits
//! in.

use crate::AzureCloud;
use crate::arm::{Arm, ArmError, GRAPH_MAX_SUBSCRIPTIONS};
use crate::id::text;
use precip_types::scan::Resource;
use precip_types::{AccountId, Arn, Body, Region, body};
use serde::Deserialize;
use std::collections::HashMap;

const API_VERSION: &str = "2022-12-01";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Subscription {
    pub subscription_id: String,
    pub display_name: String,
    pub state: String,
    pub tenant_id: String,
    #[serde(skip)]
    pub parent: Option<Parent>,
}

/// The management group directly above a subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Parent {
    pub id: String,
    pub name: String,
    /// Every management group above, nearest first, by id.
    pub ancestors: Vec<String>,
}

impl Subscription {
    /// Disabled and deleted subscriptions answer no reads.
    pub(crate) fn is_readable(&self) -> bool {
        !matches!(self.state.as_str(), "Disabled" | "Deleted")
    }

    pub(crate) fn id(&self) -> AccountId {
        AccountId::from(self.subscription_id.to_lowercase())
    }

    pub(crate) fn to_resource(&self, cloud: AzureCloud) -> Resource {
        let id = self.id();
        Resource {
            arn: Arn::from(format!("/subscriptions/{id}")),
            account_id: id,
            region: Region::global(),
            tags: Default::default(),
            body: Body::Account(body::Account {
                name: self.display_name.clone(),
                status: self.state.clone(),
                parent_id: self.parent.as_ref().map(|p| p.id.clone()),
                parent_name: self.parent.as_ref().map(|p| p.name.clone()),
                cloud: cloud.name().into(),
            }),
        }
    }
}

pub(crate) async fn list(arm: &Arm) -> Result<Vec<Subscription>, ArmError> {
    arm.list(&format!("/subscriptions?api-version={API_VERSION}")).await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ChainRow {
    subscription_id: String,
    chain: Vec<ChainLink>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ChainLink {
    name: String,
    display_name: String,
}

const CHAIN_QUERY: &str = "resourcecontainers
| where type =~ 'microsoft.resources/subscriptions'
| project subscriptionId, chain = properties.managementGroupAncestorsChain";

/// The management group of each subscription, by lowercased id. Without
/// read access to management groups the chain is empty.
pub(crate) async fn parents(
    arm: &Arm,
    subscriptions: &[String],
) -> Result<HashMap<String, Parent>, ArmError> {
    let mut out = HashMap::new();
    for batch in subscriptions.chunks(GRAPH_MAX_SUBSCRIPTIONS) {
        let rows: Vec<ChainRow> = arm.graph(batch, CHAIN_QUERY).await?;
        out.extend(rows.into_iter().filter_map(parent_of));
    }
    Ok(out)
}

fn parent_of(row: ChainRow) -> Option<(String, Parent)> {
    let first = row.chain.first()?;
    let id = text(&first.name)?;
    Some((
        row.subscription_id.to_lowercase(),
        Parent {
            name: text(&first.display_name).unwrap_or_else(|| id.clone()),
            id,
            ancestors: row.chain.iter().filter_map(|l| text(&l.name)).collect(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_management_group_is_the_parent() {
        let row: ChainRow = serde_json::from_value(serde_json::json!({
            "subscriptionId": "S1",
            "chain": [
                {"name": "prod-mg", "displayName": "Production"},
                {"name": "tenant-root", "displayName": "Tenant Root Group"},
            ],
        }))
        .unwrap();
        let (sub, p) = parent_of(row).unwrap();
        assert_eq!(sub, "s1");
        assert_eq!(p.id, "prod-mg");
        assert_eq!(p.name, "Production");
        assert_eq!(p.ancestors, ["prod-mg", "tenant-root"]);
        assert!(parent_of(ChainRow::default()).is_none());
    }

    #[test]
    fn subscriptions_become_global_accounts() {
        let s = Subscription {
            subscription_id: "ABC".into(),
            display_name: "shop-prod".into(),
            state: "Enabled".into(),
            parent: Some(Parent {
                id: "mg".into(),
                name: "Prod".into(),
                ancestors: vec![],
            }),
            ..Default::default()
        };
        assert!(s.is_readable());
        let r = s.to_resource(AzureCloud::UsGovernment);
        assert_eq!(r.arn, "/subscriptions/abc");
        assert_eq!(r.region, "global");
        let Body::Account(a) = &r.body else { panic!() };
        assert_eq!(a.parent_name.as_deref(), Some("Prod"));
        assert_eq!(a.cloud, "AzureUSGovernment");
        let off =
            Subscription { state: "Disabled".into(), ..Default::default() };
        assert!(!off.is_readable());
    }
}
