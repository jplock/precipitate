//! A scan is one read-only pass over a set of AWS accounts and regions.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One scan of an AWS estate. Scans are appended to the store and never
/// overwritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scan {
    pub id: String,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    /// ARN of the identity the scan ran as.
    pub caller_arn: String,
    pub targets: Vec<ScanTarget>,
    pub resources: Vec<Resource>,
    pub relationships: Vec<Relationship>,
}

/// One account and region pair the scan attempted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanTarget {
    pub account_id: String,
    pub region: String,
    pub status: TargetStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "error")]
pub enum TargetStatus {
    Scanned,
    /// Skipped because Resource Explorer reported nothing in the region.
    Empty,
    Partial(String),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    /// An AWS account in the Organization, with its OU. Region is `global`.
    Account,
    Ec2Instance,
    /// vCPU and memory for an instance type seen in the region. Synthetic
    /// ARN `arn:<partition>:ec2:<region>::instance-type/<name>`.
    InstanceType,
    EbsVolume,
    Ami,
    Vpc,
    Subnet,
    SecurityGroup,
    SecurityGroupRule,
    RouteTable,
    InternetGateway,
    NatGateway,
    VpcPeeringConnection,
    /// A managed prefix list with its entries, so security group rules that
    /// reference one can be expanded.
    PrefixList,
    AutoScalingGroup,
    /// A launch template version, carrying the instance type an ASG
    /// launches when it has no mixed-instance overrides.
    LaunchTemplate,
    /// A launch configuration, the pre-template way an ASG names its
    /// instance type. Still common in older estates.
    LaunchConfiguration,
    ElasticIp,
    LoadBalancer,
    TargetGroup,
    CloudFormationStack,
    /// A resource of a managed service with no Oxide equivalent. The
    /// service name is in `body.service`. Counted, never sized.
    ManagedService,
}

/// A resource as collected, keyed by ARN. The collected fields live in
/// `body` so the schema can grow without a migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub arn: String,
    pub kind: ResourceKind,
    pub account_id: String,
    pub region: String,
    pub tags: BTreeMap<String, String>,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    AttachedTo,
    MemberOfAsg,
    InStack,
    TargetOf,
    InSubnet,
    HasSecurityGroup,
    /// Security group rule to the group it belongs to.
    RuleOf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub from_arn: String,
    pub edge: Edge,
    pub to_arn: String,
}
