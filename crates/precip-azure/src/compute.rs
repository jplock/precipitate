//! Virtual machines with their disks, network interfaces, public IPs,
//! scale sets and images. A VM's network, security groups and load
//! balancer pools are on its network interfaces, so one collector reads
//! them together and resolves the edges.

use crate::collect::{BASE, Base, Batch};
use crate::id::{Ref, architecture, boot_mode, parent, rid, text};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};

const API: &str = "Microsoft.ResourceGraph/resources/read (compute)";

pub(crate) struct Compute;

impl Collect for Compute {
    type Context = Batch;
    const API: &'static str = API;

    async fn collect(
        &self,
        ctx: &Batch,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let rows = Rows {
            vms: ctx.graph(API, &vm_query()).await?,
            scale_set_vms: ctx.graph(API, &scale_set_vm_query()).await?,
            scale_sets: ctx.graph(API, &scale_set_query()).await?,
            nics: ctx.graph(API, NIC_QUERY).await?,
            disks: ctx.graph(API, &disk_query()).await?,
            public_ips: ctx.graph(API, &public_ip_query()).await?,
            images: ctx.graph(API, &image_query()).await?,
        };
        Ok(rows.assemble())
    }
}

fn vm_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.compute/virtualmachines'
| extend osDiskId = tolower(tostring(properties.storageProfile.osDisk.managedDisk.id))
| join kind=leftouter (
    resources
    | where type =~ 'microsoft.compute/disks'
    | project osDiskId = tolower(id), hyperVGeneration = tostring(properties.hyperVGeneration)
  ) on osDiskId
| project {BASE},
    osDiskId,
    vmSize = tostring(properties.hardwareProfile.vmSize),
    osType = tostring(properties.storageProfile.osDisk.osType),
    imageId = tostring(properties.storageProfile.imageReference.id),
    imagePublisher = tostring(properties.storageProfile.imageReference.publisher),
    imageOffer = tostring(properties.storageProfile.imageReference.offer),
    imageSku = tostring(properties.storageProfile.imageReference.sku),
    imageVersion = tostring(properties.storageProfile.imageReference.version),
    powerState = tostring(properties.extended.instanceView.powerState.code),
    timeCreated = tostring(properties.timeCreated),
    hyperVGeneration,
    securityType = tostring(properties.securityProfile.securityType),
    osDiskPlacement = tostring(properties.storageProfile.osDisk.diffDiskSettings.option),
    dataDisks = array_length(properties.storageProfile.dataDisks),
    nics = properties.networkProfile.networkInterfaces,
    scaleSetId = tostring(properties.virtualMachineScaleSet.id)"
    )
}

/// Instances of uniform scale sets live in their own table, not
/// `resources`.
fn scale_set_vm_query() -> String {
    format!(
        "computeresources
| where type =~ 'microsoft.compute/virtualmachinescalesets/virtualmachines'
| project {BASE},
    vmSize = tostring(sku.name),
    osType = tostring(properties.storageProfile.osDisk.osType),
    timeCreated = tostring(properties.timeCreated)"
    )
}

fn scale_set_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.compute/virtualmachinescalesets'
| project {BASE},
    skuName = tostring(sku.name),
    capacity = toint(sku.capacity),
    nicConfigs = properties.virtualMachineProfile.networkProfile.networkInterfaceConfigurations"
    )
}

const NIC_QUERY: &str = "resources
| where type =~ 'microsoft.network/networkinterfaces'
| project id,
    vmId = tostring(properties.virtualMachine.id),
    nsgId = tostring(properties.networkSecurityGroup.id),
    ipConfigurations = properties.ipConfigurations";

fn disk_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.compute/disks'
| project {BASE},
    sizeGb = toint(properties.diskSizeGB),
    skuName = tostring(sku.name),
    iops = toint(properties.diskIOPSReadWrite),
    mbps = toint(properties.diskMBpsReadWrite),
    encryptionSetId = tostring(properties.encryption.diskEncryptionSetId),
    diskState = tostring(properties.diskState),
    managedBy = tostring(managedBy)"
    )
}

fn public_ip_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.network/publicipaddresses'
| project {BASE},
    ipAddress = tostring(properties.ipAddress),
    ipConfigurationId = tostring(properties.ipConfiguration.id)"
    )
}

fn image_query() -> String {
    format!(
        "resources
| where type in~ ('microsoft.compute/images', 'microsoft.compute/galleries/images')
| project {BASE},
    imageOsType = tostring(properties.storageProfile.osDisk.osType),
    galleryOsType = tostring(properties.osType),
    hyperVGeneration = tostring(properties.hyperVGeneration),
    architecture = tostring(properties.architecture),
    state = tostring(properties.provisioningState)"
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct VmRow {
    #[serde(flatten)]
    pub base: Base,
    pub os_disk_id: String,
    pub vm_size: String,
    pub os_type: String,
    pub image_id: String,
    pub image_publisher: String,
    pub image_offer: String,
    pub image_sku: String,
    pub image_version: String,
    pub power_state: String,
    pub time_created: String,
    pub hyper_v_generation: String,
    pub security_type: String,
    pub os_disk_placement: String,
    pub data_disks: Option<usize>,
    pub nics: Vec<Ref>,
    pub scale_set_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ScaleSetRow {
    #[serde(flatten)]
    pub base: Base,
    pub sku_name: String,
    pub capacity: Option<i32>,
    pub nic_configs: Vec<NicConfig>,
}

/// A scale set's template for its instances' network interfaces.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct NicConfig {
    pub properties: NicConfigProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct NicConfigProperties {
    pub network_security_group: Option<Ref>,
    pub ip_configurations: Vec<IpConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct NicRow {
    pub id: String,
    pub vm_id: String,
    pub nsg_id: String,
    pub ip_configurations: Vec<IpConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct IpConfig {
    pub properties: IpConfigProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct IpConfigProperties {
    pub subnet: Option<Ref>,
    #[serde(rename = "publicIPAddress")]
    pub public_ip_address: Option<Ref>,
    pub load_balancer_backend_address_pools: Vec<Ref>,
    pub application_gateway_backend_address_pools: Vec<Ref>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct DiskRow {
    #[serde(flatten)]
    pub base: Base,
    pub size_gb: Option<i32>,
    pub sku_name: String,
    pub iops: Option<i32>,
    pub mbps: Option<i32>,
    pub encryption_set_id: String,
    pub disk_state: String,
    pub managed_by: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PublicIpRow {
    #[serde(flatten)]
    pub base: Base,
    pub ip_address: String,
    pub ip_configuration_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ImageRow {
    #[serde(flatten)]
    pub base: Base,
    pub image_os_type: String,
    pub gallery_os_type: String,
    pub hyper_v_generation: String,
    pub architecture: String,
    pub state: String,
}

#[derive(Debug, Default)]
pub(crate) struct Rows {
    pub vms: Vec<VmRow>,
    pub scale_set_vms: Vec<VmRow>,
    pub scale_sets: Vec<ScaleSetRow>,
    pub nics: Vec<NicRow>,
    pub disks: Vec<DiskRow>,
    pub public_ips: Vec<PublicIpRow>,
    pub images: Vec<ImageRow>,
}

/// What an instance's network interfaces say about it.
#[derive(Debug, Default)]
struct Wiring {
    subnets: Vec<Arn>,
    security_groups: BTreeSet<Arn>,
    pools: BTreeSet<Arn>,
    public_ips: Vec<Arn>,
}

impl Wiring {
    fn add(&mut self, nsg: Option<Arn>, configs: &[IpConfig]) {
        self.security_groups.extend(nsg);
        for c in configs {
            let p = &c.properties;
            if let Some(s) = p.subnet.as_ref().and_then(Ref::rid) {
                if !self.subnets.contains(&s) {
                    self.subnets.push(s);
                }
            }
            self.public_ips
                .extend(p.public_ip_address.as_ref().and_then(Ref::rid));
            self.pools.extend(
                p.load_balancer_backend_address_pools
                    .iter()
                    .chain(&p.application_gateway_backend_address_pools)
                    .filter_map(Ref::rid),
            );
        }
    }

    /// Each interface's subnet, primary first, and the primary's network.
    fn subnets_and_network(&self) -> (Vec<String>, Option<String>) {
        let subnets: Vec<String> =
            self.subnets.iter().map(Arn::to_string).collect();
        let network = subnets.first().and_then(|s| parent(s, "subnets"));
        (subnets, network)
    }

    fn relate(&self, out: &mut Collected, instance: &Arn) {
        for g in &self.security_groups {
            out.relate(instance.clone(), Edge::HasSecurityGroup, g.clone());
        }
        for p in &self.pools {
            out.relate(instance.clone(), Edge::TargetOf, p.clone());
        }
    }
}

/// `PowerState/running` as `running`.
fn power_state(code: &str) -> Option<String> {
    text(code.rsplit('/').next().unwrap_or(code))
}

impl VmRow {
    /// A custom image's id without its version, else the marketplace URN.
    fn image(&self) -> Option<String> {
        if let Some(id) = text(&self.image_id) {
            let id = id.to_lowercase();
            return Some(parent(&id, "versions").unwrap_or(id));
        }
        let urn = [
            &self.image_publisher,
            &self.image_offer,
            &self.image_sku,
            &self.image_version,
        ];
        urn.iter()
            .all(|p| !p.is_empty())
            .then(|| urn.map(String::as_str).join(":"))
    }

    fn boot_mode(&self) -> Option<String> {
        boot_mode(&self.hyper_v_generation).or_else(|| {
            // Trusted launch and confidential VMs are generation 2 only.
            matches!(
                self.security_type.as_str(),
                "TrustedLaunch" | "ConfidentialVM"
            )
            .then(|| "uefi".to_string())
        })
    }
}

impl Rows {
    pub(crate) fn assemble(self) -> Collected {
        let mut out = Collected::default();

        let public_ip_address: HashMap<Arn, String> = self
            .public_ips
            .iter()
            .filter_map(|p| Some((rid(&p.base.id), text(&p.ip_address)?)))
            .collect();
        let nics: HashMap<Arn, &NicRow> =
            self.nics.iter().map(|n| (rid(&n.id), n)).collect();
        let nic_vm: HashMap<Arn, Arn> = self
            .nics
            .iter()
            .filter_map(|n| Some((rid(&n.id), rid(&text(&n.vm_id)?))))
            .collect();

        for vm in &self.vms {
            let arn = rid(&vm.base.id);
            let mut wiring = Wiring::default();
            for nic in vm.nics.iter().filter_map(|n| nics.get(&rid(&n.id))) {
                wiring.add(
                    text(&nic.nsg_id).map(|g| rid(&g)),
                    &nic.ip_configurations,
                );
            }
            let (subnets, network_id) = wiring.subnets_and_network();
            let public_ip = wiring
                .public_ips
                .iter()
                .find_map(|p| public_ip_address.get(p))
                .cloned();
            wiring.relate(&mut out, &arn);
            if let Some(set) = text(&vm.scale_set_id) {
                out.relate(arn.clone(), Edge::MemberOfScaleGroup, rid(&set));
            }
            out.resources.push(vm.base.resource(Body::Instance(
                body::Instance {
                    name: Some(vm.base.name.clone()),
                    instance_type: text(&vm.vm_size),
                    architecture: None,
                    platform: text(&vm.os_type),
                    state: power_state(&vm.power_state),
                    image_id: vm.image(),
                    subnets,
                    network_id,
                    boot_disk:
                        text(&vm.os_disk_id).map(|d| rid(&d).to_string()),
                    boot_mode: vm.boot_mode(),
                    launch_time: text(&vm.time_created),
                    volume_count: 1 + vm.data_disks.unwrap_or(0),
                    local_disk_count: usize::from(
                        vm.os_disk_placement.eq_ignore_ascii_case("local"),
                    ),
                    network_interface_count: vm.nics.len(),
                    public_ip,
                },
            )));
        }

        // Uniform scale set instances take their wiring from the set's
        // template, as their interfaces are not in `resources`.
        let sets: HashMap<Arn, &ScaleSetRow> =
            self.scale_sets.iter().map(|s| (rid(&s.base.id), s)).collect();
        for vm in &self.scale_set_vms {
            let arn = rid(&vm.base.id);
            let set_id = parent(arn.as_str(), "virtualmachines").map(Arn::from);
            let set = set_id.as_ref().and_then(|id| sets.get(id));
            let mut wiring = Wiring::default();
            for c in set.into_iter().flat_map(|s| &s.nic_configs) {
                let nsg = c
                    .properties
                    .network_security_group
                    .as_ref()
                    .and_then(Ref::rid);
                wiring.add(nsg, &c.properties.ip_configurations);
            }
            wiring.public_ips.clear();
            let (subnets, network_id) = wiring.subnets_and_network();
            wiring.relate(&mut out, &arn);
            if let Some(set_id) = &set_id {
                out.relate(
                    arn.clone(),
                    Edge::MemberOfScaleGroup,
                    set_id.clone(),
                );
            }
            let instance_type = text(&vm.vm_size)
                .or_else(|| set.and_then(|s| text(&s.sku_name)));
            out.resources.push(vm.base.resource(Body::Instance(
                body::Instance {
                    name: Some(vm.base.name.clone()),
                    instance_type,
                    platform: text(&vm.os_type),
                    state: power_state(&vm.power_state),
                    subnets,
                    network_id,
                    launch_time: text(&vm.time_created),
                    volume_count: 1,
                    network_interface_count:
                        set.map_or(0, |s| s.nic_configs.len()),
                    ..Default::default()
                },
            )));
        }

        for set in &self.scale_sets {
            let arn = rid(&set.base.id);
            let observed = out
                .relationships
                .iter()
                .filter(|r| {
                    r.edge == Edge::MemberOfScaleGroup && r.to_arn == arn
                })
                .count();
            out.resources.push(set.base.resource(Body::ScaleGroup(
                body::ScaleGroup {
                    name: Some(set.base.name.clone()),
                    desired_capacity: set.capacity,
                    observed_instances: observed,
                    instance_types: text(&set.sku_name).into_iter().collect(),
                    ..Default::default()
                },
            )));
        }

        for disk in &self.disks {
            let arn = rid(&disk.base.id);
            let attached: Vec<String> = text(&disk.managed_by)
                .map(|vm| rid(&vm).to_string())
                .into_iter()
                .collect();
            for vm in &attached {
                out.relate(
                    arn.clone(),
                    Edge::AttachedTo,
                    Arn::from(vm.as_str()),
                );
            }
            out.resources.push(disk.base.resource(Body::Volume(
                body::Volume {
                    name: Some(disk.base.name.clone()),
                    size_gib: disk.size_gb,
                    volume_type: text(&disk.sku_name),
                    iops: disk.iops,
                    throughput: disk.mbps,
                    // Managed disks are always encrypted at rest; a disk
                    // encryption set means a customer-managed key.
                    encrypted: Some(true),
                    encryption_key_id:
                        text(&disk.encryption_set_id).map(|k| k.to_lowercase()),
                    state: text(&disk.disk_state),
                    attached_to: attached,
                },
            )));
        }

        for ip in &self.public_ips {
            let arn = rid(&ip.base.id);
            let nic = parent(&ip.ip_configuration_id, "ipconfigurations")
                .map(Arn::from);
            let vm = nic.as_ref().and_then(|n| nic_vm.get(n));
            if let Some(vm) = vm {
                out.relate(arn.clone(), Edge::AttachedTo, vm.clone());
            }
            out.resources.push(ip.base.resource(Body::PublicIp(
                body::PublicIp {
                    name: Some(ip.base.name.clone()),
                    public_ip: text(&ip.ip_address),
                    instance: vm.map(ToString::to_string),
                    network_interface_id: nic.map(String::from),
                    domain: None,
                },
            )));
        }

        for image in &self.images {
            let os = text(&image.image_os_type)
                .or_else(|| text(&image.gallery_os_type));
            out.resources.push(image.base.resource(Body::Image(body::Image {
                name: Some(image.base.name.clone()),
                platform: os,
                architecture: architecture(&image.architecture),
                boot_mode: boot_mode(&image.hyper_v_generation),
                state: text(&image.state),
                ..Default::default()
            })));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_types::scan::{Relationship, ResourceKind};

    const SUB: &str = "/subscriptions/S1/resourceGroups/Shop/providers";

    fn row<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
        serde_json::from_value(v).unwrap()
    }

    fn rows() -> Rows {
        Rows {
            vms: vec![row(serde_json::json!({
                "id": format!("{SUB}/Microsoft.Compute/virtualMachines/web-1"),
                "name": "web-1",
                "location": "EastUS",
                "subscriptionId": "S1",
                "tags": {"app": "shop"},
                "vmSize": "Standard_D4s_v5",
                "osType": "Windows",
                "imagePublisher": "MicrosoftWindowsServer",
                "imageOffer": "WindowsServer",
                "imageSku": "2022-datacenter",
                "imageVersion": "latest",
                "imageId": "",
                "powerState": "PowerState/running",
                "hyperVGeneration": "V1",
                "securityType": "",
                "osDiskPlacement": "",
                "dataDisks": 1,
                "nics": [{"id": format!("{SUB}/Microsoft.Network/networkInterfaces/web-1-nic")}],
                "scaleSetId": "",
            }))],
            scale_set_vms: vec![row(serde_json::json!({
                "id": format!("{SUB}/Microsoft.Compute/virtualMachineScaleSets/api/virtualMachines/0"),
                "name": "api_0",
                "location": "eastus",
                "subscriptionId": "S1",
                "tags": null,
                "vmSize": "",
                "osType": "Linux",
            }))],
            scale_sets: vec![row(serde_json::json!({
                "id": format!("{SUB}/Microsoft.Compute/virtualMachineScaleSets/api"),
                "name": "api",
                "location": "eastus",
                "subscriptionId": "S1",
                "skuName": "Standard_D2s_v5",
                "capacity": 3,
                "nicConfigs": [{"properties": {
                    "networkSecurityGroup": {"id": format!("{SUB}/Microsoft.Network/networkSecurityGroups/api-nsg")},
                    "ipConfigurations": [{"properties": {
                        "subnet": {"id": format!("{SUB}/Microsoft.Network/virtualNetworks/shop-vnet/subnets/api")},
                        "loadBalancerBackendAddressPools": [{"id": format!("{SUB}/Microsoft.Network/loadBalancers/api-lb/backendAddressPools/pool")}],
                    }}],
                }}],
            }))],
            nics: vec![row(serde_json::json!({
                "id": format!("{SUB}/Microsoft.Network/networkInterfaces/WEB-1-NIC"),
                "vmId": format!("{SUB}/Microsoft.Compute/virtualMachines/WEB-1"),
                "nsgId": format!("{SUB}/Microsoft.Network/networkSecurityGroups/web-nsg"),
                "ipConfigurations": [{"properties": {
                    "subnet": {"id": format!("{SUB}/Microsoft.Network/virtualNetworks/shop-vnet/subnets/web")},
                    "publicIPAddress": {"id": format!("{SUB}/Microsoft.Network/publicIPAddresses/web-1-ip")},
                    "applicationGatewayBackendAddressPools": [{"id": format!("{SUB}/Microsoft.Network/applicationGateways/shop-agw/backendAddressPools/web")}],
                }}],
            }))],
            disks: vec![
                row(serde_json::json!({
                    "id": format!("{SUB}/Microsoft.Compute/disks/web-1-os"),
                    "name": "web-1-os",
                    "location": "eastus",
                    "subscriptionId": "S1",
                    "sizeGb": 128,
                    "skuName": "Premium_LRS",
                    "managedBy": format!("{SUB}/Microsoft.Compute/virtualMachines/Web-1"),
                })),
                row(serde_json::json!({
                    "id": format!("{SUB}/Microsoft.Compute/disks/spare"),
                    "name": "spare",
                    "location": "eastus",
                    "subscriptionId": "S1",
                    "sizeGb": 32,
                    "managedBy": "",
                })),
            ],
            public_ips: vec![row(serde_json::json!({
                "id": format!("{SUB}/Microsoft.Network/publicIPAddresses/web-1-ip"),
                "name": "web-1-ip",
                "location": "eastus",
                "subscriptionId": "S1",
                "ipAddress": "20.1.2.3",
                "ipConfigurationId": format!("{SUB}/Microsoft.Network/networkInterfaces/web-1-nic/ipConfigurations/ipconfig1"),
            }))],
            images: vec![row(serde_json::json!({
                "id": format!("{SUB}/Microsoft.Compute/galleries/g/images/base"),
                "name": "base",
                "location": "eastus",
                "subscriptionId": "S1",
                "galleryOsType": "Linux",
                "hyperVGeneration": "V2",
                "architecture": "Arm64",
            }))],
        }
    }

    fn lower(s: &str) -> String {
        s.to_lowercase()
    }

    fn edges(out: &Collected, edge: Edge) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = out
            .relationships
            .iter()
            .filter(|r: &&Relationship| r.edge == edge)
            .map(|r| (r.from_arn.to_string(), r.to_arn.to_string()))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_vm_takes_its_network_from_its_interfaces() {
        let out = rows().assemble();
        let vm =
            lower(&format!("{SUB}/Microsoft.Compute/virtualMachines/web-1"));
        let r = out.resources.iter().find(|r| r.arn == vm.as_str()).unwrap();
        assert_eq!(r.account_id, "s1");
        assert_eq!(r.region, "eastus");
        let Body::Instance(i) = &r.body else { panic!("not an instance") };
        assert_eq!(i.instance_type.as_deref(), Some("Standard_D4s_v5"));
        assert_eq!(i.platform.as_deref(), Some("Windows"));
        assert_eq!(i.state.as_deref(), Some("running"));
        assert_eq!(i.boot_mode.as_deref(), Some("legacy-bios"));
        assert_eq!(
            i.image_id.as_deref(),
            Some("MicrosoftWindowsServer:WindowsServer:2022-datacenter:latest")
        );
        assert_eq!(i.volume_count, 2, "OS disk plus one data disk");
        assert_eq!(i.local_disk_count, 0);
        assert_eq!(i.public_ip.as_deref(), Some("20.1.2.3"));
        assert_eq!(
            i.network_id.as_deref(),
            Some(
                lower(&format!(
                    "{SUB}/Microsoft.Network/virtualNetworks/shop-vnet"
                ))
                .as_str()
            )
        );
        assert_eq!(
            edges(&out, Edge::HasSecurityGroup)[0],
            (
                vm.clone(),
                lower(&format!(
                    "{SUB}/Microsoft.Network/networkSecurityGroups/web-nsg"
                ))
            )
        );
        assert!(edges(&out, Edge::TargetOf).contains(&(
            vm.clone(),
            lower(&format!("{SUB}/Microsoft.Network/applicationGateways/shop-agw/backendAddressPools/web"))
        )));
    }

    #[test]
    fn disks_and_public_ips_attach_to_their_vm_whatever_the_case() {
        let out = rows().assemble();
        let vm =
            lower(&format!("{SUB}/Microsoft.Compute/virtualMachines/web-1"));
        assert_eq!(
            edges(&out, Edge::AttachedTo),
            [
                (
                    lower(&format!("{SUB}/Microsoft.Compute/disks/web-1-os")),
                    vm.clone()
                ),
                (
                    lower(&format!(
                        "{SUB}/Microsoft.Network/publicIPAddresses/web-1-ip"
                    )),
                    vm.clone()
                ),
            ]
        );
        let ip = out.of_kind(ResourceKind::PublicIp).next().unwrap();
        let Body::PublicIp(p) = &ip.body else { panic!() };
        assert_eq!(p.instance.as_deref(), Some(vm.as_str()));
        assert_eq!(
            out.of_kind(ResourceKind::Volume).count(),
            2,
            "unattached disks are kept"
        );
    }

    #[test]
    fn scale_set_instances_take_the_sets_size_and_wiring() {
        let out = rows().assemble();
        let set = lower(&format!(
            "{SUB}/Microsoft.Compute/virtualMachineScaleSets/api"
        ));
        let vm = format!("{set}/virtualmachines/0");
        let r = out.resources.iter().find(|r| r.arn == vm.as_str()).unwrap();
        let Body::Instance(i) = &r.body else { panic!() };
        assert_eq!(i.instance_type.as_deref(), Some("Standard_D2s_v5"));
        assert_eq!(i.network_interface_count, 1);
        assert_eq!(
            edges(&out, Edge::MemberOfScaleGroup),
            [(vm.clone(), set.clone())]
        );
        assert!(edges(&out, Edge::TargetOf).contains(&(
            vm,
            lower(&format!("{SUB}/Microsoft.Network/loadBalancers/api-lb/backendAddressPools/pool"))
        )));
        let g = out.of_kind(ResourceKind::ScaleGroup).next().unwrap();
        let Body::ScaleGroup(g) = &g.body else { panic!() };
        assert_eq!((g.desired_capacity, g.observed_instances), (Some(3), 1));
        assert_eq!(g.instance_types, ["Standard_D2s_v5"]);
    }

    #[test]
    fn gallery_images_carry_os_architecture_and_boot_mode() {
        let out = rows().assemble();
        let img = out.of_kind(ResourceKind::Image).next().unwrap();
        let Body::Image(i) = &img.body else { panic!() };
        assert_eq!(i.platform.as_deref(), Some("Linux"));
        assert_eq!(i.architecture.as_deref(), Some("arm64"));
        assert_eq!(i.boot_mode.as_deref(), Some("uefi"));
    }

    #[test]
    fn custom_images_drop_their_version_and_trusted_launch_boots_uefi() {
        let vm = VmRow {
            image_id: format!(
                "{SUB}/Microsoft.Compute/galleries/g/images/base/versions/1.0.0"
            ),
            security_type: "TrustedLaunch".into(),
            ..Default::default()
        };
        assert_eq!(
            vm.image().as_deref(),
            Some(
                lower(&format!(
                    "{SUB}/Microsoft.Compute/galleries/g/images/base"
                ))
                .as_str()
            )
        );
        assert_eq!(vm.boot_mode().as_deref(), Some("uefi"));
        let partial =
            VmRow { image_publisher: "x".into(), ..Default::default() };
        assert_eq!(partial.image(), None, "an incomplete URN is no image");
    }

    #[test]
    fn an_ephemeral_os_disk_is_a_local_disk() {
        let mut r = rows();
        r.vms[0].os_disk_placement = "Local".into();
        let out = r.assemble();
        let Body::Instance(i) =
            &out.of_kind(ResourceKind::Instance).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(i.local_disk_count, 1);
    }
}
