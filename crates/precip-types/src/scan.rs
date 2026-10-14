//! A scan is one read-only pass over a set of AWS accounts and regions.

use crate::{AccountId, Arn, Region};
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
    pub caller_arn: Arn,
    pub targets: Vec<ScanTarget>,
    pub resources: Vec<Resource>,
    pub relationships: Vec<Relationship>,
}

/// One account and region pair the scan attempted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanTarget {
    pub account_id: AccountId,
    pub region: Region,
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

impl TargetStatus {
    /// The name stored in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scanned => "scanned",
            Self::Empty => "empty",
            Self::Partial(_) => "partial",
            Self::Failed(_) => "failed",
        }
    }

    /// What went wrong, for partial and failed targets.
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Scanned | Self::Empty => None,
            Self::Partial(e) | Self::Failed(e) => Some(e),
        }
    }
}

crate::named_enum! {
pub enum ResourceKind {
    /// An AWS account in the Organization, with its OU. Region is `global`.
    Account = "account",
    Ec2Instance = "ec2_instance",
    /// vCPU and memory for an instance type seen in the region. Synthetic
    /// ARN `arn:<partition>:ec2:<region>::instance-type/<name>`.
    InstanceType = "instance_type",
    EbsVolume = "ebs_volume",
    Ami = "ami",
    Vpc = "vpc",
    Subnet = "subnet",
    SecurityGroup = "security_group",
    SecurityGroupRule = "security_group_rule",
    RouteTable = "route_table",
    InternetGateway = "internet_gateway",
    NatGateway = "nat_gateway",
    VpcPeeringConnection = "vpc_peering_connection",
    /// A managed prefix list with its entries, so security group rules that
    /// reference one can be expanded.
    PrefixList = "prefix_list",
    AutoScalingGroup = "auto_scaling_group",
    /// A launch template version, carrying the instance type an ASG
    /// launches when it has no mixed-instance overrides.
    LaunchTemplate = "launch_template",
    /// A launch configuration, the pre-template way an ASG names its
    /// instance type. Still common in older estates.
    LaunchConfiguration = "launch_configuration",
    ElasticIp = "elastic_ip",
    LoadBalancer = "load_balancer",
    TargetGroup = "target_group",
    CloudFormationStack = "cloud_formation_stack",
    /// A resource of a managed service with no Oxide equivalent. The
    /// service name is in `body.service`. Counted, never sized.
    ManagedService = "managed_service",
}
}

/// A resource as collected, keyed by ARN. The collected fields live in
/// `body` so the schema can grow without a migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub arn: Arn,
    pub kind: ResourceKind,
    pub account_id: AccountId,
    pub region: Region,
    pub tags: BTreeMap<String, String>,
    pub body: serde_json::Value,
}

crate::named_enum! {
pub enum Edge {
    AttachedTo = "attached_to",
    MemberOfAsg = "member_of_asg",
    InStack = "in_stack",
    TargetOf = "target_of",
    InSubnet = "in_subnet",
    HasSecurityGroup = "has_security_group",
    /// Security group rule to the group it belongs to.
    RuleOf = "rule_of",
}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub from_arn: Arn,
    pub edge: Edge,
    pub to_arn: Arn,
}
