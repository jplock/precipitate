//! A scan is one read-only pass over a set of cloud accounts and regions.

use crate::body::Body;
use crate::{AccountId, Arn, Region};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

crate::named_enum! {
/// The cloud a scan read.
pub enum Provider {
    Aws = "aws",
    Azure = "azure",
    Gcp = "gcp",
}
}

/// One scan of one provider's estate. Scans are appended to the store and
/// never overwritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scan {
    pub id: String,
    pub provider: Provider,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    /// The identity the scan ran as: an AWS principal ARN, an Azure
    /// principal's sign-in name or id, or a Google account's email.
    pub caller: String,
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
    /// Skipped because the provider's resource index reported nothing in
    /// the region.
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
    /// An AWS account, Azure subscription or Google Cloud project, with
    /// its OU, management group or folder. Region is `global`.
    Account = "account",
    Instance = "instance",
    /// vCPU and memory for a machine size seen in the region, under a
    /// synthetic id the scanner makes up.
    InstanceType = "instance_type",
    Volume = "volume",
    Image = "image",
    Network = "network",
    Subnet = "subnet",
    SecurityGroup = "security_group",
    SecurityGroupRule = "security_group_rule",
    RouteTable = "route_table",
    InternetGateway = "internet_gateway",
    NatGateway = "nat_gateway",
    PeeringConnection = "peering_connection",
    /// A managed prefix list with its entries, so security group rules that
    /// reference one can be expanded.
    PrefixList = "prefix_list",
    ScaleGroup = "scale_group",
    /// An AWS launch template version, carrying the instance type an ASG
    /// launches when it has no mixed-instance overrides.
    LaunchTemplate = "launch_template",
    /// An AWS launch configuration, the pre-template way an ASG names its
    /// instance type. Still common in older estates.
    LaunchConfiguration = "launch_configuration",
    PublicIp = "public_ip",
    LoadBalancer = "load_balancer",
    TargetGroup = "target_group",
    DeploymentGroup = "deployment_group",
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
            Self::Volume | Self::PublicIp => (Edge::AttachedTo, Direction::Out),
            Self::ScaleGroup => (Edge::MemberOfScaleGroup, Direction::In),
            Self::TargetGroup => (Edge::TargetOf, Direction::In),
            Self::SecurityGroup => (Edge::HasSecurityGroup, Direction::In),
            Self::LoadBalancer => (Edge::AttachedTo, Direction::In),
            Self::Account
            | Self::Instance
            | Self::InstanceType
            | Self::Image
            | Self::Network
            | Self::Subnet
            | Self::SecurityGroupRule
            | Self::RouteTable
            | Self::InternetGateway
            | Self::NatGateway
            | Self::PeeringConnection
            | Self::PrefixList
            | Self::LaunchTemplate
            | Self::LaunchConfiguration
            | Self::DeploymentGroup
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
    MemberOfScaleGroup = "member_of_scale_group",
    InDeploymentGroup = "in_deployment_group",
    TargetOf = "target_of",
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
        let r = resource(Body::Network(body::Network::default()));
        assert_eq!(r.first_tag(&["Missing", "App"]), Some("shop"));
        assert_eq!(
            r.first_tag(&["Blank", "App"]),
            None,
            "the first present tag decides, and blank is missing"
        );
    }

    #[test]
    fn kind_and_name_come_from_the_body() {
        let r = resource(Body::ScaleGroup(body::ScaleGroup {
            name: Some("web".into()),
            ..Default::default()
        }));
        assert_eq!(r.kind(), ResourceKind::ScaleGroup);
        assert_eq!(r.name(), Some("web"));
        assert_eq!(
            resource(Body::Network(body::Network::default())).name(),
            None
        );
    }

    #[test]
    fn resources_serialize_with_kind_and_body_side_by_side() {
        let r = resource(Body::ManagedService(body::Managed {
            name: Some("orders".into()),
            service: body::Service::KeyValue { product: "dynamodb".into() },
        }));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "managed_service");
        assert_eq!(
            v["body"],
            serde_json::json!({
                "name": "orders",
                "service": "key_value",
                "product": "dynamodb",
            })
        );
        let back: Resource = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn stored_bodies_round_trip_and_tolerate_missing_fields() {
        let stored = serde_json::json!({"name": "i-1", "state": "running"});
        let b = Body::from_value(ResourceKind::Instance, stored).unwrap();
        let Body::Instance(i) = &b else { panic!("wrong kind") };
        assert_eq!(i.state.as_deref(), Some("running"));
        assert_eq!(i.network_interface_count, 0, "missing fields default");
        assert_eq!(b.to_value().unwrap()["name"], "i-1");
        assert!(
            Body::from_value(
                ResourceKind::ManagedService,
                serde_json::json!({"name": "x"})
            )
            .is_err(),
            "a managed service must name its service"
        );
    }

    /// The store's `json_extract` queries and the content hash read these
    /// keys; a renamed field changes both.
    #[test]
    fn bodies_store_flat_objects_with_their_field_names() {
        let instance = Body::Instance(body::Instance {
            name: Some("i-1".into()),
            instance_type: Some("m5.large".into()),
            network_id: Some("vpc-1".into()),
            volume_count: 2,
            ..Default::default()
        });
        assert_eq!(
            instance.to_value().unwrap(),
            serde_json::json!({
                "name": "i-1",
                "instance_type": "m5.large",
                "architecture": null,
                "platform": null,
                "state": null,
                "image_id": null,
                "subnets": [],
                "network_id": "vpc-1",
                "boot_disk": null,
                "boot_mode": null,
                "launch_time": null,
                "volume_count": 2,
                "local_disk_count": 0,
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
                "network_id": null,
                "dns_name": null,
                "security_groups": [],
            })
        );
        let rds = Body::ManagedService(body::Managed {
            name: Some("orders".into()),
            service: body::Service::Database {
                product: "rds".into(),
                engine: Some("postgres".into()),
                engine_version: None,
                instance_class: None,
                allocated_storage_gib: Some(100),
                high_availability: Some(false),
                network_id: None,
            },
        });
        assert_eq!(
            rds.to_value().unwrap(),
            serde_json::json!({
                "service": "database",
                "product": "rds",
                "name": "orders",
                "engine": "postgres",
                "engine_version": null,
                "instance_class": null,
                "allocated_storage_gib": 100,
                "high_availability": false,
                "network_id": null,
            })
        );
    }

    /// A rule is stored in Oxide's terms, with its peer and protocol
    /// named by kind.
    #[test]
    fn rules_store_direction_action_protocol_and_peer() {
        let rule = Body::SecurityGroupRule(body::SecurityGroupRule {
            group: "sg".into(),
            direction: body::TrafficDirection::Inbound,
            action: body::RuleAction::Deny,
            priority: Some(100),
            protocol: body::Protocol::Tcp,
            ports: Some((22, 22)),
            icmp: None,
            peer: body::Peer::IpNet("10.0.0.0/8".into()),
            description: None,
        });
        let v = rule.to_value().unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "group": "sg",
                "direction": "inbound",
                "action": "deny",
                "priority": 100,
                "protocol": "tcp",
                "ports": [22, 22],
                "icmp": null,
                "peer": {"ip_net": "10.0.0.0/8"},
                "description": null,
            })
        );
        assert_eq!(
            Body::from_value(ResourceKind::SecurityGroupRule, v).unwrap(),
            rule
        );
        let other =
            serde_json::to_value(body::Protocol::Other("gre".into())).unwrap();
        assert_eq!(other, serde_json::json!({"other": "gre"}));
        let route = serde_json::to_value(body::Route {
            destination: Some("0.0.0.0/0".into()),
            target: body::RouteTarget::InternetGateway("igw".into()),
            state: None,
        })
        .unwrap();
        assert_eq!(
            route["target"],
            serde_json::json!({"internet_gateway": "igw"})
        );
    }

    #[test]
    fn protocols_parse_from_names_and_numbers() {
        use body::Protocol;
        for (s, p) in [
            ("-1", Protocol::All),
            ("*", Protocol::All),
            ("TCP", Protocol::Tcp),
            ("6", Protocol::Tcp),
            ("udp", Protocol::Udp),
            ("17", Protocol::Udp),
            ("Icmp", Protocol::Icmp),
            ("1", Protocol::Icmp),
            ("icmpv6", Protocol::Icmp6),
            ("58", Protocol::Icmp6),
            ("esp", Protocol::Other("esp".into())),
        ] {
            assert_eq!(Protocol::parse(s), p, "{s}");
        }
    }

    #[test]
    fn every_kind_has_a_body_with_the_same_name() {
        for kind in ResourceKind::ALL {
            // These two have required fields; every other body defaults.
            let value = if *kind == ResourceKind::ManagedService {
                serde_json::json!({"service": "kubernetes", "product": "eks"})
            } else if *kind == ResourceKind::SecurityGroupRule {
                serde_json::json!({
                    "group": "sg", "direction": "inbound", "action": "allow",
                    "priority": null, "protocol": "all", "ports": null,
                    "icmp": null, "peer": {"ip_net": "0.0.0.0/0"},
                    "description": null,
                })
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
            via(ResourceKind::Volume),
            Some((Edge::AttachedTo, Direction::Out))
        );
        assert_eq!(
            via(ResourceKind::LoadBalancer),
            Some((Edge::AttachedTo, Direction::In))
        );
        assert_eq!(via(ResourceKind::Instance), None);
        assert_eq!(via(ResourceKind::ManagedService), None);
        let inheriting = ResourceKind::ALL
            .iter()
            .filter(|k| k.inherits_via().is_some())
            .count();
        assert_eq!(inheriting, 6);
    }
}
