//! Virtual networks and what hangs off them: subnets, peerings, network
//! security groups and their rules, route tables, NAT gateways, IP groups,
//! and load balancers with their backend pools.

use crate::collect::{BASE, Base, Batch};
use crate::id::{Ref, parent, rid, text};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::scan::{Edge, Resource};
use precip_types::{AccountId, Body, Region, body};
use serde::Deserialize;
use std::collections::BTreeMap;

const API: &str = "Microsoft.ResourceGraph/resources/read (network)";
const LB_API: &str = "Microsoft.ResourceGraph/resources/read (load balancers)";

pub(crate) struct Network;

impl Collect for Network {
    type Context = Batch;
    const API: &'static str = API;

    async fn collect(
        &self,
        ctx: &Batch,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let rows = Rows {
            vnets: ctx.graph(API, &vnet_query()).await?,
            nsgs: ctx.graph(API, &nsg_query()).await?,
            route_tables: ctx.graph(API, &route_table_query()).await?,
            nat_gateways: ctx.graph(API, &nat_query()).await?,
            ip_groups: ctx.graph(API, &ip_group_query()).await?,
        };
        Ok(rows.assemble())
    }
}

pub(crate) struct LoadBalancers;

impl Collect for LoadBalancers {
    type Context = Batch;
    const API: &'static str = LB_API;

    async fn collect(
        &self,
        ctx: &Batch,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let rows: Vec<LbRow> = ctx.graph(LB_API, &lb_query()).await?;
        Ok(load_balancers(&rows))
    }
}

fn vnet_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.network/virtualnetworks'
| project {BASE},
    addressPrefixes = properties.addressSpace.addressPrefixes,
    subnets = properties.subnets,
    peerings = properties.virtualNetworkPeerings"
    )
}

fn nsg_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.network/networksecuritygroups'
| project {BASE}, rules = properties.securityRules"
    )
}

fn route_table_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.network/routetables'
| project {BASE}, routes = properties.routes, subnets = properties.subnets"
    )
}

fn nat_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.network/natgateways'
| project {BASE},
    subnets = properties.subnets,
    publicIpAddresses = properties.publicIpAddresses,
    state = tostring(properties.provisioningState)"
    )
}

fn ip_group_query() -> String {
    format!(
        "resources
| where type =~ 'microsoft.network/ipgroups'
| project {BASE}, ipAddresses = properties.ipAddresses"
    )
}

fn lb_query() -> String {
    format!(
        "resources
| where type in~ ('microsoft.network/loadbalancers', 'microsoft.network/applicationgateways')
| project {BASE},
    type = tolower(type),
    frontends = properties.frontendIPConfigurations,
    pools = properties.backendAddressPools"
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct VnetRow {
    #[serde(flatten)]
    pub base: Base,
    pub address_prefixes: Vec<String>,
    pub subnets: Vec<SubnetJson>,
    pub peerings: Vec<PeeringJson>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct SubnetJson {
    pub id: String,
    pub properties: SubnetProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct SubnetProperties {
    pub address_prefix: String,
    pub address_prefixes: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PeeringJson {
    pub id: String,
    pub properties: PeeringProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PeeringProperties {
    pub peering_state: String,
    pub remote_virtual_network: Ref,
    pub remote_address_space: AddressSpace,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct AddressSpace {
    pub address_prefixes: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct NsgRow {
    #[serde(flatten)]
    pub base: Base,
    pub rules: Vec<RuleJson>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct RuleJson {
    pub id: String,
    pub properties: RuleProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct RuleProperties {
    pub direction: String,
    pub access: String,
    pub protocol: String,
    pub priority: Option<i32>,
    pub description: String,
    pub source_address_prefix: String,
    pub source_address_prefixes: Vec<String>,
    pub source_application_security_groups: Vec<Ref>,
    pub destination_address_prefix: String,
    pub destination_address_prefixes: Vec<String>,
    pub destination_application_security_groups: Vec<Ref>,
    pub destination_port_range: String,
    pub destination_port_ranges: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct RouteTableRow {
    #[serde(flatten)]
    pub base: Base,
    pub routes: Vec<RouteJson>,
    pub subnets: Vec<Ref>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct RouteJson {
    pub properties: RouteProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct RouteProperties {
    pub address_prefix: String,
    pub next_hop_type: String,
    pub next_hop_ip_address: String,
    pub provisioning_state: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct NatRow {
    #[serde(flatten)]
    pub base: Base,
    pub subnets: Vec<Ref>,
    pub public_ip_addresses: Vec<Ref>,
    pub state: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct IpGroupRow {
    #[serde(flatten)]
    pub base: Base,
    pub ip_addresses: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct LbRow {
    #[serde(flatten)]
    pub base: Base,
    #[serde(rename = "type")]
    pub kind: String,
    pub frontends: Vec<Frontend>,
    pub pools: Vec<PoolJson>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Frontend {
    pub properties: FrontendProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct FrontendProperties {
    #[serde(rename = "publicIPAddress")]
    pub public_ip_address: Option<Ref>,
    pub subnet: Option<Ref>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PoolJson {
    pub id: String,
    pub name: String,
    pub properties: PoolProperties,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PoolProperties {
    #[serde(rename = "backendIPConfigurations")]
    pub backend_ip_configurations: Vec<Ref>,
}

#[derive(Debug, Default)]
pub(crate) struct Rows {
    pub vnets: Vec<VnetRow>,
    pub nsgs: Vec<NsgRow>,
    pub route_tables: Vec<RouteTableRow>,
    pub nat_gateways: Vec<NatRow>,
    pub ip_groups: Vec<IpGroupRow>,
}

/// A resource nested in another (a subnet, a rule, a backend pool), which
/// takes its account, region and tags from its parent.
fn child(parent: &Base, id: &str, body: Body) -> Resource {
    Resource {
        arn: rid(id),
        account_id: AccountId::from(parent.subscription_id.to_lowercase()),
        region: Region::from(parent.location.to_lowercase()),
        tags: BTreeMap::new(),
        body,
    }
}

fn ids(refs: &[Ref]) -> Vec<String> {
    refs.iter().filter_map(Ref::rid).map(String::from).collect()
}

/// The single value, else the list: ARM fills one or the other.
fn one_or_many(one: &str, many: &[String]) -> Vec<String> {
    match text(one) {
        Some(v) => vec![v],
        None => many.to_vec(),
    }
}

/// A port range as `(from, to)`; `*` is every port and leaves both open.
fn ports(range: &str) -> (Option<i32>, Option<i32>) {
    match range.split_once('-') {
        Some((a, b)) => (a.trim().parse().ok(), b.trim().parse().ok()),
        None => {
            let p = range.trim().parse().ok();
            (p, p)
        }
    }
}

/// Where a rule's traffic comes from or goes to: an address range, or a
/// service tag (`Internet`, `VirtualNetwork`), which is a named prefix
/// list.
enum Peer {
    V4(String),
    V6(String),
    Tag(String),
    Group(String),
}

fn peer(prefix: &str) -> Peer {
    match prefix {
        "*" => Peer::V4("0.0.0.0/0".into()),
        p if p.contains(':') => Peer::V6(p.into()),
        p if p.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
            Peer::V4(p.into())
        }
        p => Peer::Tag(p.into()),
    }
}

/// One NSG rule as one stored rule per peer and port range, the way EC2
/// states rules.
fn rules(nsg: &Base, rule: &RuleJson) -> Vec<Resource> {
    let p = &rule.properties;
    let egress = p.direction.eq_ignore_ascii_case("outbound");
    let (prefix, many, groups) = if egress {
        (
            &p.destination_address_prefix,
            &p.destination_address_prefixes,
            &p.destination_application_security_groups,
        )
    } else {
        (
            &p.source_address_prefix,
            &p.source_address_prefixes,
            &p.source_application_security_groups,
        )
    };
    let mut peers: Vec<Peer> =
        one_or_many(prefix, many).iter().map(|p| peer(p)).collect();
    peers.extend(ids(groups).into_iter().map(Peer::Group));
    let ranges =
        one_or_many(&p.destination_port_range, &p.destination_port_ranges);
    let protocol = match p.protocol.to_ascii_lowercase().as_str() {
        "*" => "-1".to_string(),
        other => other.to_string(),
    };
    let base_id = rid(&rule.id).to_string();
    let count = peers.len() * ranges.len().max(1);
    let mut out = Vec::new();
    for peer in &peers {
        for range in ranges
            .iter()
            .map(String::as_str)
            .chain(ranges.is_empty().then_some("*"))
        {
            let id = if count == 1 {
                base_id.clone()
            } else {
                format!("{base_id}#{}", out.len() + 1)
            };
            let (from_port, to_port) = ports(range);
            let mut r = body::SecurityGroupRule {
                rule_id: id.clone(),
                group_id: Some(rid(&nsg.id).to_string()),
                egress: Some(egress),
                deny: p.access.eq_ignore_ascii_case("deny"),
                priority: p.priority,
                ip_protocol: Some(protocol.clone()),
                from_port,
                to_port,
                description: text(&p.description),
                ..Default::default()
            };
            match peer {
                Peer::V4(c) => r.cidr_ipv4 = Some(c.clone()),
                Peer::V6(c) => r.cidr_ipv6 = Some(c.clone()),
                Peer::Tag(t) => r.prefix_list_id = Some(t.clone()),
                Peer::Group(g) => r.referenced_group_id = Some(g.clone()),
            }
            out.push(child(nsg, &id, Body::SecurityGroupRule(r)));
        }
    }
    out
}

impl Rows {
    pub(crate) fn assemble(self) -> Collected {
        let mut out = Collected::default();
        for vnet in &self.vnets {
            let network_id = rid(&vnet.base.id).to_string();
            out.resources.push(vnet.base.resource(Body::Network(
                body::Network {
                    network_id: network_id.clone(),
                    name: Some(vnet.base.name.clone()),
                    cidr_block: vnet.address_prefixes.first().cloned(),
                    cidr_blocks: vnet.address_prefixes.clone(),
                    ..Default::default()
                },
            )));
            for s in &vnet.subnets {
                let prefixes = one_or_many(
                    &s.properties.address_prefix,
                    &s.properties.address_prefixes,
                );
                let (v6, v4): (Vec<String>, Vec<String>) =
                    prefixes.into_iter().partition(|p| p.contains(':'));
                out.resources.push(child(
                    &vnet.base,
                    &s.id,
                    Body::Subnet(body::Subnet {
                        subnet_id: rid(&s.id).to_string(),
                        network_id: Some(network_id.clone()),
                        cidr_block: v4.into_iter().next(),
                        ipv6_cidr_blocks: v6,
                        ..Default::default()
                    }),
                ));
            }
            for peering in &vnet.peerings {
                let pp = &peering.properties;
                out.resources.push(child(
                    &vnet.base,
                    &peering.id,
                    Body::PeeringConnection(body::PeeringConnection {
                        peering_id: rid(&peering.id).to_string(),
                        status: text(&pp.peering_state),
                        requester: body::PeeringSide {
                            network_id: Some(network_id.clone()),
                            owner_id: Some(
                                vnet.base.subscription_id.to_lowercase(),
                            ),
                            region: Some(vnet.base.location.to_lowercase()),
                            cidr_block: vnet.address_prefixes.first().cloned(),
                        },
                        accepter: body::PeeringSide {
                            network_id: pp
                                .remote_virtual_network
                                .rid()
                                .map(String::from),
                            owner_id: text(&pp.remote_virtual_network.id)
                                .and_then(|id| {
                                    id.to_lowercase()
                                        .strip_prefix("/subscriptions/")
                                        .and_then(|r| r.split('/').next())
                                        .map(str::to_string)
                                }),
                            region: None,
                            cidr_block: pp
                                .remote_address_space
                                .address_prefixes
                                .first()
                                .cloned(),
                        },
                    }),
                ));
            }
        }

        for nsg in &self.nsgs {
            let group = rid(&nsg.base.id);
            out.resources.push(nsg.base.resource(Body::SecurityGroup(
                body::SecurityGroup {
                    group_id: group.to_string(),
                    group_name: Some(nsg.base.name.clone()),
                    ..Default::default()
                },
            )));
            for rule in &nsg.rules {
                for r in rules(&nsg.base, rule) {
                    out.relate(r.arn.clone(), Edge::RuleOf, group.clone());
                    out.resources.push(r);
                }
            }
        }

        for rt in &self.route_tables {
            let subnets = ids(&rt.subnets);
            out.resources.push(
                rt.base.resource(Body::RouteTable(body::RouteTable {
                    route_table_id: rid(&rt.base.id).to_string(),
                    network_id: subnets
                        .first()
                        .and_then(|s| parent(s, "subnets")),
                    main: false,
                    subnets,
                    routes: rt
                        .routes
                        .iter()
                        .map(|r| body::Route {
                            destination: text(&r.properties.address_prefix),
                            target: text(&r.properties.next_hop_ip_address)
                                .or_else(|| text(&r.properties.next_hop_type)),
                            state: text(&r.properties.provisioning_state),
                        })
                        .collect(),
                })),
            );
        }

        for nat in &self.nat_gateways {
            let subnets = ids(&nat.subnets);
            out.resources.push(nat.base.resource(Body::NatGateway(
                body::NatGateway {
                    nat_gateway_id: rid(&nat.base.id).to_string(),
                    network_id:
                        subnets.first().and_then(|s| parent(s, "subnets")),
                    subnet_id: subnets.first().cloned(),
                    state: text(&nat.state),
                    connectivity: Some("public".into()),
                    public_ips: ids(&nat.public_ip_addresses),
                },
            )));
        }

        for g in &self.ip_groups {
            out.resources.push(g.base.resource(Body::PrefixList(
                body::PrefixList {
                    prefix_list_id: rid(&g.base.id).to_string(),
                    name: Some(g.base.name.clone()),
                    entries: g.ip_addresses.clone(),
                    ..Default::default()
                },
            )));
        }
        out
    }
}

/// Load balancers and application gateways, with one target group per
/// backend pool. Which instances sit in a pool is on their network
/// interfaces, so the compute collector relates them.
pub(crate) fn load_balancers(rows: &[LbRow]) -> Collected {
    let mut out = Collected::default();
    for lb in rows {
        let arn = rid(&lb.base.id);
        let public = lb
            .frontends
            .iter()
            .any(|f| f.properties.public_ip_address.is_some());
        let network_id = lb
            .frontends
            .iter()
            .find_map(|f| f.properties.subnet.as_ref().and_then(Ref::rid))
            .and_then(|s| parent(s.as_str(), "subnets"));
        let lb_type = if lb.kind.ends_with("applicationgateways") {
            "application_gateway"
        } else {
            "load_balancer"
        };
        out.resources.push(lb.base.resource(Body::LoadBalancer(
            body::LoadBalancer {
                name: Some(lb.base.name.clone()),
                lb_type: Some(lb_type.into()),
                scheme: Some(
                    if public { "internet-facing" } else { "internal" }.into(),
                ),
                network_id: network_id.clone(),
                dns_name: None,
                security_groups: Vec::new(),
            },
        )));
        for pool in &lb.pools {
            let pool_arn = rid(&pool.id);
            out.relate(pool_arn.clone(), Edge::AttachedTo, arn.clone());
            out.resources.push(child(
                &lb.base,
                &pool.id,
                Body::TargetGroup(body::TargetGroup {
                    name: text(&pool.name),
                    target_type: Some("ip_configuration".into()),
                    network_id: network_id.clone(),
                    load_balancers: vec![arn.to_string()],
                    instance_targets: pool
                        .properties
                        .backend_ip_configurations
                        .len(),
                    ..Default::default()
                }),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_types::Arn;
    use precip_types::scan::ResourceKind;

    fn find<'a>(out: &'a Collected, arn: &str) -> &'a Resource {
        out.resources
            .iter()
            .find(|r| r.arn == Arn::from(arn.to_lowercase()))
            .unwrap_or_else(|| panic!("no {arn}"))
    }

    const P: &str =
        "/subscriptions/S1/resourceGroups/Net/providers/Microsoft.Network";

    fn base(kind: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "id": format!("{P}/{kind}/{name}"),
            "name": name,
            "location": "westeurope",
            "subscriptionId": "S1",
            "tags": {"env": "prod"},
        })
    }

    fn with(
        mut v: serde_json::Value,
        extra: serde_json::Value,
    ) -> serde_json::Value {
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        v
    }

    fn row<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
        serde_json::from_value(v).unwrap()
    }

    fn rows() -> Rows {
        Rows {
            vnets: vec![row(with(
                base("virtualNetworks", "hub"),
                serde_json::json!({
                    "addressPrefixes": ["10.0.0.0/16"],
                    "subnets": [{"id": format!("{P}/virtualNetworks/hub/subnets/app"),
                                 "properties": {"addressPrefixes": ["10.0.1.0/24", "fd00::/64"]}}],
                    "peerings": [{"id": format!("{P}/virtualNetworks/hub/virtualNetworkPeerings/to-spoke"),
                                  "properties": {"peeringState": "Connected",
                                                 "remoteVirtualNetwork": {"id": "/subscriptions/S2/resourceGroups/x/providers/Microsoft.Network/virtualNetworks/spoke"},
                                                 "remoteAddressSpace": {"addressPrefixes": ["10.1.0.0/16"]}}}],
                }),
            ))],
            nsgs: vec![row(with(
                base("networkSecurityGroups", "web"),
                serde_json::json!({"rules": [
                    {"id": format!("{P}/networkSecurityGroups/web/securityRules/https"),
                     "properties": {"direction": "Inbound", "access": "Allow", "protocol": "Tcp",
                                    "priority": 100, "sourceAddressPrefix": "Internet",
                                    "destinationPortRange": "443"}},
                    {"id": format!("{P}/networkSecurityGroups/web/securityRules/admin"),
                     "properties": {"direction": "Inbound", "access": "Deny", "protocol": "*",
                                    "priority": 200,
                                    "sourceAddressPrefixes": ["203.0.113.0/24", "2001:db8::/32"],
                                    "destinationPortRanges": ["22", "3389-3390"]}},
                ]}),
            ))],
            route_tables: vec![row(with(
                base("routeTables", "rt"),
                serde_json::json!({
                    "routes": [{"properties": {"addressPrefix": "0.0.0.0/0", "nextHopType": "VirtualAppliance",
                                               "nextHopIpAddress": "10.0.0.4", "provisioningState": "Succeeded"}}],
                    "subnets": [{"id": format!("{P}/virtualNetworks/hub/subnets/app")}],
                }),
            ))],
            nat_gateways: vec![row(with(
                base("natGateways", "nat"),
                serde_json::json!({"subnets": [{"id": format!("{P}/virtualNetworks/hub/subnets/app")}],
                                   "publicIpAddresses": [{"id": format!("{P}/publicIPAddresses/nat-ip")}],
                                   "state": "Succeeded"}),
            ))],
            ip_groups: vec![row(with(
                base("ipGroups", "office"),
                serde_json::json!({"ipAddresses": ["198.51.100.0/24"]}),
            ))],
        }
    }

    #[test]
    fn networks_are_named_and_subnets_split_by_family() {
        let out = rows().assemble();
        let net = find(&out, &format!("{P}/virtualNetworks/hub"));
        assert_eq!(net.name(), Some("hub"));
        let sub = find(&out, &format!("{P}/virtualNetworks/hub/subnets/app"));
        let Body::Subnet(s) = &sub.body else { panic!() };
        assert_eq!(s.cidr_block.as_deref(), Some("10.0.1.0/24"));
        assert_eq!(s.ipv6_cidr_blocks, ["fd00::/64"]);
        assert_eq!(s.network_id.as_deref(), Some(net.arn.as_str()));
        assert!(
            sub.tags.is_empty(),
            "children do not copy their parent's tags"
        );
    }

    #[test]
    fn peerings_name_both_networks_and_the_remote_subscription() {
        let out = rows().assemble();
        let p = out.of_kind(ResourceKind::PeeringConnection).next().unwrap();
        let Body::PeeringConnection(p) = &p.body else { panic!() };
        assert_eq!(p.status.as_deref(), Some("Connected"));
        assert_eq!(p.accepter.owner_id.as_deref(), Some("s2"));
        assert_eq!(p.accepter.cidr_block.as_deref(), Some("10.1.0.0/16"));
    }

    #[test]
    fn nsg_rules_split_into_one_rule_per_peer_and_port_range() {
        let out = rows().assemble();
        let rules: Vec<&body::SecurityGroupRule> = out
            .of_kind(ResourceKind::SecurityGroupRule)
            .filter_map(|r| {
                if let Body::SecurityGroupRule(b) = &r.body {
                    Some(b)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(rules.len(), 5, "one https rule, four admin rules");
        let https = rules[0];
        assert_eq!(https.prefix_list_id.as_deref(), Some("Internet"));
        assert_eq!((https.from_port, https.to_port), (Some(443), Some(443)));
        assert_eq!(https.ip_protocol.as_deref(), Some("tcp"));
        assert!(!https.deny);
        let admin = &rules[1..];
        assert!(
            admin
                .iter()
                .all(|r| r.deny && r.ip_protocol.as_deref() == Some("-1"))
        );
        assert_eq!(admin[1].from_port, Some(3389));
        assert_eq!(admin[1].to_port, Some(3390));
        assert_eq!(admin[2].cidr_ipv6.as_deref(), Some("2001:db8::/32"));
        let ids: std::collections::BTreeSet<&str> =
            admin.iter().map(|r| r.rule_id.as_str()).collect();
        assert_eq!(ids.len(), 4, "split rules get distinct ids");
        assert_eq!(
            out.relationships.iter().filter(|r| r.edge == Edge::RuleOf).count(),
            5
        );
    }

    #[test]
    fn route_tables_and_nat_gateways_find_their_network_through_a_subnet() {
        let out = rows().assemble();
        let hub = rid(&format!("{P}/virtualNetworks/hub")).to_string();
        let Body::RouteTable(rt) =
            &out.of_kind(ResourceKind::RouteTable).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(rt.network_id.as_deref(), Some(hub.as_str()));
        assert_eq!(rt.routes[0].target.as_deref(), Some("10.0.0.4"));
        let Body::NatGateway(n) =
            &out.of_kind(ResourceKind::NatGateway).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(n.network_id.as_deref(), Some(hub.as_str()));
        let Body::PrefixList(g) =
            &out.of_kind(ResourceKind::PrefixList).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(g.entries, ["198.51.100.0/24"]);
    }

    #[test]
    fn backend_pools_become_target_groups_attached_to_their_balancer() {
        let row: LbRow = serde_json::from_value(with(
            base("applicationGateways", "shop-agw"),
            serde_json::json!({
                "type": "microsoft.network/applicationgateways",
                "frontends": [{"properties": {"publicIPAddress": {"id": format!("{P}/publicIPAddresses/agw-ip")}}},
                              {"properties": {"subnet": {"id": format!("{P}/virtualNetworks/hub/subnets/agw")}}}],
                "pools": [{"id": format!("{P}/applicationGateways/shop-agw/backendAddressPools/web"),
                           "name": "web",
                           "properties": {"backendIPConfigurations": [{"id": "a"}, {"id": "b"}]}}],
            }),
        ))
        .unwrap();
        let out = load_balancers(&[row]);
        let lb = find(&out, &format!("{P}/applicationGateways/shop-agw"));
        let Body::LoadBalancer(b) = &lb.body else { panic!() };
        assert_eq!(b.lb_type.as_deref(), Some("application_gateway"));
        assert_eq!(b.scheme.as_deref(), Some("internet-facing"));
        assert_eq!(
            b.network_id.as_deref(),
            Some(rid(&format!("{P}/virtualNetworks/hub")).as_str())
        );
        let pool = find(
            &out,
            &format!(
                "{P}/applicationGateways/shop-agw/backendAddressPools/web"
            ),
        );
        let Body::TargetGroup(t) = &pool.body else { panic!() };
        assert_eq!(t.instance_targets, 2);
        assert_eq!(out.relationships[0].edge, Edge::AttachedTo);
        assert_eq!(out.relationships[0].to_arn, lb.arn);
    }

    #[test]
    fn port_ranges_and_peers_parse() {
        assert_eq!(ports("*"), (None, None));
        assert_eq!(ports("80"), (Some(80), Some(80)));
        assert_eq!(ports("1000 - 2000"), (Some(1000), Some(2000)));
        assert!(matches!(peer("*"), Peer::V4(c) if c == "0.0.0.0/0"));
        assert!(matches!(peer("10.0.0.0/8"), Peer::V4(_)));
        assert!(matches!(peer("VirtualNetwork"), Peer::Tag(_)));
    }
}
