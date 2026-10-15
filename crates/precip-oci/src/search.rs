//! Resource Search, used only to skip compartments with nothing to
//! collect in a region, as AWS scans skip regions Resource Explorer
//! reports empty.

use crate::api::{Oci, PAGE, Rows, Service};
use precip_collect::http::HttpError;
use serde::Deserialize;
use std::collections::HashSet;

/// The resource types the collectors read.
const QUERY: &str = "query instance, volume, bootvolume, vcn, subnet, \
    securitylist, networksecuritygroup, routetable, internetgateway, \
    natgateway, publicip resources";

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Summary {
    compartment_id: String,
    lifecycle_state: String,
}

/// The compartments with a live resource of a collected type in `region`.
pub(crate) async fn occupied(
    oci: &Oci,
    region: &str,
) -> Result<HashSet<String>, HttpError> {
    let url = oci.url(
        Service::Search,
        region,
        "/20180409/resources",
        &[("limit", PAGE)],
    )?;
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "Structured",
        "query": QUERY,
        "matchingContextType": "NONE",
    }))?;
    let rows: Vec<Summary> = oci.list(url, Some(body), Rows::Items).await?;
    Ok(rows
        .into_iter()
        .filter(|r| !r.lifecycle_state.eq_ignore_ascii_case("TERMINATED"))
        .map(|r| r.compartment_id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_collect::http::fake::{Script, ok};
    use std::sync::Arc;

    #[test]
    fn compartments_with_live_resources_are_occupied() {
        let script =
            Arc::new(Script::queue(vec![ok(serde_json::json!({"items": [
                {"compartmentId": "a", "lifecycleState": "RUNNING"},
                {"compartmentId": "b", "lifecycleState": "TERMINATED"},
                {"compartmentId": "a", "lifecycleState": "AVAILABLE"},
            ]}))]));
        let oci = Oci::fake(script.clone());
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(occupied(&oci, "us-ashburn-1"))
            .unwrap();
        assert_eq!(got, HashSet::from(["a".to_string()]));
        let sent = &script.sent()[0];
        assert!(sent.url.contains("query.us-ashburn-1.oci.oraclecloud.com/20180409/resources?limit=1000"));
        assert!(
            sent.body
                .as_deref()
                .unwrap_or_default()
                .contains("\"type\":\"Structured\"")
        );
    }
}
