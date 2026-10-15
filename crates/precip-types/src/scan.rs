//! A scan is one read-only pass over a set of AWS accounts and regions.

use crate::body::Body;
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

/// Which way a relationship is followed: from the resource to the edge's
/// target, or from the edge's source to the resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

/// How a resource takes its workload from the resources it is related to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inheritance {
    pub edge: Edge,
    pub direction: Direction,
}

impl ResourceKind {
    /// The relationship a resource of this kind follows to the resources
    /// whose workload it joins, for kinds that have no placement of their
    /// own. Load balancers follow target groups, which follow instances.
    pub fn inherits_via(self) -> Option<Inheritance> {
        let (edge, direction) = match self {
            Self::EbsVolume | Self::ElasticIp => {
                (Edge::AttachedTo, Direction::Out)
            }
            Self::AutoScalingGroup => (Edge::MemberOfAsg, Direction::In),
            Self::TargetGroup => (Edge::TargetOf, Direction::In),
            Self::SecurityGroup => (Edge::HasSecurityGroup, Direction::In),
            Self::LoadBalancer => (Edge::AttachedTo, Direction::In),
            Self::Account
            | Self::Ec2Instance
            | Self::InstanceType
            | Self::Ami
            | Self::Vpc
            | Self::Subnet
            | Self::SecurityGroupRule
            | Self::RouteTable
            | Self::InternetGateway
            | Self::NatGateway
            | Self::VpcPeeringConnection
            | Self::PrefixList
            | Self::LaunchTemplate
            | Self::LaunchConfiguration
            | Self::CloudFormationStack
            | Self::ManagedService => return None,
        };
        Some(Inheritance { edge, direction })
    }
}

/// A resource as collected, keyed by ARN. Its kind is the kind of its
/// body, so the two cannot disagree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub arn: Arn,
    pub account_id: AccountId,
    pub region: Region,
    pub tags: BTreeMap<String, String>,
    #[serde(flatten)]
    pub body: Body,
}

impl Resource {
    pub fn kind(&self) -> ResourceKind {
        self.body.kind()
    }

    /// The resource's own name, for kinds that have one.
    pub fn name(&self) -> Option<&str> {
        self.body.name()
    }

    /// The value of the first of `keys` tagged on the resource, trimmed.
    /// Blank values count as missing.
    pub fn first_tag(&self, keys: &[impl AsRef<str>]) -> Option<&str> {
        keys.iter()
            .find_map(|k| self.tags.get(k.as_ref()))
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body;

    fn resource(body: Body) -> Resource {
        Resource {
            arn: Arn::from("arn:x"),
            account_id: AccountId::from("1"),
            region: Region::from("us-east-1"),
            tags: [
                ("Blank".to_string(), "  ".to_string()),
                ("App".to_string(), " shop ".to_string()),
            ]
            .into(),
            body,
        }
    }

    #[test]
    fn tag_lookup_takes_the_first_present_key() {
        let r = resource(Body::Vpc(body::Vpc::default()));
        assert_eq!(r.first_tag(&["Missing", "App"]), Some("shop"));
        assert_eq!(
            r.first_tag(&["Blank", "App"]),
            None,
            "the first present tag decides, and blank is missing"
        );
    }

    #[test]
    fn kind_and_name_come_from_the_body() {
        let r = resource(Body::AutoScalingGroup(body::AutoScalingGroup {
            name: Some("web".into()),
            ..Default::default()
        }));
        assert_eq!(r.kind(), ResourceKind::AutoScalingGroup);
        assert_eq!(r.name(), Some("web"));
        assert_eq!(resource(Body::Vpc(body::Vpc::default())).name(), None);
    }

    #[test]
    fn resources_serialize_with_kind_and_body_side_by_side() {
        let r = resource(Body::ManagedService(body::Managed {
            name: Some("orders".into()),
            service: body::Service::DynamoDb,
        }));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "managed_service");
        assert_eq!(
            v["body"],
            serde_json::json!({"name": "orders", "service": "dynamodb"})
        );
        let back: Resource = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn stored_bodies_round_trip_and_tolerate_missing_fields() {
        let stored =
            serde_json::json!({"instance_id": "i-1", "state": "running"});
        let b = Body::from_value(ResourceKind::Ec2Instance, stored).unwrap();
        let Body::Ec2Instance(i) = &b else { panic!("wrong kind") };
        assert_eq!(i.state.as_deref(), Some("running"));
        assert_eq!(i.network_interface_count, 0, "missing fields default");
        assert_eq!(b.to_value().unwrap()["instance_id"], "i-1");
        assert!(
            Body::from_value(
                ResourceKind::ManagedService,
                serde_json::json!({"name": "x"})
            )
            .is_err(),
            "a managed service must name its service"
        );
    }

    /// Stored bodies and content hashes were written from JSON literals
    /// before bodies were typed; a typed body must store the same object
    /// or every resource would get a new version on the next scan.
    #[test]
    fn typed_bodies_store_the_same_json_as_before() {
        let instance = Body::Ec2Instance(body::Instance {
            instance_id: "i-1".into(),
            instance_type: Some("m5.large".into()),
            vpc_id: Some("vpc-1".into()),
            ebs_volume_count: 2,
            ..Default::default()
        });
        assert_eq!(
            instance.to_value().unwrap(),
            serde_json::json!({
                "instance_id": "i-1",
                "instance_type": "m5.large",
                "architecture": null,
                "platform": null,
                "state": null,
                "image_id": null,
                "subnet_id": null,
                "vpc_id": "vpc-1",
                "boot_mode": null,
                "current_boot_mode": null,
                "launch_time": null,
                "ebs_volume_count": 2,
                "instance_store_volume_count": 0,
                "network_interface_count": 0,
                "public_ip": null,
            })
        );
        let lb = Body::LoadBalancer(body::LoadBalancer {
            name: Some("web".into()),
            lb_type: Some("application".into()),
            ..Default::default()
        });
        assert_eq!(
            lb.to_value().unwrap(),
            serde_json::json!({
                "name": "web",
                "type": "application",
                "scheme": null,
                "vpc_id": null,
                "dns_name": null,
                "security_groups": [],
            })
        );
        let rds = Body::ManagedService(body::Managed {
            name: Some("orders".into()),
            service: body::Service::Rds {
                engine: Some("postgres".into()),
                engine_version: None,
                instance_class: None,
                allocated_storage_gib: Some(100),
                multi_az: Some(false),
                vpc_id: None,
            },
        });
        assert_eq!(
            rds.to_value().unwrap(),
            serde_json::json!({
                "service": "rds",
                "name": "orders",
                "engine": "postgres",
                "engine_version": null,
                "instance_class": null,
                "allocated_storage_gib": 100,
                "multi_az": false,
                "vpc_id": null,
            })
        );
    }

    #[test]
    fn every_kind_has_a_body_with_the_same_name() {
        for kind in ResourceKind::ALL {
            let value = if *kind == ResourceKind::ManagedService {
                serde_json::json!({"service": "eks"})
            } else {
                serde_json::json!({})
            };
            let b = Body::from_value(*kind, value).unwrap();
            assert_eq!(b.kind(), *kind);
        }
    }

    #[test]
    fn only_attached_kinds_inherit() {
        let via =
            |k: ResourceKind| k.inherits_via().map(|i| (i.edge, i.direction));
        assert_eq!(
            via(ResourceKind::EbsVolume),
            Some((Edge::AttachedTo, Direction::Out))
        );
        assert_eq!(
            via(ResourceKind::LoadBalancer),
            Some((Edge::AttachedTo, Direction::In))
        );
        assert_eq!(via(ResourceKind::Ec2Instance), None);
        assert_eq!(via(ResourceKind::ManagedService), None);
        let inheriting = ResourceKind::ALL
            .iter()
            .filter(|k| k.inherits_via().is_some())
            .count();
        assert_eq!(inheriting, 6);
    }
}
