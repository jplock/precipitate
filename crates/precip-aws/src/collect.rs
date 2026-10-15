//! The shape every AWS collector shares. A collector is one service's
//! describe calls for one region. The region scanner runs each collector,
//! records failures by API name, logs what came back, and merges it.

use aws_config::SdkConfig;
use aws_smithy_async::future::pagination_stream::PaginationStream;
use precip_types::scan::{Edge, Relationship, Resource, ResourceKind};
use precip_types::{Arn, Body, Region};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::accounts::CallerIdentity;
use crate::arn::ArnType;
use crate::{autoscaling, cloudformation, ec2, elb, managed, tagging, vpc};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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

#[derive(Debug, Default)]
pub(crate) struct Collected {
    pub resources: Vec<Resource>,
    pub relationships: Vec<Relationship>,
    /// Tags to merge into resources collected earlier, by ARN. Existing
    /// keys are kept; missing ones are added.
    pub tag_updates: Vec<(Arn, BTreeMap<String, String>)>,
}

impl Collected {
    /// Fold another collector's output into this one.
    pub(crate) fn absorb(&mut self, other: Collected) {
        if !other.tag_updates.is_empty() {
            let by_arn: HashMap<Arn, usize> = self
                .resources
                .iter()
                .enumerate()
                .map(|(i, r)| (r.arn.clone(), i))
                .collect();
            for (arn, tags) in other.tag_updates {
                if let Some(&i) = by_arn.get(&arn) {
                    for (k, v) in tags {
                        self.resources[i].tags.entry(k).or_insert(v);
                    }
                }
            }
        }
        self.resources.extend(other.resources);
        self.relationships.extend(other.relationships);
    }

    /// Resources of one kind collected so far.
    pub(crate) fn of_kind(
        &self,
        kind: ResourceKind,
    ) -> impl Iterator<Item = &Resource> {
        self.resources.iter().filter(move |r| r.kind() == kind)
    }

    /// The distinct values `field` picks out of the bodies collected so
    /// far, sorted.
    pub(crate) fn distinct<'a, I>(
        &'a self,
        field: impl Fn(&'a Body) -> I,
    ) -> Vec<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut v: Vec<String> = self
            .resources
            .iter()
            .flat_map(|r| field(&r.body))
            .map(str::to_string)
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Add an edge between two resources.
    pub(crate) fn relate(&mut self, from: Arn, edge: Edge, to: Arn) {
        self.relationships.push(Relationship {
            from_arn: from,
            edge,
            to_arn: to,
        });
    }
}

/// An SDK string as an owned body field.
pub(crate) fn own(s: Option<&str>) -> Option<String> {
    s.map(str::to_string)
}

/// SDK strings as an owned body list.
pub(crate) fn owned<'a>(
    items: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    items.into_iter().map(str::to_string).collect()
}

/// One failed API call, named so the report can say which permission or
/// service was the problem.
#[derive(Debug, thiserror::Error)]
#[error("{api}")]
pub(crate) struct CollectError {
    pub api: &'static str,
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl CollectError {
    pub(crate) fn new<E>(api: &'static str) -> impl FnOnce(E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        move |e| Self { api, source: Box::new(e) }
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

/// When a collector runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Concurrently with the other first-stage collectors.
    First,
    /// After the first stage, one at a time in [`all`] order, reading
    /// everything collected before it.
    Second,
}

/// A collector as it is written: a describe call for one region.
pub(crate) trait Collect: Send + Sync + 'static {
    /// The API the collector calls, used in logs and partial-scan reports.
    const API: &'static str;
    const STAGE: Stage = Stage::First;

    /// `prior` holds everything collectors earlier in [`all`] produced for
    /// this region, so a second-stage collector can describe only what is
    /// referenced (images in use, instance types in use).
    fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> impl Future<Output = Result<Collected, CollectError>> + Send;
}

/// A collector as the region scanner holds it.
pub(crate) trait Collector: Send + Sync {
    fn name(&self) -> &'static str;
    fn stage(&self) -> Stage;
    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>>;
}

impl<T: Collect> Collector for T {
    fn name(&self) -> &'static str {
        T::API
    }

    fn stage(&self) -> Stage {
        T::STAGE
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(Collect::collect(self, ctx, prior))
    }
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

/// Every collector. Second-stage collectors run after the rest, in this
/// order.
pub(crate) fn all() -> Vec<Arc<dyn Collector>> {
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
