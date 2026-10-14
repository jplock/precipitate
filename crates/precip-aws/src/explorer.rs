//! AWS Resource Explorer, used only to skip empty regions. It indexes every
//! resource type, so a region with a zero count of workload types has
//! nothing for the collectors to find. It is optional: when no index can
//! answer, every enabled region is scanned.
//!
//! Two modes. An organization-wide aggregator in the management account
//! answers for every member in one place, but only when multi-account
//! search is enabled; that is probed, never assumed. Otherwise each account
//! is asked about itself through its own aggregator.

use aws_config::SdkConfig;

/// Resource types whose presence means a region has something worth
/// collecting. Every region also holds AWS-created defaults (a default
/// VPC, security group, route tables) and, under Control Tower, guardrail
/// CloudFormation stacks, so neither counts. Values of one filter are
/// joined with OR when comma-separated; repeating the filter name would
/// AND them and match nothing.
const WORKLOAD_RESOURCE_TYPES: &[&str] = &[
    "ec2:instance",
    "ec2:volume",
    "autoscaling:autoScalingGroup",
    "elasticloadbalancing:loadbalancer",
    "rds:db",
    "eks:cluster",
    "ecs:cluster",
    "lambda:function",
    "elasticache:cluster",
    "dynamodb:table",
    "elasticfilesystem:file-system",
];

fn region_query(region: &str, account_id: Option<&str>) -> String {
    let mut q = format!(
        "region:{region} resourcetype:{}",
        WORKLOAD_RESOURCE_TYPES.join(",")
    );
    if let Some(id) = account_id {
        q.push_str(&format!(" accountid:{id}"));
    }
    q
}

/// A client bound to an aggregator index and its default view.
#[derive(Debug, Clone)]
pub struct Explorer {
    client: aws_sdk_resourceexplorer2::Client,
    view: String,
    pub aggregator_region: String,
}

impl Explorer {
    /// Find the account's aggregator index and default view. `None` when
    /// Resource Explorer is not set up or not permitted.
    pub async fn discover(config: &SdkConfig) -> Option<Self> {
        // ListIndexes answers from any region; it returns indexes in all of
        // them. The aggregator index is the one that can search across
        // regions.
        let probe = config
            .to_builder()
            .region(
                config
                    .region()
                    .cloned()
                    .unwrap_or_else(|| aws_config::Region::new("us-east-1")),
            )
            .build();
        let client = aws_sdk_resourceexplorer2::Client::new(&probe);
        let indexes = match client.list_indexes().send().await {
            Ok(out) => out,
            Err(e) => {
                tracing::debug!(error = %e, "resource explorer unavailable");
                return None;
            }
        };
        let aggregator_region = indexes.indexes().iter().find_map(|i| {
            let is_aggregator =
                i.r#type().is_some_and(|t| t.as_str() == "AGGREGATOR");
            is_aggregator.then(|| i.region().map(str::to_string)).flatten()
        })?;

        let aggregator = config
            .to_builder()
            .region(aws_config::Region::new(aggregator_region.clone()))
            .build();
        let client = aws_sdk_resourceexplorer2::Client::new(&aggregator);
        let view = match client.get_default_view().send().await {
            Ok(out) => out.view_arn().map(str::to_string),
            Err(e) => {
                tracing::debug!(error = %e, "no default resource explorer view");
                return None;
            }
        }?;
        Some(Self { client, view, aggregator_region })
    }

    /// Whether this aggregator indexes accounts other than `own_account`.
    /// True only with organization-wide multi-account search enabled; a
    /// single-account aggregator returns zero here and must not be used to
    /// prune other accounts' regions.
    pub async fn sees_other_accounts(&self, own_account: &str) -> bool {
        let out = self
            .client
            .search()
            .view_arn(&self.view)
            .query_string(format!("-accountid:{own_account}"))
            .max_results(1)
            .send()
            .await;
        match out {
            Ok(out) => {
                let total = out.count().and_then(|c| c.total_resources());
                tracing::info!(
                    aggregator = %self.aggregator_region,
                    other_account_resources = total,
                    "resource explorer multi-account probe"
                );
                total.is_some_and(|n| n > 0)
            }
            Err(e) => {
                tracing::debug!(error = %e, "multi-account probe failed");
                false
            }
        }
    }

    /// Regions with at least one workload resource for `account_id` (or
    /// the index's own account when `None`), or `None` when a search
    /// fails. A region is pruned only on a definite zero count.
    pub async fn regions_with_resources(
        &self,
        account_id: Option<&str>,
        regions: &[String],
    ) -> Option<Vec<String>> {
        let mut active = Vec::new();
        for region in regions {
            let out = self
                .client
                .search()
                .view_arn(&self.view)
                .query_string(region_query(region, account_id))
                .max_results(1)
                .send()
                .await;
            match out {
                Ok(out) => {
                    let total = out.count().and_then(|c| c.total_resources());
                    tracing::debug!(
                        region,
                        account = account_id,
                        total,
                        "resource explorer count"
                    );
                    if total != Some(0) {
                        active.push(region.clone());
                    }
                }
                Err(e) => {
                    tracing::debug!(region, error = %e, "resource explorer search failed");
                    return None;
                }
            }
        }
        tracing::info!(
            aggregator = %self.aggregator_region,
            account = account_id,
            enabled = regions.len(),
            with_resources = active.len(),
            "resource explorer pruned regions"
        );
        Some(active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_query_filters_to_workload_types() {
        let q = region_query("us-east-1", None);
        assert!(q.starts_with("region:us-east-1 resourcetype:ec2:instance,"));
        assert!(q.ends_with(",elasticfilesystem:file-system"));
        assert_eq!(
            q.matches("resourcetype:").count(),
            1,
            "one filter, many values"
        );
        assert!(!q.contains("ec2:vpc"), "default VPCs must not count");
        assert!(
            !q.contains("cloudformation"),
            "guardrail stacks must not count"
        );
    }

    #[test]
    fn account_filter_is_appended_for_organization_search() {
        let q = region_query("eu-west-1", Some("123456789012"));
        assert!(q.ends_with(" accountid:123456789012"));
    }
}
