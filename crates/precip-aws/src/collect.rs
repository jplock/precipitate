//! The shape every AWS collector shares. A collector is one service's
//! describe calls for one region. The region scanner runs each collector,
//! records failures by API name, and merges what came back.

use aws_config::SdkConfig;
use precip_types::scan::{Relationship, Resource};
use std::future::Future;
use std::pin::Pin;

use crate::accounts::CallerIdentity;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything a collector needs for one region.
pub struct RegionContext {
    /// SDK config already pinned to `region`.
    pub config: SdkConfig,
    pub identity: CallerIdentity,
    pub region: String,
}

impl RegionContext {
    /// ARN for a resource of `service` in this region and account.
    pub fn arn(&self, service: &str, resource: &str) -> String {
        crate::accounts::resource_arn(
            &self.identity.partition,
            service,
            &self.region,
            &self.identity.account_id,
            resource,
        )
    }
}

#[derive(Debug, Default)]
pub struct Collected {
    pub resources: Vec<Resource>,
    pub relationships: Vec<Relationship>,
    /// Tags to merge into resources collected earlier, by ARN. Existing
    /// keys are kept; missing ones are added.
    pub tag_updates: Vec<(String, std::collections::BTreeMap<String, String>)>,
}

impl Collected {
    /// Fold another collector's output into this one.
    pub fn absorb(&mut self, other: Collected) {
        if !other.tag_updates.is_empty() {
            let by_arn: std::collections::HashMap<String, usize> = self
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

pub trait Collector: Send + Sync {
    /// Short name used in logs and partial-scan reports.
    fn name(&self) -> &'static str;

    /// Whether this collector reads `prior`. Collectors that do not are run
    /// concurrently in a first stage; those that do run in a second stage
    /// once the first has finished.
    fn needs_prior(&self) -> bool {
        false
    }

    /// `prior` holds everything collectors earlier in [`all`] produced for
    /// this region, so a collector can describe only what is referenced
    /// (images in use, instance types in use).
    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>>;
}

impl Collected {
    /// Resources of one kind collected so far.
    pub fn of_kind(
        &self,
        kind: precip_types::ResourceKind,
    ) -> impl Iterator<Item = &Resource> {
        self.resources.iter().filter(move |r| r.kind == kind)
    }

    /// Distinct string values of `field` across resources of `kind`.
    pub fn distinct_field(
        &self,
        kind: precip_types::ResourceKind,
        field: &str,
    ) -> Vec<String> {
        let mut v: Vec<String> = self
            .of_kind(kind)
            .filter_map(|r| r.body.get(field).and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// Build a tag map from any SDK tag type that exposes `key()` and
/// `value()`. Tags with a missing key or value are dropped.
pub fn tags<'a, T, K, V>(
    tags: impl IntoIterator<Item = &'a T>,
    key: K,
    value: V,
) -> std::collections::BTreeMap<String, String>
where
    T: 'a,
    K: Fn(&T) -> Option<&str>,
    V: Fn(&T) -> Option<&str>,
{
    tags.into_iter()
        .filter_map(|t| Some((key(t)?.to_string(), value(t)?.to_string())))
        .collect()
}

/// Every collector. Those with `needs_prior` run after the rest.
pub fn all() -> Vec<std::sync::Arc<dyn Collector>> {
    vec![
        std::sync::Arc::new(crate::ec2::Instances),
        std::sync::Arc::new(crate::ec2::Volumes),
        std::sync::Arc::new(crate::ec2::Addresses),
        std::sync::Arc::new(crate::vpc::Vpcs),
        std::sync::Arc::new(crate::vpc::Subnets),
        std::sync::Arc::new(crate::vpc::SecurityGroups),
        std::sync::Arc::new(crate::vpc::SecurityGroupRules),
        std::sync::Arc::new(crate::vpc::RouteTables),
        std::sync::Arc::new(crate::vpc::InternetGateways),
        std::sync::Arc::new(crate::vpc::NatGateways),
        std::sync::Arc::new(crate::vpc::PeeringConnections),
        std::sync::Arc::new(crate::autoscaling::Groups),
        std::sync::Arc::new(crate::elb::LoadBalancers),
        std::sync::Arc::new(crate::elb::TargetGroups),
        std::sync::Arc::new(crate::cloudformation::Stacks),
        std::sync::Arc::new(crate::managed::Rds),
        std::sync::Arc::new(crate::managed::Eks),
        std::sync::Arc::new(crate::managed::Ecs),
        std::sync::Arc::new(crate::managed::Lambda),
        std::sync::Arc::new(crate::managed::ElastiCache),
        std::sync::Arc::new(crate::managed::DynamoDb),
        std::sync::Arc::new(crate::managed::Efs),
        // Second stage, in order: each reads what ran before it.
        std::sync::Arc::new(crate::tagging::Tags),
        std::sync::Arc::new(crate::cloudformation::StackResources),
        std::sync::Arc::new(crate::autoscaling::LaunchTemplates),
        std::sync::Arc::new(crate::autoscaling::LaunchConfigurations),
        std::sync::Arc::new(crate::ec2::Images),
        std::sync::Arc::new(crate::ec2::InstanceTypes),
        std::sync::Arc::new(crate::vpc::PrefixLists),
    ]
}

/// Build a resource in this region with the common fields filled.
impl RegionContext {
    pub fn resource(
        &self,
        arn: String,
        kind: precip_types::ResourceKind,
        tags: std::collections::BTreeMap<String, String>,
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
