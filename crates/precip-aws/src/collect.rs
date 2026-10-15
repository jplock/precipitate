//! What AWS collectors share beyond the common runner: the region
//! context, SDK pagination and SDK tags.

use aws_config::SdkConfig;
use aws_smithy_async::future::pagination_stream::PaginationStream;
use precip_collect::Collector;
pub(crate) use precip_collect::{
    Collect, CollectError, Collected, Stage, own, owned,
};
#[cfg(test)]
use precip_types::AccountId;
use precip_types::scan::Resource;
use precip_types::{Arn, Body, Region};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::accounts::CallerIdentity;
use crate::arn::ArnType;
use crate::{autoscaling, cloudformation, ec2, elb, managed, tagging, vpc};

/// Everything a collector needs for one region.
pub(crate) struct RegionContext {
    /// SDK config already pinned to `region`.
    pub config: SdkConfig,
    pub identity: CallerIdentity,
    pub region: Region,
}

impl RegionContext {
    /// The ARN of a resource of type `ty` with this id, in this region and
    /// account.
    pub(crate) fn arn(&self, ty: ArnType, id: &str) -> Arn {
        ty.arn(
            &self.identity.partition,
            &self.region,
            &self.identity.account_id,
            id,
        )
    }

    /// A reference to the resource of type `ty` with id `id`, as its ARN.
    pub(crate) fn refer(
        &self,
        ty: ArnType,
        id: Option<&str>,
    ) -> Option<String> {
        id.map(|i| self.arn(ty, i).to_string())
    }

    /// References to resources of type `ty`, as their ARNs.
    pub(crate) fn refer_all<'a>(
        &self,
        ty: ArnType,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> Vec<String> {
        ids.into_iter().map(|i| self.arn(ty, i).to_string()).collect()
    }

    /// A context for account 111111111111 in us-east-1, for tests that
    /// build bodies without calling AWS.
    #[cfg(test)]
    pub(crate) fn test() -> Self {
        Self {
            config: SdkConfig::builder().build(),
            identity: CallerIdentity {
                account_id: AccountId::from("111111111111"),
                arn: Arn::from("arn:aws:iam::111111111111:root"),
                partition: "aws".into(),
            },
            region: Region::from("us-east-1"),
        }
    }

    /// A resource in this region with the common fields filled.
    pub(crate) fn resource(
        &self,
        arn: Arn,
        tags: BTreeMap<String, String>,
        body: Body,
    ) -> Resource {
        Resource {
            arn,
            account_id: self.identity.account_id.clone(),
            region: self.region.clone(),
            tags,
            body,
        }
    }
}

/// The next page of a paginated call, with a failure named after `api`.
pub(crate) async fn next_page<P, E>(
    api: &'static str,
    pages: &mut PaginationStream<Result<P, E>>,
) -> Result<Option<P>, CollectError>
where
    E: std::error::Error + Send + Sync + 'static,
{
    pages.next().await.transpose().map_err(CollectError::new(api))
}

/// A tag as one of the SDKs models it.
pub(crate) trait SdkTag {
    fn tag_key(&self) -> Option<&str>;
    fn tag_value(&self) -> Option<&str>;
}

macro_rules! sdk_tag {
    (optional: $($ty:ty),+) => {
        $(impl SdkTag for $ty {
            fn tag_key(&self) -> Option<&str> {
                self.key()
            }
            fn tag_value(&self) -> Option<&str> {
                self.value()
            }
        })+
    };
    (required: $($ty:ty),+) => {
        $(impl SdkTag for $ty {
            fn tag_key(&self) -> Option<&str> {
                Some(self.key())
            }
            fn tag_value(&self) -> Option<&str> {
                Some(self.value())
            }
        })+
    };
}

sdk_tag!(optional:
    aws_sdk_ec2::types::Tag,
    aws_sdk_autoscaling::types::TagDescription,
    aws_sdk_cloudformation::types::Tag,
    aws_sdk_rds::types::Tag
);
sdk_tag!(required:
    aws_sdk_efs::types::Tag,
    aws_sdk_resourcegroupstagging::types::Tag
);

/// A tag map from SDK tags. Tags with a missing key or value are dropped.
pub(crate) fn tags<'a, T: SdkTag + 'a>(
    tags: impl IntoIterator<Item = &'a T>,
) -> BTreeMap<String, String> {
    tags.into_iter()
        .filter_map(|t| {
            Some((t.tag_key()?.to_string(), t.tag_value()?.to_string()))
        })
        .collect()
}

/// EC2 names most resources by their `Name` tag.
pub(crate) fn name_tag(tags: &BTreeMap<String, String>) -> Option<String> {
    tags.get("Name").filter(|n| !n.trim().is_empty()).cloned()
}

/// Every collector. Second-stage collectors run after the rest, in this
/// order.
pub(crate) fn all() -> Vec<Arc<dyn Collector<RegionContext>>> {
    vec![
        Arc::new(ec2::Instances),
        Arc::new(ec2::Volumes),
        Arc::new(ec2::Addresses),
        Arc::new(vpc::Vpcs),
        Arc::new(vpc::Subnets),
        Arc::new(vpc::SecurityGroups),
        Arc::new(vpc::SecurityGroupRules),
        Arc::new(vpc::RouteTables),
        Arc::new(vpc::InternetGateways),
        Arc::new(vpc::NatGateways),
        Arc::new(vpc::PeeringConnections),
        Arc::new(autoscaling::Groups),
        Arc::new(elb::LoadBalancers),
        Arc::new(elb::TargetGroups),
        Arc::new(cloudformation::Stacks),
        Arc::new(managed::Rds),
        Arc::new(managed::Eks),
        Arc::new(managed::Ecs),
        Arc::new(managed::Lambda),
        Arc::new(managed::ElastiCache),
        Arc::new(managed::DynamoDb),
        Arc::new(managed::Efs),
        // Second stage, in order: each reads what ran before it.
        Arc::new(tagging::Tags),
        Arc::new(cloudformation::StackResources),
        Arc::new(autoscaling::LaunchTemplates),
        Arc::new(autoscaling::LaunchConfigurations),
        Arc::new(ec2::Images),
        Arc::new(ec2::InstanceTypes),
        Arc::new(vpc::PrefixLists),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_drop_entries_missing_a_key_or_value() {
        let t = |k: Option<&str>, v: Option<&str>| {
            aws_sdk_ec2::types::Tag::builder()
                .set_key(k.map(Into::into))
                .set_value(v.map(Into::into))
                .build()
        };
        let got = tags(&[
            t(Some("Name"), Some("web")),
            t(None, Some("x")),
            t(Some("y"), None),
        ]);
        assert_eq!(
            got,
            BTreeMap::from([("Name".to_string(), "web".to_string())])
        );
    }

    #[test]
    fn second_stage_collectors_come_last() {
        let stages: Vec<Stage> = all().iter().map(|c| c.stage()).collect();
        let first_second =
            stages.iter().position(|s| *s == Stage::Second).unwrap();
        assert!(stages[first_second..].iter().all(|s| *s == Stage::Second));
    }
}
