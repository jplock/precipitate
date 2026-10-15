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
    Instance(Instance),
    InstanceType(InstanceType),
    Volume(Volume),
    Image(Image),
    Network(Network),
    Subnet(Subnet),
    SecurityGroup(SecurityGroup),
    SecurityGroupRule(SecurityGroupRule),
    RouteTable(RouteTable),
    InternetGateway(InternetGateway),
    NatGateway(NatGateway),
    PeeringConnection(PeeringConnection),
    PrefixList(PrefixList),
    ScaleGroup(ScaleGroup),
    LaunchTemplate(LaunchTemplate),
    LaunchConfiguration(LaunchConfiguration),
    PublicIp(PublicIp),
    LoadBalancer(LoadBalancer),
    TargetGroup(TargetGroup),
    DeploymentGroup(DeploymentGroup),
    ManagedService(Managed),
}

impl Body {
    pub fn kind(&self) -> ResourceKind {
        match self {
            Self::Account(_) => ResourceKind::Account,
            Self::Instance(_) => ResourceKind::Instance,
            Self::InstanceType(_) => ResourceKind::InstanceType,
            Self::Volume(_) => ResourceKind::Volume,
            Self::Image(_) => ResourceKind::Image,
            Self::Network(_) => ResourceKind::Network,
            Self::Subnet(_) => ResourceKind::Subnet,
            Self::SecurityGroup(_) => ResourceKind::SecurityGroup,
            Self::SecurityGroupRule(_) => ResourceKind::SecurityGroupRule,
            Self::RouteTable(_) => ResourceKind::RouteTable,
            Self::InternetGateway(_) => ResourceKind::InternetGateway,
            Self::NatGateway(_) => ResourceKind::NatGateway,
            Self::PeeringConnection(_) => ResourceKind::PeeringConnection,
            Self::PrefixList(_) => ResourceKind::PrefixList,
            Self::ScaleGroup(_) => ResourceKind::ScaleGroup,
            Self::LaunchTemplate(_) => ResourceKind::LaunchTemplate,
            Self::LaunchConfiguration(_) => ResourceKind::LaunchConfiguration,
            Self::PublicIp(_) => ResourceKind::PublicIp,
            Self::LoadBalancer(_) => ResourceKind::LoadBalancer,
            Self::TargetGroup(_) => ResourceKind::TargetGroup,
            Self::DeploymentGroup(_) => ResourceKind::DeploymentGroup,
            Self::ManagedService(_) => ResourceKind::ManagedService,
        }
    }

    /// The resource's own name, where the cloud gives it one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Account(b) => Some(&b.name),
            Self::InstanceType(b) => Some(&b.name),
            Self::Instance(b) => b.name.as_deref(),
            Self::Volume(b) => b.name.as_deref(),
            Self::Image(b) => b.name.as_deref(),
            Self::Network(b) => b.name.as_deref(),
            Self::Subnet(b) => b.name.as_deref(),
            Self::SecurityGroup(b) => b.name.as_deref(),
            Self::SecurityGroupRule(_) => None,
            Self::RouteTable(b) => b.name.as_deref(),
            Self::InternetGateway(b) => b.name.as_deref(),
            Self::NatGateway(b) => b.name.as_deref(),
            Self::PeeringConnection(b) => b.name.as_deref(),
            Self::PrefixList(b) => b.name.as_deref(),
            Self::ScaleGroup(b) => b.name.as_deref(),
            Self::LaunchTemplate(b) => b.name.as_deref(),
            Self::LaunchConfiguration(b) => b.name.as_deref(),
            Self::PublicIp(b) => b.name.as_deref(),
            Self::LoadBalancer(b) => b.name.as_deref(),
            Self::TargetGroup(b) => b.name.as_deref(),
            Self::DeploymentGroup(b) => b.name.as_deref(),
            Self::ManagedService(b) => b.name.as_deref(),
        }
    }

    /// The instance type and image a launch source names: an instance, a
    /// launch template or a launch configuration.
    pub fn launch_source(&self) -> Option<(Option<&str>, Option<&str>)> {
        match self {
            Self::Instance(b) => {
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
            | Self::Volume(_)
            | Self::Image(_)
            | Self::Network(_)
            | Self::Subnet(_)
            | Self::SecurityGroup(_)
            | Self::SecurityGroupRule(_)
            | Self::RouteTable(_)
            | Self::InternetGateway(_)
            | Self::NatGateway(_)
            | Self::PeeringConnection(_)
            | Self::PrefixList(_)
            | Self::ScaleGroup(_)
            | Self::PublicIp(_)
            | Self::LoadBalancer(_)
            | Self::TargetGroup(_)
            | Self::DeploymentGroup(_)
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
    /// A cloud account: an AWS account in the Organization with its OU, an
    /// Azure subscription with its management group, or a Google Cloud
    /// project with its folder. Becomes an Oxide project's scope.
    Account {
        name: String,
        status: String,
        parent_id: Option<String>,
        parent_name: Option<String>,
        /// The AWS partition (`aws`, `aws-us-gov`), Azure cloud
        /// (`AzureCloud`, `AzureUSGovernment`) or `gcp` the account lives
        /// in.
        cloud: String,
    }
    /// A virtual machine, which becomes an `oxide_instance`. Scanners
    /// normalise `architecture` to `x86_64` or `arm64` and `boot_mode` to
    /// `legacy-bios` or `uefi` where they can.
    Instance {
        name: Option<String>,
        instance_type: Option<String>,
        architecture: Option<String>,
        /// The operating system as the provider reports it. Every Windows
        /// variant starts with `Windows`.
        platform: Option<String>,
        state: Option<String>,
        /// The image's ARN, or a marketplace image's URN.
        image_id: Option<String>,
        /// The subnet of each network interface, by ARN, primary first.
        subnets: Vec<String>,
        /// The network of the primary interface, by ARN.
        network_id: Option<String>,
        /// The disk it boots from, by ARN.
        boot_disk: Option<String>,
        boot_mode: Option<String>,
        launch_time: Option<String>,
        /// Network-attached data and OS disks.
        volume_count: usize,
        /// Disks on the host that do not survive a stop, such as EC2
        /// instance store.
        local_disk_count: usize,
        network_interface_count: usize,
        /// The public address, if any. A static address is also a
        /// `PublicIp` attached to the instance.
        public_ip: Option<String>,
    }
    /// vCPU and memory for a machine size seen in a region: an EC2
    /// instance type or an Azure VM size.
    InstanceType {
        name: String,
        vcpus: Option<i32>,
        memory_mib: Option<i64>,
        architectures: Option<Vec<String>>,
        bare_metal: Option<bool>,
        gpus: usize,
        accelerators: usize,
        local_storage_gb: Option<i64>,
        network_performance: Option<String>,
    }
    /// A network-attached disk, which becomes an `oxide_disk`.
    Volume {
        name: Option<String>,
        size_gib: Option<i32>,
        volume_type: Option<String>,
        iops: Option<i32>,
        throughput: Option<i32>,
        encrypted: Option<bool>,
        encryption_key_id: Option<String>,
        state: Option<String>,
        /// The instances it is attached to, by ARN.
        attached_to: Vec<String>,
    }
    Image {
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
    /// A VPC or virtual network, which becomes an `oxide_vpc`.
    Network {
        /// The network's own name, for providers that name networks
        /// outside their tags.
        name: Option<String>,
        cidr_block: Option<String>,
        cidr_blocks: Vec<String>,
        is_default: Option<bool>,
        owner_id: Option<String>,
    }
    /// A subnet, which becomes an `oxide_vpc_subnet`.
    Subnet {
        name: Option<String>,
        /// The network, by ARN.
        network_id: Option<String>,
        cidr_block: Option<String>,
        ipv6_cidr_blocks: Vec<String>,
        availability_zone: Option<String>,
        map_public_ip_on_launch: Option<bool>,
    }
    /// An EC2 security group, an Azure network security group or a Google
    /// Cloud firewall. Its rules become `oxide_vpc_firewall_rules`
    /// targeting the instances related to it by `has_security_group`.
    SecurityGroup {
        name: Option<String>,
        description: Option<String>,
        /// The network, by ARN.
        network_id: Option<String>,
    }
    RouteTable {
        name: Option<String>,
        /// The network, by ARN.
        network_id: Option<String>,
        main: bool,
        /// The subnets associated with it, by ARN.
        subnets: Vec<String>,
        routes: Vec<Route>,
    }
    InternetGateway {
        name: Option<String>,
        /// The networks it is attached to, by ARN.
        networks: Vec<String>,
    }
    NatGateway {
        name: Option<String>,
        /// The network and subnet, by ARN.
        network_id: Option<String>,
        subnet_id: Option<String>,
        state: Option<String>,
        connectivity: Option<String>,
        /// Its public addresses, by ARN.
        public_ips: Vec<String>,
    }
    PeeringConnection {
        name: Option<String>,
        status: Option<String>,
        requester: PeeringSide,
        accepter: PeeringSide,
    }
    PeeringSide {
        /// The network, by ARN.
        network_id: Option<String>,
        owner_id: Option<String>,
        region: Option<String>,
        cidr_block: Option<String>,
    }
    /// An AWS managed prefix list or an Azure IP group.
    PrefixList {
        name: Option<String>,
        owner_id: Option<String>,
        address_family: Option<String>,
        entries: Vec<String>,
    }
    /// An Auto Scaling group, VM scale set or managed instance group.
    ScaleGroup {
        name: Option<String>,
        min_size: Option<i32>,
        desired_capacity: Option<i32>,
        max_size: Option<i32>,
        observed_instances: usize,
        /// The AWS launch template's own id, which with the version names
        /// the template the group launches; not a resource reference.
        launch_template_id: Option<String>,
        launch_configuration_name: Option<String>,
        launch_template_version: Option<String>,
        /// The machine sizes the group launches, sorted and distinct.
        instance_types: Vec<String>,
    }
    /// The launch template version an Auto Scaling group points at.
    LaunchTemplate {
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
    /// A static public address (an Elastic IP or an Azure or Google Cloud
    /// public IP), which becomes an `oxide_floating_ip`.
    PublicIp {
        name: Option<String>,
        public_ip: Option<String>,
        /// The instance it is attached to, by ARN.
        instance: Option<String>,
        network_interface_id: Option<String>,
        domain: Option<String>,
    }
    LoadBalancer {
        name: Option<String>,
        #[serde(rename = "type")]
        lb_type: Option<String>,
        scheme: Option<String>,
        /// The network, by ARN.
        network_id: Option<String>,
        dns_name: Option<String>,
        /// Its security groups, by ARN.
        security_groups: Vec<String>,
    }
    /// A target group or a load balancer backend pool.
    TargetGroup {
        name: Option<String>,
        target_type: Option<String>,
        protocol: Option<String>,
        port: Option<i32>,
        /// The network, by ARN.
        network_id: Option<String>,
        /// The load balancers in front of it, by ARN.
        load_balancers: Vec<String>,
        instance_targets: usize,
    }
    /// A unit resources are deployed in together: a CloudFormation stack,
    /// an Azure resource group or a GKE cluster's nodes.
    DeploymentGroup {
        name: Option<String>,
        /// The parent and root groups, by ARN.
        parent_id: Option<String>,
        root_id: Option<String>,
        status: Option<String>,
    }
}

/// One firewall rule in Oxide's terms: a direction, an action, a priority,
/// a protocol filter and one peer. The instances it applies to are those
/// related to its group by `has_security_group`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityGroupRule {
    /// The group, NSG or firewall it belongs to, by ARN.
    pub group: String,
    pub direction: TrafficDirection,
    pub action: RuleAction,
    /// Lowest first. `None` where the provider does not order rules, as in
    /// EC2, whose rules only allow.
    pub priority: Option<i32>,
    pub protocol: Protocol,
    /// The destination port range. `None` is every port.
    pub ports: Option<(i32, i32)>,
    /// The ICMP type and code, for ICMP. `None` is every type.
    pub icmp: Option<(i32, Option<i32>)>,
    /// Where traffic comes from (inbound) or goes to (outbound).
    pub peer: Peer,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficDirection {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Allow,
    Deny,
}

/// The protocols an Oxide firewall rule can filter on, and any other the
/// provider names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    All,
    Tcp,
    Udp,
    Icmp,
    Icmp6,
    Other(String),
}

impl Protocol {
    /// A protocol by name or IANA number, as the providers write them:
    /// `tcp`, `6`, `-1` and `*` for every protocol, and so on.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "-1" | "*" | "all" | "any" => Self::All,
            "tcp" | "6" => Self::Tcp,
            "udp" | "17" => Self::Udp,
            "icmp" | "1" => Self::Icmp,
            "icmpv6" | "icmp6" | "58" => Self::Icmp6,
            other => Self::Other(other.to_string()),
        }
    }
}

/// The far side of a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Peer {
    /// An IPv4 or IPv6 network, `0.0.0.0/0` for anywhere.
    IpNet(String),
    /// A prefix list or IP group, by ARN, whose entries are IP networks.
    PrefixList(String),
    /// The instances in another security group, by ARN.
    Group(String),
    /// The instances carrying a Google Cloud network tag.
    Tag(String),
    /// The instances running as a Google Cloud service account.
    ServiceAccount(String),
    /// An Azure service tag, such as `Internet` or `VirtualNetwork`.
    ServiceTag(String),
}

/// One route of a route table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Route {
    /// A CIDR, or a prefix list by ARN.
    pub destination: Option<String>,
    pub target: RouteTarget,
    pub state: Option<String>,
}

/// Where a route sends traffic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    /// Within the network; Oxide's system router covers these.
    #[default]
    Local,
    /// An internet gateway, by ARN.
    InternetGateway(String),
    /// The internet, through a gateway the provider does not list as a
    /// resource (Azure's `Internet` next hop, Google Cloud's default
    /// internet gateway).
    Internet,
    /// Traffic is dropped.
    Drop,
    NatGateway(String),
    Instance(String),
    /// A next-hop address, such as a virtual appliance.
    Ip(String),
    Peering(String),
    /// A target with no Oxide equivalent (a transit gateway, a VPN), by
    /// the provider's id or name.
    Other(String),
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

/// Which kind of managed service, with the fields its list call returns.
/// `product` names the provider's service, such as `rds` or
/// `postgresql_flexible`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "service", rename_all = "snake_case")]
pub enum Service {
    Database {
        product: String,
        engine: Option<String>,
        engine_version: Option<String>,
        instance_class: Option<String>,
        allocated_storage_gib: Option<i32>,
        high_availability: Option<bool>,
        network_id: Option<String>,
    },
    Kubernetes {
        product: String,
    },
    Containers {
        product: String,
    },
    Functions {
        product: String,
        runtime: Option<String>,
        memory_mib: Option<i32>,
        network_id: Option<String>,
    },
    Cache {
        product: String,
        engine: Option<String>,
        node_type: Option<String>,
        nodes: Option<i32>,
    },
    KeyValue {
        product: String,
    },
    FileShare {
        product: String,
        size_bytes: Option<i64>,
    },
}

impl Service {
    /// The kind of service, as stored in the body.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Database { .. } => "database",
            Self::Kubernetes { .. } => "kubernetes",
            Self::Containers { .. } => "containers",
            Self::Functions { .. } => "functions",
            Self::Cache { .. } => "cache",
            Self::KeyValue { .. } => "key_value",
            Self::FileShare { .. } => "file_share",
        }
    }

    /// The provider's name for the service.
    pub fn product(&self) -> &str {
        match self {
            Self::Database { product, .. }
            | Self::Kubernetes { product }
            | Self::Containers { product }
            | Self::Functions { product, .. }
            | Self::Cache { product, .. }
            | Self::KeyValue { product }
            | Self::FileShare { product, .. } => product,
        }
    }
}
