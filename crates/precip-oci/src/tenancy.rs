//! The tenancy: its compartments, the regions it subscribes to, and each
//! region's availability domains. Compartments are the accounts precip
//! records; the tenancy itself is the root compartment.

use crate::api::{Oci, PAGE, Rows, Service};
use crate::search;
use precip_collect::http::HttpError;
use precip_types::scan::Resource;
use precip_types::{AccountId, Arn, Body, Region, body};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Compartment {
    pub id: String,
    pub name: String,
    pub state: String,
    /// The compartment directly above; none for the root.
    pub parent: Option<String>,
    pub parent_name: Option<String>,
}

impl Compartment {
    pub(crate) fn account_id(&self) -> AccountId {
        AccountId::from(self.id.as_str())
    }

    /// The compartment as an account, in `realm` (`oc1`).
    pub(crate) fn to_resource(&self, realm: &str) -> Resource {
        Resource {
            arn: Arn::from(self.id.as_str()),
            account_id: self.account_id(),
            region: Region::global(),
            tags: Default::default(),
            body: Body::Account(body::Account {
                name: self.name.clone(),
                status: self.state.clone(),
                parent_id: self.parent.clone(),
                parent_name: self.parent_name.clone(),
                cloud: realm.into(),
            }),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Listed {
    id: String,
    compartment_id: String,
    name: String,
    lifecycle_state: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Named {
    name: String,
}

/// The root compartment and every active compartment below it the caller
/// can read. The tenancy's own name needs `inspect tenancies`; without it
/// the root is called `root`.
pub(crate) async fn compartments(
    oci: &Oci,
    region: &str,
    tenancy: &str,
) -> Result<Vec<Compartment>, HttpError> {
    let url = oci.url(
        Service::Identity,
        region,
        "/20160918/compartments",
        &[
            ("compartmentId", tenancy),
            ("compartmentIdInSubtree", "true"),
            ("accessLevel", "ACCESSIBLE"),
            ("lifecycleState", "ACTIVE"),
            ("limit", PAGE),
        ],
    )?;
    let listed: Vec<Listed> = oci.list(url, None, Rows::Array).await?;
    let root_name = match oci
        .get::<Named>(oci.url(
            Service::Identity,
            region,
            &format!("/20160918/tenancies/{tenancy}"),
            &[],
        )?)
        .await
    {
        Ok(t) if !t.name.is_empty() => t.name,
        Ok(_) => "root".into(),
        Err(e) => {
            tracing::warn!(error = %precip_collect::error_chain(&e), "reading the tenancy's name");
            "root".into()
        }
    };
    Ok(assemble(tenancy, root_name, listed))
}

fn assemble(
    tenancy: &str,
    root_name: String,
    listed: Vec<Listed>,
) -> Vec<Compartment> {
    let mut names: HashMap<String, String> =
        listed.iter().map(|c| (c.id.clone(), c.name.clone())).collect();
    names.insert(tenancy.to_string(), root_name.clone());
    let mut out = vec![Compartment {
        id: tenancy.to_string(),
        name: root_name,
        state: "ACTIVE".into(),
        parent: None,
        parent_name: None,
    }];
    for c in listed {
        out.push(Compartment {
            parent_name: names.get(&c.compartment_id).cloned(),
            parent: Some(c.compartment_id).filter(|p| !p.is_empty()),
            id: c.id,
            name: c.name,
            state: c.lifecycle_state,
        });
    }
    out
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Subscription {
    region_name: String,
    status: String,
}

/// The regions the tenancy subscribes to and can use.
pub(crate) async fn regions(
    oci: &Oci,
    region: &str,
    tenancy: &str,
) -> Result<Vec<String>, HttpError> {
    let url = oci.url(
        Service::Identity,
        region,
        &format!("/20160918/tenancies/{tenancy}/regionSubscriptions"),
        &[],
    )?;
    let subs: Vec<Subscription> = oci.get(url).await?;
    let mut out: Vec<String> = subs
        .into_iter()
        .filter(|s| s.status == "READY")
        .map(|s| s.region_name)
        .collect();
    out.sort();
    Ok(out)
}

/// What a scan needs to know about one region before its compartments.
#[derive(Debug, Clone)]
pub(crate) struct RegionPlan {
    pub region: String,
    /// The region's availability domains, or why they could not be read.
    pub domains: Result<Vec<String>, String>,
    /// Compartments with resources in the region, by Resource Search;
    /// unknown when the search failed.
    pub occupied: Option<HashSet<String>>,
}

pub(crate) async fn plan(oci: &Oci, region: &str, tenancy: &str) -> RegionPlan {
    let domains = async {
        let url = oci.url(
            Service::Identity,
            region,
            "/20160918/availabilityDomains",
            &[("compartmentId", tenancy)],
        )?;
        let ads: Vec<Named> = oci.get(url).await?;
        Ok::<_, HttpError>(ads.into_iter().map(|a| a.name).collect())
    }
    .await
    .map_err(|e| {
        format!(
            "listing availability domains: {}",
            precip_collect::error_chain(&e)
        )
    });
    let occupied = match search::occupied(oci, region).await {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(region, error = %precip_collect::error_chain(&e),
                "Resource Search failed; scanning every compartment");
            None
        }
    };
    RegionPlan { region: region.to_string(), domains, occupied }
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_collect::http::fake::{Script, ok, status};
    use std::sync::Arc;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("rt")
            .block_on(f)
    }

    #[test]
    fn compartments_hang_under_the_named_root() {
        let script = Arc::new(Script::routes(vec![
            (
                "/compartments",
                ok(serde_json::json!([
                    {"id": "c.shop", "compartmentId": "t", "name": "shop", "lifecycleState": "ACTIVE"},
                    {"id": "c.prod", "compartmentId": "c.shop", "name": "shop-prod", "lifecycleState": "ACTIVE"},
                ])),
            ),
            ("/tenancies/t", ok(serde_json::json!({"name": "acme"}))),
        ]));
        let oci = Oci::fake(script.clone());
        let got = block_on(compartments(&oci, "us-ashburn-1", "t")).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(
            (got[0].name.as_str(), got[0].parent.as_deref()),
            ("acme", None)
        );
        assert_eq!(got[2].parent_name.as_deref(), Some("shop"));
        assert_eq!(got[1].parent_name.as_deref(), Some("acme"));
        assert!(
            script.urls()[0]
                .contains("compartmentIdInSubtree=true&accessLevel=ACCESSIBLE")
        );
        let r = got[2].to_resource("oc1");
        assert_eq!(
            (r.account_id.as_str(), r.region.as_str()),
            ("c.prod", "global")
        );
        let Body::Account(a) = &r.body else { panic!() };
        assert_eq!(a.cloud, "oc1");
    }

    #[test]
    fn an_unreadable_tenancy_name_falls_back_to_root() {
        let script = Arc::new(Script::routes(vec![
            ("/compartments", ok(serde_json::json!([]))),
            (
                "/tenancies/t",
                status(
                    404,
                    serde_json::json!({"code": "NotAuthorizedOrNotFound"}),
                ),
            ),
        ]));
        let got = block_on(compartments(&Oci::fake(script), "r", "t")).unwrap();
        assert_eq!(got[0].name, "root");
    }

    #[test]
    fn only_ready_regions_are_scanned_and_a_region_plan_survives_failures() {
        let script = Arc::new(Script::routes(vec![
            (
                "regionSubscriptions",
                ok(serde_json::json!([
                    {"regionName": "us-phoenix-1", "status": "READY"},
                    {"regionName": "us-ashburn-1", "status": "READY"},
                    {"regionName": "eu-paris-1", "status": "IN_PROGRESS"},
                ])),
            ),
            (
                "availabilityDomains",
                status(
                    404,
                    serde_json::json!({"code": "NotAuthorizedOrNotFound"}),
                ),
            ),
        ]));
        let oci = Oci::fake(script);
        assert_eq!(
            block_on(regions(&oci, "r", "t")).unwrap(),
            ["us-ashburn-1", "us-phoenix-1"]
        );
        let p = block_on(plan(&oci, "us-ashburn-1", "t"));
        assert!(p.domains.unwrap_err().contains("NotAuthorizedOrNotFound"));
        assert!(p.occupied.is_none(), "the search was not answered");
    }
}
