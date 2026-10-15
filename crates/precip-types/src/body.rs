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

    /// The resource's own name, for kinds that have one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Account(b) => Some(&b.name),
            Self::InstanceType(b) => Some(&b.name),
            Self::Image(b) => b.name.as_deref(),
            Self::Network(b) => b.name.as_deref(),
            Self::PrefixList(b) => b.name.as_deref(),
            Self::ScaleGroup(b) => b.name.as_deref(),
            Self::LaunchTemplate(b) => b.name.as_deref(),
            Self::LaunchConfiguration(b) => b.name.as_deref(),
            Self::LoadBalancer(b) => b.name.as_deref(),
            Self::TargetGroup(b) => b.name.as_deref(),
            Self::DeploymentGroup(b) => b.name.as_deref(),
            Self::ManagedService(b) => b.name.as_deref(),
            Self::Instance(_)
            | Self::Volume(_)
            | Self::Subnet(_)
            | Self::SecurityGroup(_)
            | Self::SecurityGroupRule(_)
            | Self::RouteTable(_)
            | Self::InternetGateway(_)
            | Self::NatGateway(_)
            | Self::PeeringConnection(_)
            | Self::PublicIp(_) => None,
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
    /// A cloud account: an AWS account in the Organization with its OU, or
    /// an Azure subscription with its management group.
    Account {
        name: String,
        status: String,
        parent_id: Option<String>,
        parent_name: Option<String>,
        /// The AWS partition (`aws`, `aws-us-gov`) or Azure cloud
        /// (`AzureCloud`, `AzureUSGovernment`) the account lives in.
        cloud: String,
    }
    /// A virtual machine. Scanners normalise `architecture` to `x86_64` or
    /// `arm64` and `boot_mode` to `legacy-bios` or `uefi` where they can.
    Instance {
        instance_id: String,
        instance_type: Option<String>,
        architecture: Option<String>,
        /// The operating system as the provider reports it. Every Windows
        /// variant starts with `Windows`.
        platform: Option<String>,
        state: Option<String>,
        image_id: Option<String>,
        subnet_id: Option<String>,
        network_id: Option<String>,
        boot_mode: Option<String>,
        launch_time: Option<String>,
        /// Network-attached data and OS disks.
        volume_count: usize,
        /// Disks on the host that do not survive a stop, such as EC2
        /// instance store.
        local_disk_count: usize,
        network_interface_count: usize,
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
    Volume {
        volume_id: String,
        size_gib: Option<i32>,
        volume_type: Option<String>,
        iops: Option<i32>,
        throughput: Option<i32>,
        encrypted: Option<bool>,
        encryption_key_id: Option<String>,
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
    /// A VPC or a virtual network.
    Network {
        network_id: String,
        /// The network's own name, for providers that name networks
        /// outside their tags.
        name: Option<String>,
        cidr_block: Option<String>,
        cidr_blocks: Vec<String>,
        is_default: Option<bool>,
        owner_id: Option<String>,
    }
    Subnet {
        subnet_id: String,
        network_id: Option<String>,
        cidr_block: Option<String>,
        ipv6_cidr_blocks: Vec<String>,
        availability_zone: Option<String>,
        map_public_ip_on_launch: Option<bool>,
    }
    /// An EC2 security group or an Azure network security group.
    SecurityGroup {
        group_id: String,
        group_name: Option<String>,
        description: Option<String>,
        network_id: Option<String>,
    }
    /// One rule, with its source kept distinct: CIDR, prefix list (or
    /// Azure service tag) or another security group.
    SecurityGroupRule {
        rule_id: String,
        group_id: Option<String>,
        egress: Option<bool>,
        /// Azure rules can deny; EC2 rules only allow.
        deny: bool,
        /// Azure evaluates rules in priority order, lowest first.
        priority: Option<i32>,
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
        network_id: Option<String>,
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
        networks: Vec<String>,
    }
    NatGateway {
        nat_gateway_id: String,
        network_id: Option<String>,
        subnet_id: Option<String>,
        state: Option<String>,
        connectivity: Option<String>,
        public_ips: Vec<String>,
    }
    PeeringConnection {
        peering_id: String,
        status: Option<String>,
        requester: PeeringSide,
        accepter: PeeringSide,
    }
    PeeringSide {
        network_id: Option<String>,
        owner_id: Option<String>,
        region: Option<String>,
        cidr_block: Option<String>,
    }
    /// An AWS managed prefix list or an Azure IP group.
    PrefixList {
        prefix_list_id: String,
        name: Option<String>,
        owner_id: Option<String>,
        address_family: Option<String>,
        entries: Vec<String>,
    }
    /// An Auto Scaling group or a VM scale set.
    ScaleGroup {
        name: Option<String>,
        min_size: Option<i32>,
        desired_capacity: Option<i32>,
        max_size: Option<i32>,
        observed_instances: usize,
        launch_template_id: Option<String>,
        launch_configuration_name: Option<String>,
        launch_template_version: Option<String>,
        /// The machine sizes the group launches, sorted and distinct.
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
    /// An Elastic IP or an Azure public IP address.
    PublicIp {
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
        network_id: Option<String>,
        dns_name: Option<String>,
        security_groups: Vec<String>,
    }
    /// A target group or a load balancer backend pool.
    TargetGroup {
        name: Option<String>,
        target_type: Option<String>,
        protocol: Option<String>,
        port: Option<i32>,
        network_id: Option<String>,
        load_balancers: Vec<String>,
        instance_targets: usize,
    }
    /// A unit resources are deployed in together: a CloudFormation stack
    /// or an Azure resource group.
    DeploymentGroup {
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
