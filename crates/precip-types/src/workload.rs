//! Workloads group resources into the unit that gets a disposition and a
//! size. A workload spans environments, accounts and regions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The signal that placed a resource in its workload. Instances are
/// placed by the first signal that matches, in this order; other
/// resources inherit from the instances they are attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupingSignal {
    /// Manual placement stored in `workload_overrides`.
    Override,
    /// The `precip:workload` tag.
    OverrideTag,
    /// CloudFormation root stack.
    CloudFormationStack,
    AutoScalingGroup,
    LoadBalancer,
    /// A conventional tag such as `Application` or `Service`.
    Tag,
    /// The resource name starts with the name of a known stack or an
    /// already placed workload. Used for managed services, whose list
    /// calls return no tags.
    NamePrefix,
    /// A known workload name appears whole inside the resource name.
    NameContains,
    /// The resource's account hosts exactly one workload, which it joins.
    AccountOnly,
    Vpc,
    Unassigned,
    /// Inherited through a relationship to a placed resource.
    Attached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadMember {
    pub arn: String,
    pub workload: String,
    pub environment: String,
    pub signal: GroupingSignal,
}

/// Manual placement by ARN, from the store's `workload_overrides` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub workload: String,
    pub environment: String,
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
    pub environment_tokens: BTreeMap<String, String>,
    /// Environment used when nothing identifies one.
    pub default_environment: String,
    /// Stack name prefixes that belong to AWS tooling rather than the
    /// customer (Control Tower, StackSets, SAM's bootstrap). Resources in
    /// them are grouped under `platform_workload`.
    pub platform_stack_prefixes: Vec<String>,
    pub platform_workload: String,
    /// Trailing name segments that name a layer of a workload rather than
    /// the workload itself: `shop-vpc`, `shop-app` and `shop-db` are all
    /// `shop`. One trailing token is stripped when something remains.
    pub layer_tokens: Vec<String>,
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
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            default_environment: "default".to_string(),
            platform_stack_prefixes: [
                "StackSet-",
                "AWSControlTower",
                "aws-sam-cli-managed",
                "aws-quicksetup",
            ]
            .map(str::to_string)
            .to_vec(),
            platform_workload: "aws-platform".to_string(),
            layer_tokens: [
                "vpc", "network", "net", "app", "api", "web", "ui", "db",
                "database", "data", "infra", "iam", "dns", "cdn", "storage",
                "queue", "cache", "worker", "batch", "cluster", "stack",
            ]
            .map(str::to_string)
            .to_vec(),
        }
    }
}
