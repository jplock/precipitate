//! What the scanner records about each kind of resource. Each body
//! serializes to the same JSON object the database stores, so content
//! hashes and `json_extract` queries see the same keys. Every field has a
//! default so rows written before a field existed still load.

use crate::scan::ResourceKind;
use serde::{Deserialize, Serialize};

/// The collected fields of one resource, by kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub enum Body {
    Account(Account),
    Ec2Instance(Instance),
    InstanceType(InstanceType),
    EbsVolume(Volume),
    Ami(Image),
    Vpc(Vpc),
    Subnet(Subnet),
    SecurityGroup(SecurityGroup),
    SecurityGroupRule(SecurityGroupRule),
    RouteTable(RouteTable),
    InternetGateway(InternetGateway),
    NatGateway(NatGateway),
    VpcPeeringConnection(PeeringConnection),
    PrefixList(PrefixList),
    AutoScalingGroup(AutoScalingGroup),
    LaunchTemplate(LaunchTemplate),
    LaunchConfiguration(LaunchConfiguration),
    ElasticIp(ElasticIp),
    LoadBalancer(LoadBalancer),
    TargetGroup(TargetGroup),
    CloudFormationStack(Stack),
    ManagedService(Managed),
}

impl Body {
    pub fn kind(&self) -> ResourceKind {
        match self {
            Self::Account(_) => ResourceKind::Account,
            Self::Ec2Instance(_) => ResourceKind::Ec2Instance,
            Self::InstanceType(_) => ResourceKind::InstanceType,
            Self::EbsVolume(_) => ResourceKind::EbsVolume,
            Self::Ami(_) => ResourceKind::Ami,
            Self::Vpc(_) => ResourceKind::Vpc,
            Self::Subnet(_) => ResourceKind::Subnet,
            Self::SecurityGroup(_) => ResourceKind::SecurityGroup,
            Self::SecurityGroupRule(_) => ResourceKind::SecurityGroupRule,
            Self::RouteTable(_) => ResourceKind::RouteTable,
            Self::InternetGateway(_) => ResourceKind::InternetGateway,
            Self::NatGateway(_) => ResourceKind::NatGateway,
            Self::VpcPeeringConnection(_) => ResourceKind::VpcPeeringConnection,
            Self::PrefixList(_) => ResourceKind::PrefixList,
            Self::AutoScalingGroup(_) => ResourceKind::AutoScalingGroup,
            Self::LaunchTemplate(_) => ResourceKind::LaunchTemplate,
            Self::LaunchConfiguration(_) => ResourceKind::LaunchConfiguration,
            Self::ElasticIp(_) => ResourceKind::ElasticIp,
            Self::LoadBalancer(_) => ResourceKind::LoadBalancer,
            Self::TargetGroup(_) => ResourceKind::TargetGroup,
            Self::CloudFormationStack(_) => ResourceKind::CloudFormationStack,
            Self::ManagedService(_) => ResourceKind::ManagedService,
        }
    }

    /// The resource's own name, for kinds that have one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Account(b) => Some(&b.name),
            Self::InstanceType(b) => Some(&b.name),
            Self::Ami(b) => b.name.as_deref(),
            Self::PrefixList(b) => b.name.as_deref(),
            Self::AutoScalingGroup(b) => b.name.as_deref(),
            Self::LaunchTemplate(b) => b.name.as_deref(),
            Self::LaunchConfiguration(b) => b.name.as_deref(),
            Self::LoadBalancer(b) => b.name.as_deref(),
            Self::TargetGroup(b) => b.name.as_deref(),
            Self::CloudFormationStack(b) => b.name.as_deref(),
            Self::ManagedService(b) => b.name.as_deref(),
            Self::Ec2Instance(_)
            | Self::EbsVolume(_)
            | Self::Vpc(_)
            | Self::Subnet(_)
            | Self::SecurityGroup(_)
            | Self::SecurityGroupRule(_)
            | Self::RouteTable(_)
            | Self::InternetGateway(_)
            | Self::NatGateway(_)
            | Self::VpcPeeringConnection(_)
            | Self::ElasticIp(_) => None,
        }
    }

    /// The instance type and image a launch source names: an instance, a
    /// launch template or a launch configuration.
    pub fn launch_source(&self) -> Option<(Option<&str>, Option<&str>)> {
        match self {
            Self::Ec2Instance(b) => {
                Some((b.instance_type.as_deref(), b.image_id.as_deref()))
            }
            Self::LaunchTemplate(b) => {
                Some((b.instance_type.as_deref(), b.image_id.as_deref()))
            }
            Self::LaunchConfiguration(b) => {
                Some((b.instance_type.as_deref(), b.image_id.as_deref()))
            }
            Self::Account(_)
            | Self::InstanceType(_)
            | Self::EbsVolume(_)
            | Self::Ami(_)
            | Self::Vpc(_)
            | Self::Subnet(_)
            | Self::SecurityGroup(_)
            | Self::SecurityGroupRule(_)
            | Self::RouteTable(_)
            | Self::InternetGateway(_)
            | Self::NatGateway(_)
            | Self::VpcPeeringConnection(_)
            | Self::PrefixList(_)
            | Self::AutoScalingGroup(_)
            | Self::ElasticIp(_)
            | Self::LoadBalancer(_)
            | Self::TargetGroup(_)
            | Self::CloudFormationStack(_)
            | Self::ManagedService(_) => None,
        }
    }

    /// The fields alone, as the database stores them.
    pub fn to_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        let mut tagged = serde_json::to_value(self)?;
        Ok(tagged
            .get_mut("body")
            .map(serde_json::Value::take)
            .unwrap_or_default())
    }

    /// A body from its kind and the stored fields.
    pub fn from_value(
        kind: ResourceKind,
        body: serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        serde_json::from_value(
            serde_json::json!({ "kind": kind, "body": body }),
        )
    }
}

macro_rules! bodies {
    ($(
        $(#[$meta:meta])*
        $name:ident { $($(#[$fmeta:meta])* $field:ident: $ty:ty),* $(,)? }
    )+) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default)]
        pub struct $name {
            $($(#[$fmeta])* pub $field: $ty,)*
        }
    )+};
}

bodies! {
    /// An AWS account in the Organization, with its OU.
    Account {
        name: String,
        status: String,
        parent_id: Option<String>,
        parent_name: Option<String>,
        partition: String,
    }
    Instance {
        instance_id: String,
        instance_type: Option<String>,
        architecture: Option<String>,
        platform: Option<String>,
        state: Option<String>,
        image_id: Option<String>,
        subnet_id: Option<String>,
        vpc_id: Option<String>,
        boot_mode: Option<String>,
        current_boot_mode: Option<String>,
        launch_time: Option<String>,
        ebs_volume_count: usize,
        instance_store_volume_count: usize,
        network_interface_count: usize,
        public_ip: Option<String>,
    }
    InstanceType {
        name: String,
        vcpus: Option<i32>,
        memory_mib: Option<i64>,
        architectures: Option<Vec<String>>,
        bare_metal: Option<bool>,
        gpus: usize,
        accelerators: usize,
        instance_storage_gb: Option<i64>,
        network_performance: Option<String>,
    }
    Volume {
        volume_id: String,
        size_gib: Option<i32>,
        volume_type: Option<String>,
        iops: Option<i32>,
        throughput: Option<i32>,
        encrypted: Option<bool>,
        kms_key_id: Option<String>,
        state: Option<String>,
        attached_to: Vec<String>,
    }
    Image {
        image_id: String,
        name: Option<String>,
        owner_id: Option<String>,
        owner_alias: Option<String>,
        platform: Option<String>,
        architecture: Option<String>,
        boot_mode: Option<String>,
        marketplace_product_codes: usize,
        encrypted_snapshot: bool,
        public: Option<bool>,
        state: Option<String>,
    }
    Vpc {
        vpc_id: String,
        cidr_block: Option<String>,
        cidr_blocks: Vec<String>,
        is_default: Option<bool>,
        owner_id: Option<String>,
    }
    Subnet {
        subnet_id: String,
        vpc_id: Option<String>,
        cidr_block: Option<String>,
        ipv6_cidr_blocks: Vec<String>,
        availability_zone: Option<String>,
        map_public_ip_on_launch: Option<bool>,
    }
    SecurityGroup {
        group_id: String,
        group_name: Option<String>,
        description: Option<String>,
        vpc_id: Option<String>,
    }
    /// One rule, with its source kept distinct: CIDR, prefix list or
    /// another security group.
    SecurityGroupRule {
        rule_id: String,
        group_id: Option<String>,
        egress: Option<bool>,
        ip_protocol: Option<String>,
        from_port: Option<i32>,
        to_port: Option<i32>,
        cidr_ipv4: Option<String>,
        cidr_ipv6: Option<String>,
        prefix_list_id: Option<String>,
        referenced_group_id: Option<String>,
        description: Option<String>,
    }
    RouteTable {
        route_table_id: String,
        vpc_id: Option<String>,
        main: bool,
        subnets: Vec<String>,
        routes: Vec<Route>,
    }
    Route {
        destination: Option<String>,
        target: Option<String>,
        state: Option<String>,
    }
    InternetGateway {
        internet_gateway_id: String,
        vpcs: Vec<String>,
    }
    NatGateway {
        nat_gateway_id: String,
        vpc_id: Option<String>,
        subnet_id: Option<String>,
        state: Option<String>,
        connectivity: Option<String>,
        public_ips: Vec<String>,
    }
    PeeringConnection {
        vpc_peering_connection_id: String,
        status: Option<String>,
        requester: PeeringSide,
        accepter: PeeringSide,
    }
    PeeringSide {
        vpc_id: Option<String>,
        owner_id: Option<String>,
        region: Option<String>,
        cidr_block: Option<String>,
    }
    PrefixList {
        prefix_list_id: String,
        name: Option<String>,
        owner_id: Option<String>,
        address_family: Option<String>,
        entries: Vec<String>,
    }
    AutoScalingGroup {
        name: Option<String>,
        min_size: Option<i32>,
        desired_capacity: Option<i32>,
        max_size: Option<i32>,
        observed_instances: usize,
        launch_template_id: Option<String>,
        launch_configuration_name: Option<String>,
        launch_template_version: Option<String>,
        /// Mixed-instance overrides, sorted and distinct.
        instance_types: Vec<String>,
    }
    /// The launch template version an Auto Scaling group points at.
    LaunchTemplate {
        launch_template_id: String,
        name: Option<String>,
        version: i64,
        requested_version: String,
        instance_type: Option<String>,
        image_id: Option<String>,
    }
    LaunchConfiguration {
        name: Option<String>,
        instance_type: Option<String>,
        image_id: Option<String>,
        created: Option<String>,
    }
    ElasticIp {
        allocation_id: String,
        public_ip: Option<String>,
        instance_id: Option<String>,
        network_interface_id: Option<String>,
        domain: Option<String>,
    }
    LoadBalancer {
        name: Option<String>,
        #[serde(rename = "type")]
        lb_type: Option<String>,
        scheme: Option<String>,
        vpc_id: Option<String>,
        dns_name: Option<String>,
        security_groups: Vec<String>,
    }
    TargetGroup {
        name: Option<String>,
        target_type: Option<String>,
        protocol: Option<String>,
        port: Option<i32>,
        vpc_id: Option<String>,
        load_balancer_arns: Vec<String>,
        instance_targets: usize,
    }
    Stack {
        name: Option<String>,
        parent_id: Option<String>,
        root_id: Option<String>,
        status: Option<String>,
    }
}

/// A managed service with no Oxide equivalent. Counted, never sized. The
/// service has no default: a row that does not name one is an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Managed {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(flatten)]
    pub service: Service,
}

/// Which managed service, with the fields its list call returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "service", rename_all = "lowercase")]
pub enum Service {
    Rds {
        engine: Option<String>,
        engine_version: Option<String>,
        instance_class: Option<String>,
        allocated_storage_gib: Option<i32>,
        multi_az: Option<bool>,
        vpc_id: Option<String>,
    },
    Eks,
    Ecs,
    Lambda {
        runtime: Option<String>,
        memory_mib: Option<i32>,
        vpc_id: Option<String>,
    },
    ElastiCache {
        engine: Option<String>,
        node_type: Option<String>,
        nodes: Option<i32>,
    },
    DynamoDb,
    Efs {
        size_bytes: Option<i64>,
    },
}

impl Service {
    /// The service's name, as stored in the body.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rds { .. } => "rds",
            Self::Eks => "eks",
            Self::Ecs => "ecs",
            Self::Lambda { .. } => "lambda",
            Self::ElastiCache { .. } => "elasticache",
            Self::DynamoDb => "dynamodb",
            Self::Efs { .. } => "efs",
        }
    }
}
