//! The shape every AWS collector shares. A collector is one service's
//! describe calls for one region. The region scanner runs each collector,
//! records failures by API name, logs what came back, and merges it.

use aws_config::SdkConfig;
use aws_smithy_async::future::pagination_stream::PaginationStream;
use precip_types::scan::{Relationship, Resource, ResourceKind};
use precip_types::{Arn, Region};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::accounts::CallerIdentity;
use crate::arn::ArnType;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything a collector needs for one region.
pub struct RegionContext {
    /// SDK config already pinned to `region`.
    pub config: SdkConfig,
    pub identity: CallerIdentity,
    pub region: Region,
}

impl RegionContext {
    /// The ARN of a resource of type `ty` with this id, in this region and
    /// account.
    pub fn arn(&self, ty: ArnType, id: &str) -> Arn {
        ty.arn(
            &self.identity.partition,
            &self.region,
            &self.identity.account_id,
            id,
        )
    }

    /// A resource in this region with the common fields filled.
    pub fn resource(
        &self,
        arn: Arn,
        kind: ResourceKind,
        tags: BTreeMap<String, String>,
        body: serde_json::Value,
    ) -> Resource {
        Resource {
            arn,
            kind,
            account_id: self.identity.account_id.clone(),
            region: self.region.clone(),
            tags,
            body,
        }
    }
}

#[derive(Debug, Default)]
pub struct Collected {
    pub resources: Vec<Resource>,
    pub relationships: Vec<Relationship>,
    /// Tags to merge into resources collected earlier, by ARN. Existing
    /// keys are kept; missing ones are added.
    pub tag_updates: Vec<(Arn, BTreeMap<String, String>)>,
}

impl Collected {
    /// Fold another collector's output into this one.
    pub fn absorb(&mut self, other: Collected) {
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
    pub fn of_kind(
        &self,
        kind: ResourceKind,
    ) -> impl Iterator<Item = &Resource> {
        self.resources.iter().filter(move |r| r.kind == kind)
    }

    /// Distinct string values of `field` across resources of `kind`.
    pub fn distinct_field(
        &self,
        kind: ResourceKind,
        field: &str,
    ) -> Vec<String> {
        let mut v: Vec<String> = self
            .of_kind(kind)
            .filter_map(|r| r.body_str(field))
            .map(str::to_string)
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Add an edge between two resources.
    pub fn relate(
        &mut self,
        from: Arn,
        edge: precip_types::scan::Edge,
        to: Arn,
    ) {
        self.relationships.push(Relationship {
            from_arn: from,
            edge,
            to_arn: to,
        });
    }
}

/// One failed API call, named so the report can say which permission or
/// service was the problem.
#[derive(Debug, thiserror::Error)]
#[error("{api}")]
pub struct CollectError {
    pub api: &'static str,
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

impl CollectError {
    pub fn new<E>(api: &'static str) -> impl FnOnce(E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        move |e| Self { api, source: Box::new(e) }
    }
}

/// The next page of a paginated call, with a failure named after `api`.
pub async fn next_page<P, E>(
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
pub enum Stage {
    /// Concurrently with the other first-stage collectors.
    First,
    /// After the first stage, one at a time in [`all`] order, reading
    /// everything collected before it.
    Second,
}

/// A collector as it is written: a describe call for one region.
pub trait Collect: Send + Sync + 'static {
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
pub trait Collector: Send + Sync {
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
pub trait SdkTag {
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
pub fn tags<'a, T: SdkTag + 'a>(
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
pub fn all() -> Vec<Arc<dyn Collector>> {
    vec![
        Arc::new(crate::ec2::Instances),
        Arc::new(crate::ec2::Volumes),
        Arc::new(crate::ec2::Addresses),
        Arc::new(crate::vpc::Vpcs),
        Arc::new(crate::vpc::Subnets),
        Arc::new(crate::vpc::SecurityGroups),
        Arc::new(crate::vpc::SecurityGroupRules),
        Arc::new(crate::vpc::RouteTables),
        Arc::new(crate::vpc::InternetGateways),
        Arc::new(crate::vpc::NatGateways),
        Arc::new(crate::vpc::PeeringConnections),
        Arc::new(crate::autoscaling::Groups),
        Arc::new(crate::elb::LoadBalancers),
        Arc::new(crate::elb::TargetGroups),
        Arc::new(crate::cloudformation::Stacks),
        Arc::new(crate::managed::Rds),
        Arc::new(crate::managed::Eks),
        Arc::new(crate::managed::Ecs),
        Arc::new(crate::managed::Lambda),
        Arc::new(crate::managed::ElastiCache),
        Arc::new(crate::managed::DynamoDb),
        Arc::new(crate::managed::Efs),
        // Second stage, in order: each reads what ran before it.
        Arc::new(crate::tagging::Tags),
        Arc::new(crate::cloudformation::StackResources),
        Arc::new(crate::autoscaling::LaunchTemplates),
        Arc::new(crate::autoscaling::LaunchConfigurations),
        Arc::new(crate::ec2::Images),
        Arc::new(crate::ec2::InstanceTypes),
        Arc::new(crate::vpc::PrefixLists),
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
