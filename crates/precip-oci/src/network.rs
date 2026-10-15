//! Virtual cloud networks and what hangs off them: subnets, security
//! lists and network security groups with their rules, route tables, and
//! internet and NAT gateways.
//!
//! Security lists and network security groups are both security groups:
//! a list applies to the instances in its subnets, a group to the VNICs
//! that name it. OCI rules only allow and are unordered.

use crate::api::{PAGE, Rows, Service};
use crate::collect::{Pair, is_gone, num_at, str_at, strings};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::body::{
    Peer, Protocol, RouteTarget, RuleAction, TrafficDirection,
};
use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};
use serde_json::Value;
use std::collections::HashMap;

const API: &str = "core (networking)";

pub(crate) struct Network;

impl Collect for Network {
    type Context = Pair;
    const API: &'static str = API;

    async fn collect(
        &self,
        ctx: &Pair,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let list = |path| async move {
            ctx.list::<Value>(path, &[]).await.map_err(CollectError::new(API))
        };
        let mut l = Listed {
            vcns: list("/20160918/vcns").await?,
            subnets: list("/20160918/subnets").await?,
            security_lists: list("/20160918/securityLists").await?,
            nsgs: list("/20160918/networkSecurityGroups").await?,
            route_tables: list("/20160918/routeTables").await?,
            internet_gateways: list("/20160918/internetGateways").await?,
            nat_gateways: list("/20160918/natGateways").await?,
            nsg_rules: HashMap::new(),
        };
        for nsg in l.nsgs.iter().filter(|n| !is_gone(n)) {
            let Some(id) = str_at(nsg, "/id") else { continue };
            let url = ctx
                .oci
                .url(
                    Service::Core,
                    &ctx.region,
                    &format!(
                        "/20160918/networkSecurityGroups/{id}/securityRules"
                    ),
                    &[("limit", PAGE)],
                )
                .map_err(CollectError::new(API))?;
            let rules = ctx
                .oci
                .list(url, None, Rows::Array)
                .await
                .map_err(CollectError::new(API))?;
            l.nsg_rules.insert(id, rules);
        }
        Ok(l.assemble(ctx))
    }
}

/// What one compartment and region listed.
#[derive(Debug, Default)]
pub(crate) struct Listed {
    pub vcns: Vec<Value>,
    pub subnets: Vec<Value>,
    pub security_lists: Vec<Value>,
    pub nsgs: Vec<Value>,
    /// Each network security group's rules, by group id.
    pub nsg_rules: HashMap<String, Vec<Value>>,
    pub route_tables: Vec<Value>,
    pub internet_gateways: Vec<Value>,
    pub nat_gateways: Vec<Value>,
}

/// A rule's TCP or UDP destination port range.
fn ports(rule: &Value, protocol: &Protocol) -> Option<(i32, i32)> {
    let options = match protocol {
        Protocol::Tcp => "/tcpOptions/destinationPortRange",
        Protocol::Udp => "/udpOptions/destinationPortRange",
        Protocol::All
        | Protocol::Icmp
        | Protocol::Icmp6
        | Protocol::Other(_) => return None,
    };
    let range = rule.pointer(options)?;
    let n = |k: &str| num_at(range, k).map(|v| v as i32);
    n("/min").zip(n("/max"))
}

/// One OCI rule in Oxide's terms. `direction` is given for a security
/// list's rules, which sit in separate ingress and egress lists.
fn rule(
    group: &str,
    v: &Value,
    direction: TrafficDirection,
) -> Option<body::SecurityGroupRule> {
    let (address, kind) = match direction {
        TrafficDirection::Inbound => ("/source", "/sourceType"),
        TrafficDirection::Outbound => ("/destination", "/destinationType"),
    };
    let address = str_at(v, address)?;
    let peer = match str_at(v, kind).as_deref() {
        Some("SERVICE_CIDR_BLOCK") => Peer::ServiceTag(address),
        Some("NETWORK_SECURITY_GROUP") => Peer::Group(address),
        Some(_) | None => Peer::IpNet(address),
    };
    let protocol = Protocol::parse(
        &str_at(v, "/protocol").unwrap_or_else(|| "all".into()),
    );
    let icmp = num_at(v, "/icmpOptions/type")
        .map(|t| (t as i32, num_at(v, "/icmpOptions/code").map(|c| c as i32)));
    Some(body::SecurityGroupRule {
        group: group.to_string(),
        direction,
        action: RuleAction::Allow,
        priority: None,
        ports: ports(v, &protocol),
        protocol,
        icmp,
        peer,
        description: str_at(v, "/description"),
    })
}

/// Where a route rule sends traffic, by the kind of its target's OCID. A
/// private IP (an appliance), a dynamic routing gateway or a service
/// gateway has no Oxide equivalent.
fn route_target(entity: &str) -> RouteTarget {
    let id = entity.to_string();
    if entity.starts_with("ocid1.internetgateway.") {
        RouteTarget::InternetGateway(id)
    } else if entity.starts_with("ocid1.natgateway.") {
        RouteTarget::NatGateway(id)
    } else if entity.starts_with("ocid1.localpeeringgateway.") {
        RouteTarget::Peering(id)
    } else {
        RouteTarget::Other(id)
    }
}

impl Listed {
    pub(crate) fn assemble(&self, ctx: &Pair) -> Collected {
        let mut out = Collected::default();
        let live = |rows: &'_ [Value]| -> Vec<Value> {
            rows.iter().filter(|r| !is_gone(r)).cloned().collect()
        };
        let mut default_tables = HashMap::new();
        for v in live(&self.vcns) {
            let Some(id) = str_at(&v, "/id") else { continue };
            if let Some(rt) = str_at(&v, "/defaultRouteTableId") {
                default_tables.insert(rt, id.clone());
            }
            let mut blocks = strings(&v, "/cidrBlocks");
            if blocks.is_empty() {
                blocks.extend(str_at(&v, "/cidrBlock"));
            }
            out.resources.push(ctx.resource(
                &id,
                &v,
                Body::Network(body::Network {
                    name: str_at(&v, "/displayName"),
                    cidr_block: blocks.first().cloned(),
                    cidr_blocks: blocks,
                    is_default: Some(false),
                    owner_id: Some(ctx.compartment.clone()),
                }),
            ));
        }

        let mut subnets_of_table: HashMap<String, Vec<String>> = HashMap::new();
        for s in live(&self.subnets) {
            let Some(id) = str_at(&s, "/id") else { continue };
            if let Some(rt) = str_at(&s, "/routeTableId") {
                subnets_of_table.entry(rt).or_default().push(id.clone());
            }
            let mut ipv6 = strings(&s, "/ipv6CidrBlocks");
            if ipv6.is_empty() {
                ipv6.extend(str_at(&s, "/ipv6CidrBlock"));
            }
            out.resources.push(
                ctx.resource(
                    &id,
                    &s,
                    Body::Subnet(body::Subnet {
                        name: str_at(&s, "/displayName"),
                        network_id: str_at(&s, "/vcnId"),
                        cidr_block: str_at(&s, "/cidrBlock"),
                        ipv6_cidr_blocks: ipv6,
                        // Regional subnets have no availability domain.
                        availability_zone: str_at(&s, "/availabilityDomain"),
                        map_public_ip_on_launch: s
                            .get("prohibitPublicIpOnVnic")
                            .and_then(Value::as_bool)
                            .map(|p| !p),
                    }),
                ),
            );
        }

        // Security lists hold their rules; a list's rules have no ids of
        // their own.
        for list in live(&self.security_lists) {
            let Some(id) = str_at(&list, "/id") else { continue };
            self.group(ctx, &mut out, &id, &list);
            for (key, direction, tag) in [
                ("/ingressSecurityRules", TrafficDirection::Inbound, "in"),
                ("/egressSecurityRules", TrafficDirection::Outbound, "out"),
            ] {
                let rules = list.pointer(key).and_then(Value::as_array);
                for (n, v) in rules.into_iter().flatten().enumerate() {
                    let Some(r) = rule(&id, v, direction) else { continue };
                    let rule_id = format!("{id}#{tag}{}", n + 1);
                    out.relate(
                        Arn::from(rule_id.as_str()),
                        Edge::RuleOf,
                        Arn::from(id.as_str()),
                    );
                    out.resources.push(ctx.resource(
                        &rule_id,
                        &Value::Null,
                        Body::SecurityGroupRule(r),
                    ));
                }
            }
        }
        for nsg in live(&self.nsgs) {
            let Some(id) = str_at(&nsg, "/id") else { continue };
            self.group(ctx, &mut out, &id, &nsg);
            for v in self.nsg_rules.get(&id).into_iter().flatten() {
                let direction =
                    if str_at(v, "/direction").as_deref() == Some("EGRESS") {
                        TrafficDirection::Outbound
                    } else {
                        TrafficDirection::Inbound
                    };
                let (Some(rule_id), Some(r)) =
                    (str_at(v, "/id"), rule(&id, v, direction))
                else {
                    continue;
                };
                let rule_id = format!("{id}#{rule_id}");
                out.relate(
                    Arn::from(rule_id.as_str()),
                    Edge::RuleOf,
                    Arn::from(id.as_str()),
                );
                out.resources.push(ctx.resource(
                    &rule_id,
                    &Value::Null,
                    Body::SecurityGroupRule(r),
                ));
            }
        }

        for rt in live(&self.route_tables) {
            let Some(id) = str_at(&rt, "/id") else { continue };
            let routes = rt
                .get("routeRules")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|r| body::Route {
                    destination: str_at(r, "/destination"),
                    target: str_at(r, "/networkEntityId")
                        .map_or(RouteTarget::Local, |e| route_target(&e)),
                    state: None,
                })
                .collect();
            out.resources.push(ctx.resource(
                &id,
                &rt,
                Body::RouteTable(body::RouteTable {
                    name: str_at(&rt, "/displayName"),
                    network_id: str_at(&rt, "/vcnId"),
                    main: default_tables.contains_key(&id),
                    subnets:
                        subnets_of_table.get(&id).cloned().unwrap_or_default(),
                    routes,
                }),
            ));
        }

        for g in live(&self.internet_gateways) {
            let Some(id) = str_at(&g, "/id") else { continue };
            out.resources.push(ctx.resource(
                &id,
                &g,
                Body::InternetGateway(body::InternetGateway {
                    name: str_at(&g, "/displayName"),
                    networks: str_at(&g, "/vcnId").into_iter().collect(),
                }),
            ));
        }
        for n in live(&self.nat_gateways) {
            let Some(id) = str_at(&n, "/id") else { continue };
            out.resources.push(ctx.resource(
                &id,
                &n,
                Body::NatGateway(body::NatGateway {
                    name: str_at(&n, "/displayName"),
                    network_id: str_at(&n, "/vcnId"),
                    subnet_id: None,
                    state:
                        str_at(&n, "/lifecycleState").map(|s| s.to_lowercase()),
                    connectivity: Some("public".into()),
                    public_ips: str_at(&n, "/publicIpId").into_iter().collect(),
                }),
            ));
        }
        out
    }

    fn group(&self, ctx: &Pair, out: &mut Collected, id: &str, v: &Value) {
        out.resources.push(ctx.resource(
            id,
            v,
            Body::SecurityGroup(body::SecurityGroup {
                name: str_at(v, "/displayName"),
                description: None,
                network_id: str_at(v, "/vcnId"),
            }),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Oci;
    use precip_collect::http::fake::Script;
    use precip_types::scan::{Resource, ResourceKind};
    use std::sync::Arc;

    fn ctx() -> Pair {
        Pair {
            oci: Oci::fake(Arc::new(Script::default())),
            region: "us-ashburn-1".into(),
            compartment: "c.net".into(),
            domains: Vec::new(),
        }
    }

    fn listed() -> Listed {
        let j = |v: serde_json::Value| v;
        Listed {
            vcns: vec![
                j(
                    serde_json::json!({"id": "vcn.prod", "displayName": "shop-vcn", "defaultRouteTableId": "rt.main",
                                     "cidrBlocks": ["10.0.0.0/16", "10.1.0.0/16"], "lifecycleState": "AVAILABLE"}),
                ),
                j(
                    serde_json::json!({"id": "vcn.old", "cidrBlock": "10.9.0.0/16", "lifecycleState": "TERMINATED"}),
                ),
            ],
            subnets: vec![j(serde_json::json!({
                "id": "s.web", "vcnId": "vcn.prod", "cidrBlock": "10.0.1.0/24", "routeTableId": "rt.main",
                "ipv6CidrBlock": "2603:c020::/64", "prohibitPublicIpOnVnic": true,
            }))],
            security_lists: vec![j(serde_json::json!({
                "id": "sl.web", "displayName": "web", "vcnId": "vcn.prod",
                "ingressSecurityRules": [
                    {"protocol": "6", "source": "0.0.0.0/0", "sourceType": "CIDR_BLOCK",
                     "tcpOptions": {"destinationPortRange": {"min": 443, "max": 443}}},
                    {"protocol": "1", "source": "10.0.0.0/8", "icmpOptions": {"type": 3, "code": 4}},
                ],
                "egressSecurityRules": [
                    {"protocol": "all", "destination": "all-iad-services-in-oracle-services-network",
                     "destinationType": "SERVICE_CIDR_BLOCK"},
                ],
            }))],
            nsgs: vec![j(
                serde_json::json!({"id": "nsg.db", "displayName": "db", "vcnId": "vcn.prod"}),
            )],
            nsg_rules: HashMap::from([(
                "nsg.db".to_string(),
                vec![j(serde_json::json!({
                    "id": "r1", "direction": "INGRESS", "protocol": "6",
                    "source": "nsg.web", "sourceType": "NETWORK_SECURITY_GROUP",
                    "tcpOptions": {"destinationPortRange": {"min": 5432, "max": 5432}},
                }))],
            )]),
            route_tables: vec![j(serde_json::json!({
                "id": "rt.main", "vcnId": "vcn.prod", "displayName": "main",
                "routeRules": [
                    {"destination": "0.0.0.0/0", "networkEntityId": "ocid1.internetgateway.oc1..igw"},
                    {"destination": "10.9.0.0/16", "networkEntityId": "ocid1.localpeeringgateway.oc1..lpg"},
                    {"destination": "192.168.0.0/16", "networkEntityId": "ocid1.drg.oc1..drg"},
                ],
            }))],
            internet_gateways: vec![j(
                serde_json::json!({"id": "ocid1.internetgateway.oc1..igw", "vcnId": "vcn.prod"}),
            )],
            nat_gateways: vec![j(
                serde_json::json!({"id": "nat.1", "vcnId": "vcn.prod", "publicIpId": "ip.nat"}),
            )],
        }
    }

    fn rules(out: &Collected) -> Vec<(&str, &body::SecurityGroupRule)> {
        out.resources
            .iter()
            .filter_map(|r: &Resource| {
                if let Body::SecurityGroupRule(b) = &r.body {
                    Some((r.arn.as_str(), b))
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn vcns_keep_their_name_and_blocks_and_subnets_their_vcn() {
        let out = listed().assemble(&ctx());
        let net = out.of_kind(ResourceKind::Network).next().unwrap();
        assert_eq!(net.name(), Some("shop-vcn"));
        let Body::Network(n) = &net.body else { panic!() };
        assert_eq!(n.cidr_blocks.len(), 2);
        assert_eq!(
            out.of_kind(ResourceKind::Network).count(),
            1,
            "terminated VCNs are left out"
        );
        let Body::Subnet(s) =
            &out.of_kind(ResourceKind::Subnet).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(s.network_id.as_deref(), Some("vcn.prod"));
        assert_eq!(s.ipv6_cidr_blocks, ["2603:c020::/64"]);
        assert_eq!(s.map_public_ip_on_launch, Some(false));
    }

    #[test]
    fn security_lists_and_groups_become_groups_with_typed_rules() {
        let out = listed().assemble(&ctx());
        assert_eq!(out.of_kind(ResourceKind::SecurityGroup).count(), 2);
        let rules = rules(&out);
        assert_eq!(rules.len(), 4);
        let (id, https) = rules[0];
        assert_eq!(id, "sl.web#in1");
        assert_eq!(
            (https.protocol.clone(), https.ports),
            (Protocol::Tcp, Some((443, 443)))
        );
        assert_eq!(https.peer, Peer::IpNet("0.0.0.0/0".into()));
        let (_, icmp) = rules[1];
        assert_eq!(
            (icmp.protocol.clone(), icmp.icmp),
            (Protocol::Icmp, Some((3, Some(4))))
        );
        let (id, services) = rules[2];
        assert_eq!(id, "sl.web#out1");
        assert_eq!(services.direction, TrafficDirection::Outbound);
        assert_eq!(
            services.peer,
            Peer::ServiceTag(
                "all-iad-services-in-oracle-services-network".into()
            )
        );
        let (id, pg) = rules[3];
        assert_eq!(id, "nsg.db#r1");
        assert_eq!(pg.peer, Peer::Group("nsg.web".into()));
        assert_eq!(pg.group, "nsg.db");
        assert!(rules.iter().all(
            |(_, r)| r.action == RuleAction::Allow && r.priority.is_none()
        ));
        assert_eq!(
            out.relationships.iter().filter(|r| r.edge == Edge::RuleOf).count(),
            4
        );
    }

    #[test]
    fn route_tables_know_their_subnets_and_typed_targets() {
        let out = listed().assemble(&ctx());
        let Body::RouteTable(rt) =
            &out.of_kind(ResourceKind::RouteTable).next().unwrap().body
        else {
            panic!()
        };
        assert!(rt.main);
        assert_eq!(rt.subnets, ["s.web"]);
        assert_eq!(
            rt.routes.iter().map(|r| r.target.clone()).collect::<Vec<_>>(),
            [
                RouteTarget::InternetGateway(
                    "ocid1.internetgateway.oc1..igw".into()
                ),
                RouteTarget::Peering(
                    "ocid1.localpeeringgateway.oc1..lpg".into()
                ),
                RouteTarget::Other("ocid1.drg.oc1..drg".into()),
            ]
        );
        let Body::InternetGateway(g) =
            &out.of_kind(ResourceKind::InternetGateway).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(g.networks, ["vcn.prod"]);
        let Body::NatGateway(n) =
            &out.of_kind(ResourceKind::NatGateway).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(n.public_ips, ["ip.nat"]);
    }

    #[test]
    fn a_rule_without_an_address_is_skipped() {
        assert!(
            rule(
                "g",
                &serde_json::json!({"protocol": "6"}),
                TrafficDirection::Inbound
            )
            .is_none()
        );
        assert_eq!(ports(&serde_json::json!({}), &Protocol::Tcp), None);
        assert_eq!(ports(&serde_json::json!({}), &Protocol::All), None);
    }
}
