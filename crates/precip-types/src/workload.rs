//! Workloads group resources into the unit that gets a disposition and a
//! size. A workload spans environments, accounts and regions.

use crate::{Arn, EnvName, PairKey, WorkloadName};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

crate::named_enum! {
/// The signal that placed a resource in its workload. Instances are
/// placed by the first signal that matches, in this order; other
/// resources inherit from the instances they are attached to.
pub enum GroupingSignal {
    /// Manual placement stored in `workload_overrides`.
    Override = "override",
    /// The `precip:workload` tag.
    OverrideTag = "override_tag",
    /// The root CloudFormation stack, or the Azure resource group.
    DeploymentGroup = "deployment_group",
    /// An Auto Scaling group or VM scale set.
    ScaleGroup = "scale_group",
    LoadBalancer = "load_balancer",
    /// A conventional tag such as `Application` or `Service`.
    Tag = "tag",
    /// The resource name starts with the name of a known stack or an
    /// already placed workload. Used for managed services, whose list
    /// calls return no tags.
    NamePrefix = "name_prefix",
    /// A known workload name appears whole inside the resource name.
    NameContains = "name_contains",
    /// The resource's account hosts exactly one workload, which it joins.
    AccountOnly = "account_only",
    /// The VPC or virtual network, by its `Name` tag or id.
    Network = "network",
    Unassigned = "unassigned",
    /// Inherited through a relationship to a placed resource.
    Attached = "attached",
}
}

impl GroupingSignal {
    /// Whether the resource joined its workload by matching names rather
    /// than by a relationship or tag.
    pub fn is_name_match(self) -> bool {
        match self {
            Self::NamePrefix | Self::NameContains => true,
            Self::Override
            | Self::OverrideTag
            | Self::DeploymentGroup
            | Self::ScaleGroup
            | Self::LoadBalancer
            | Self::Tag
            | Self::AccountOnly
            | Self::Network
            | Self::Unassigned
            | Self::Attached => false,
        }
    }
}

/// The workloads a name passes through when folds are followed, starting
/// with the name itself: `a` folded into `b` folded into `c` gives
/// `[a, b, c]`. A loop in the folds stops at the first repeat.
pub fn fold_chain(
    aliases: &BTreeMap<WorkloadName, WorkloadName>,
    name: &WorkloadName,
) -> Vec<WorkloadName> {
    let mut chain = vec![name.clone()];
    let mut current = name;
    while let Some(next) = aliases.get(current) {
        if chain.contains(next) {
            break;
        }
        chain.push(next.clone());
        current = next;
    }
    chain
}

/// The workload a name ends up in after every fold.
pub fn folded(
    aliases: &BTreeMap<WorkloadName, WorkloadName>,
    name: &WorkloadName,
) -> WorkloadName {
    fold_chain(aliases, name).pop().unwrap_or_else(|| name.clone())
}

/// One resource placed in a workload and environment, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadMember {
    pub arn: Arn,
    #[serde(flatten)]
    pub pair: PairKey,
    pub signal: GroupingSignal,
}

/// Tag keys and name tokens the grouping looks at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupingConfig {
    /// Tag keys that name a workload, in priority order.
    pub workload_tags: Vec<String>,
    /// Tag keys that name an environment, in priority order.
    pub environment_tags: Vec<String>,
    /// Name tokens recognised as an environment when they appear as a
    /// prefix or suffix of a workload, account or OU name, mapped to the
    /// canonical environment name.
    pub environment_tokens: BTreeMap<String, EnvName>,
    /// Environment used when nothing identifies one.
    pub default_environment: EnvName,
    /// Deployment group name prefixes that belong to cloud tooling rather
    /// than the customer: AWS Control Tower, StackSets and SAM's bootstrap
    /// stacks; Azure's Network Watcher, monitoring and Cloud Shell
    /// resource groups. Resources in them are grouped under
    /// `platform_workload`.
    pub platform_group_prefixes: Vec<String>,
    pub platform_workload: WorkloadName,
    /// Trailing name segments that name a layer of a workload rather than
    /// the workload itself: `shop-vpc`, `shop-app` and `shop-db` are all
    /// `shop`. One trailing token is stripped when something remains.
    pub layer_tokens: Vec<String>,
    /// Leading name segments that name the kind of resource rather than
    /// the workload, as Azure's naming convention puts them: `rg-shop` is
    /// `shop`.
    pub kind_prefixes: Vec<String>,
}

impl Default for GroupingConfig {
    fn default() -> Self {
        let tokens = [
            ("prod", "prod"),
            ("production", "prod"),
            ("prd", "prod"),
            ("staging", "staging"),
            ("stage", "staging"),
            ("stg", "staging"),
            ("uat", "uat"),
            ("qa", "qa"),
            ("test", "test"),
            ("dev", "dev"),
            ("development", "dev"),
            ("nonprod", "nonprod"),
            ("sandbox", "sandbox"),
        ];
        Self {
            workload_tags: [
                "Application",
                "application",
                "app",
                "Service",
                "service",
                "Workload",
                "workload",
                "Project",
                "project",
                "elasticbeanstalk:environment-name",
                "eks:cluster-name",
            ]
            .map(str::to_string)
            .to_vec(),
            environment_tags: [
                "Environment",
                "environment",
                "env",
                "Stage",
                "stage",
            ]
            .map(str::to_string)
            .to_vec(),
            environment_tokens: tokens
                .into_iter()
                .map(|(k, v)| (k.to_string(), EnvName::from(v)))
                .collect(),
            default_environment: EnvName::from("default"),
            platform_group_prefixes: [
                "StackSet-",
                "AWSControlTower",
                "aws-sam-cli-managed",
                "aws-quicksetup",
                "NetworkWatcherRG",
                "DefaultResourceGroup-",
                "AzureBackupRG",
                "cloud-shell-storage-",
                "MC_",
            ]
            .map(str::to_string)
            .to_vec(),
            platform_workload: WorkloadName::from("cloud-platform"),
            layer_tokens: [
                "vpc", "vnet", "network", "net", "rg", "app", "api", "web",
                "ui", "db", "database", "data", "infra", "iam", "dns", "cdn",
                "storage", "queue", "cache", "worker", "batch", "cluster",
                "stack",
            ]
            .map(str::to_string)
            .to_vec(),
            kind_prefixes: vec!["rg".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folds(pairs: &[(&str, &str)]) -> BTreeMap<WorkloadName, WorkloadName> {
        pairs
            .iter()
            .map(|(a, b)| (WorkloadName::from(*a), WorkloadName::from(*b)))
            .collect()
    }

    #[test]
    fn folds_are_followed_to_the_end_and_stop_at_a_loop() {
        let name = |s: &str| WorkloadName::from(s);
        let chain = folds(&[("a", "b"), ("b", "c")]);
        assert_eq!(
            fold_chain(&chain, &name("a")),
            [name("a"), name("b"), name("c")]
        );
        assert_eq!(folded(&chain, &name("a")), name("c"));
        assert_eq!(
            folded(&chain, &name("z")),
            name("z"),
            "an unfolded name is itself"
        );
        let looped = folds(&[("a", "b"), ("b", "a")]);
        assert_eq!(fold_chain(&looped, &name("a")), [name("a"), name("b")]);
        assert_eq!(folded(&looped, &name("b")), name("a"));
    }
}
