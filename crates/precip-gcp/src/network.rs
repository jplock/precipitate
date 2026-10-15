//! VPC networks and what hangs off them: subnets, peerings, firewall rules,
//! routes and Cloud NAT.
//!
//! A firewall rule is stored as a security group of its own with one rule
//! per peer and port range, and applies to the instances on its network
//! that carry one of its target tags or service accounts, or to all of
//! them when it names neither.

use crate::assets::{
    Asset, FIREWALL, INSTANCE, NETWORK, ROUTE, ROUTER, SUBNETWORK,
};
use crate::collect::Batch;
use crate::id::{i32_at, items_at, name, project, rid, str_at, strs_at};
use precip_collect::{Collect, CollectError, Collected};
use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const API: &str = "cloudasset.assets.listResource (network)";
/// The priority Compute Engine gives a firewall rule that sets none.
const DEFAULT_PRIORITY: i32 = 1000;

pub(crate) struct Network;

impl Collect for Network {
    type Context = Batch;
    const API: &'static str = API;

    async fn collect(
        &self,
        ctx: &Batch,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        Ok(assemble(ctx))
    }
}

/// A port range as `(from, to)`; none is every port.
fn ports(range: &str) -> (Option<i32>, Option<i32>) {
    match range.split_once('-') {
        Some((a, b)) => (a.trim().parse().ok(), b.trim().parse().ok()),
        None => {
            let p = range.trim().parse().ok();
            (p, p)
        }
    }
}

/// Where a rule's traffic comes from or goes to.
enum Peer {
    V4(String),
    V6(String),
    /// Instances with a network tag or service account, named
    /// `tag:<tag>` or `serviceAccount:<email>`.
    Instances(String),
}

fn peers(d: &Value, egress: bool) -> Vec<Peer> {
    let range = |r: &&str| {
        if r.contains(':') {
            Peer::V6((*r).to_string())
        } else {
            Peer::V4((*r).to_string())
        }
    };
    if egress {
        let ranges = strs_at(d, "/destinationRanges");
        if ranges.is_empty() {
            return vec![Peer::V4("0.0.0.0/0".into())];
        }
        return ranges.iter().map(range).collect();
    }
    let mut out: Vec<Peer> =
        strs_at(d, "/sourceRanges").iter().map(range).collect();
    out.extend(
        strs_at(d, "/sourceTags")
            .iter()
            .map(|t| Peer::Instances(format!("tag:{t}"))),
    );
    out.extend(
        strs_at(d, "/sourceServiceAccounts")
            .iter()
            .map(|s| Peer::Instances(format!("serviceAccount:{s}"))),
    );
    out
}

/// One firewall as one stored rule per protocol entry, port range and
/// peer, the way EC2 states rules.
fn rules(firewall: &Arn, d: &Value) -> Vec<body::SecurityGroupRule> {
    let egress = str_at(d, "/direction").as_deref() == Some("EGRESS");
    let peers = peers(d, egress);
    let entries: Vec<(bool, &Value)> = items_at(d, "/allowed")
        .iter()
        .map(|e| (false, e))
        .chain(items_at(d, "/denied").iter().map(|e| (true, e)))
        .collect();
    let mut expanded = Vec::new();
    for (deny, entry) in &entries {
        let protocol = match str_at(entry, "/IPProtocol").as_deref() {
            None | Some("all") => "-1".to_string(),
            Some(p) => p.to_lowercase(),
        };
        let ranges = strs_at(entry, "/ports");
        let ranges = if ranges.is_empty() { vec![""] } else { ranges };
        for range in ranges {
            for peer in &peers {
                expanded.push((*deny, protocol.clone(), range, peer));
            }
        }
    }
    let count = expanded.len();
    let mut out = Vec::new();
    for (deny, protocol, range, peer) in expanded {
        let rule_id = if count == 1 {
            firewall.to_string()
        } else {
            format!("{firewall}#{}", out.len() + 1)
        };
        let (from_port, to_port) = ports(range);
        let mut r = body::SecurityGroupRule {
            rule_id,
            group_id: Some(firewall.to_string()),
            egress: Some(egress),
            deny,
            priority: i32_at(d, "/priority").or(Some(DEFAULT_PRIORITY)),
            ip_protocol: Some(protocol),
            from_port,
            to_port,
            description: str_at(d, "/description"),
            ..Default::default()
        };
        match peer {
            Peer::V4(c) => r.cidr_ipv4 = Some(c.clone()),
            Peer::V6(c) => r.cidr_ipv6 = Some(c.clone()),
            Peer::Instances(g) => r.referenced_group_id = Some(g.clone()),
        }
        out.push(r);
    }
    out
}

/// What a firewall can match on an instance.
struct Target {
    arn: Arn,
    networks: BTreeSet<Arn>,
    tags: BTreeSet<String>,
    service_accounts: BTreeSet<String>,
}

fn targets(ctx: &Batch) -> Vec<Target> {
    ctx.inventory
        .of(INSTANCE)
        .iter()
        .map(|vm| {
            let d = &vm.resource.data;
            Target {
                arn: rid(&vm.name),
                networks: items_at(d, "/networkInterfaces")
                    .iter()
                    .filter_map(|n| str_at(n, "/network"))
                    .map(|n| rid(&n))
                    .collect(),
                tags: strs_at(d, "/tags/items")
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                service_accounts: items_at(d, "/serviceAccounts")
                    .iter()
                    .filter_map(|s| str_at(s, "/email"))
                    .collect(),
            }
        })
        .collect()
}

fn applies(firewall: &Value, network: &Arn, t: &Target) -> bool {
    if !t.networks.contains(network) {
        return false;
    }
    let tags = strs_at(firewall, "/targetTags");
    let accounts = strs_at(firewall, "/targetServiceAccounts");
    (tags.is_empty() && accounts.is_empty())
        || tags.iter().any(|x| t.tags.contains(*x))
        || accounts.iter().any(|x| t.service_accounts.contains(*x))
}

/// A route's next hop: an address or peering as given, else the name of
/// the gateway, instance, load balancer or tunnel.
fn next_hop(d: &Value) -> Option<String> {
    str_at(d, "/nextHopIp").or_else(|| str_at(d, "/nextHopPeering")).or_else(
        || {
            [
                "/nextHopGateway",
                "/nextHopInstance",
                "/nextHopIlb",
                "/nextHopVpnTunnel",
                "/nextHopHub",
                "/nextHopNetwork",
            ]
            .iter()
            .find_map(|p| str_at(d, p))
            .map(|h| name(&h).to_string())
        },
    )
}

pub(crate) fn assemble(ctx: &Batch) -> Collected {
    let inv = &ctx.inventory;
    let mut out = Collected::default();

    let mut subnets_of: BTreeMap<Arn, Vec<String>> = BTreeMap::new();
    for s in inv.of(SUBNETWORK) {
        let d = &s.resource.data;
        let network = str_at(d, "/network").map(|n| rid(&n));
        if let Some(n) = &network {
            subnets_of
                .entry(n.clone())
                .or_default()
                .push(rid(&s.name).to_string());
        }
        let ipv6: Vec<String> =
            ["/ipv6CidrRange", "/internalIpv6Prefix", "/externalIpv6Prefix"]
                .iter()
                .filter_map(|p| str_at(d, p))
                .collect();
        out.resources.push(ctx.resource(
            s,
            Body::Subnet(body::Subnet {
                subnet_id: rid(&s.name).to_string(),
                network_id: network.map(String::from),
                cidr_block: str_at(d, "/ipCidrRange"),
                ipv6_cidr_blocks: ipv6,
                ..Default::default()
            }),
        ));
    }

    for net in inv.of(NETWORK) {
        let d = &net.resource.data;
        let network_id = rid(&net.name).to_string();
        let own = name(&net.name).to_string();
        let legacy = str_at(d, "/IPv4Range");
        out.resources.push(ctx.resource(
            net,
            Body::Network(body::Network {
                network_id: network_id.clone(),
                cidr_block: legacy.clone(),
                cidr_blocks: legacy.into_iter().collect(),
                is_default: Some(own == "default"),
                owner_id: Some(ctx.account(net).to_string()),
                name: Some(own),
            }),
        ));
        for peering in items_at(d, "/peerings") {
            let Some(peer_name) = str_at(peering, "/name") else { continue };
            let remote = str_at(peering, "/network").map(|n| rid(&n));
            let id = format!("{network_id}/peerings/{peer_name}");
            out.resources.push(
                ctx.child(
                    net,
                    &id,
                    Body::PeeringConnection(body::PeeringConnection {
                        peering_id: id.clone(),
                        status: str_at(peering, "/state"),
                        requester: body::PeeringSide {
                            network_id: Some(network_id.clone()),
                            owner_id: Some(ctx.account(net).to_string()),
                            ..Default::default()
                        },
                        accepter: body::PeeringSide {
                            owner_id: remote
                                .as_ref()
                                .and_then(|r| project(r.as_str()))
                                .map(str::to_string),
                            network_id: remote.map(String::from),
                            ..Default::default()
                        },
                    }),
                ),
            );
        }
    }

    let instances = targets(ctx);
    for fw in inv.of(FIREWALL) {
        let d = &fw.resource.data;
        let group = rid(&fw.name);
        let network = str_at(d, "/network").map(|n| rid(&n));
        out.resources.push(ctx.resource(
            fw,
            Body::SecurityGroup(body::SecurityGroup {
                group_id: group.to_string(),
                group_name: Some(name(&fw.name).to_string()),
                description: str_at(d, "/description"),
                network_id: network.as_ref().map(ToString::to_string),
            }),
        ));
        // A disabled rule matches nothing.
        if d.get("disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        for r in rules(&group, d) {
            let arn = Arn::from(r.rule_id.as_str());
            out.relate(arn.clone(), Edge::RuleOf, group.clone());
            out.resources.push(ctx.child(
                fw,
                arn.as_str(),
                Body::SecurityGroupRule(r),
            ));
        }
        let Some(network) = network else { continue };
        for t in instances.iter().filter(|t| applies(d, &network, t)) {
            out.relate(t.arn.clone(), Edge::HasSecurityGroup, group.clone());
        }
    }

    // Routes belong to a network, not a table: each network's routes are
    // one table.
    let mut routes: BTreeMap<Arn, Vec<&Asset>> = BTreeMap::new();
    for r in inv.of(ROUTE) {
        if let Some(n) = str_at(&r.resource.data, "/network") {
            routes.entry(rid(&n)).or_default().push(r);
        }
    }
    for (network, members) in routes {
        let Some(first) = members.first() else { continue };
        let id = format!("{network}/routes");
        out.resources.push(
            ctx.child(
                first,
                &id,
                Body::RouteTable(body::RouteTable {
                    route_table_id: id.clone(),
                    network_id: Some(network.to_string()),
                    main: true,
                    subnets: subnets_of
                        .get(&network)
                        .cloned()
                        .unwrap_or_default(),
                    routes: members
                        .iter()
                        .map(|r| body::Route {
                            destination: str_at(&r.resource.data, "/destRange"),
                            target: next_hop(&r.resource.data),
                            state: None,
                        })
                        .collect(),
                }),
            ),
        );
    }

    for router in inv.of(ROUTER) {
        let d = &router.resource.data;
        let network = str_at(d, "/network").map(|n| rid(&n).to_string());
        for nat in items_at(d, "/nats") {
            let Some(nat_name) = str_at(nat, "/name") else { continue };
            let id = format!("{}/nats/{nat_name}", rid(&router.name));
            out.resources.push(
                ctx.child(
                    router,
                    &id,
                    Body::NatGateway(body::NatGateway {
                        nat_gateway_id: id.clone(),
                        network_id: network.clone(),
                        subnet_id: items_at(nat, "/subnetworks")
                            .iter()
                            .find_map(|s| str_at(s, "/name"))
                            .map(|s| rid(&s).to_string()),
                        state: None,
                        connectivity: Some("public".into()),
                        public_ips: strs_at(nat, "/natIps")
                            .iter()
                            .map(|i| rid(i).to_string())
                            .collect(),
                    }),
                ),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::test::{asset, batch};
    use precip_types::scan::{Resource, ResourceKind};

    const P: &str = "//compute.googleapis.com/projects/shop";
    const SELF: &str = "https://www.googleapis.com/compute/v1/projects/shop";

    fn vm(name: &str, tags: &[&str], account: &str) -> serde_json::Value {
        asset(
            INSTANCE,
            &format!("{P}/zones/us-east1-b/instances/{name}"),
            "us-east1-b",
            serde_json::json!({
                "networkInterfaces": [{"network": format!("{SELF}/global/networks/prod")}],
                "tags": {"items": tags},
                "serviceAccounts": [{"email": account}],
            }),
        )
    }

    fn assets() -> serde_json::Value {
        serde_json::json!([
            asset(
                NETWORK,
                &format!("{P}/global/networks/prod"),
                "global",
                serde_json::json!({
                    "peerings": [{"name": "to-shared", "state": "ACTIVE",
                                  "network": "https://www.googleapis.com/compute/v1/projects/host/global/networks/shared"}],
                })
            ),
            asset(
                SUBNETWORK,
                &format!("{P}/regions/us-east1/subnetworks/web"),
                "us-east1",
                serde_json::json!({
                    "network": format!("{SELF}/global/networks/prod"),
                    "ipCidrRange": "10.0.1.0/24", "ipv6CidrRange": "fd20::/64",
                })
            ),
            asset(
                FIREWALL,
                &format!("{P}/global/firewalls/allow-web"),
                "global",
                serde_json::json!({
                    "network": format!("{SELF}/global/networks/prod"),
                    "direction": "INGRESS", "priority": 900,
                    "sourceRanges": ["0.0.0.0/0", "::/0"],
                    "allowed": [{"IPProtocol": "tcp", "ports": ["80", "443"]}],
                    "targetTags": ["web"],
                })
            ),
            asset(
                FIREWALL,
                &format!("{P}/global/firewalls/deny-all-out"),
                "global",
                serde_json::json!({
                    "network": format!("{SELF}/global/networks/prod"),
                    "direction": "EGRESS",
                    "denied": [{"IPProtocol": "all"}],
                })
            ),
            asset(
                FIREWALL,
                &format!("{P}/global/firewalls/off"),
                "global",
                serde_json::json!({
                    "network": format!("{SELF}/global/networks/prod"),
                    "disabled": true, "sourceTags": ["x"], "allowed": [{"IPProtocol": "icmp"}],
                })
            ),
            vm("web-1", &["web"], "web@shop.iam.gserviceaccount.com"),
            vm("db-1", &["db"], "db@shop.iam.gserviceaccount.com"),
            asset(
                ROUTE,
                &format!("{P}/global/routes/default-internet"),
                "global",
                serde_json::json!({
                    "network": format!("{SELF}/global/networks/prod"), "destRange": "0.0.0.0/0",
                    "nextHopGateway": format!("{SELF}/global/gateways/default-internet-gateway"),
                })
            ),
            asset(
                ROUTE,
                &format!("{P}/global/routes/to-fw"),
                "global",
                serde_json::json!({
                    "network": format!("{SELF}/global/networks/prod"), "destRange": "10.9.0.0/16",
                    "nextHopIp": "10.0.1.9",
                })
            ),
            asset(
                ROUTER,
                &format!("{P}/regions/us-east1/routers/nat-router"),
                "us-east1",
                serde_json::json!({
                    "network": format!("{SELF}/global/networks/prod"),
                    "nats": [{"name": "egress", "natIps": [format!("{SELF}/regions/us-east1/addresses/nat-ip")],
                              "subnetworks": [{"name": format!("{SELF}/regions/us-east1/subnetworks/web")}]}],
                })
            ),
        ])
    }

    fn find<'a>(out: &'a Collected, arn: &str) -> &'a Resource {
        out.resources
            .iter()
            .find(|r| r.arn == arn)
            .unwrap_or_else(|| panic!("no {arn}"))
    }

    #[test]
    fn networks_subnets_and_peerings_name_their_projects() {
        let out = assemble(&batch(assets()));
        let net = find(&out, &format!("{P}/global/networks/prod"));
        assert_eq!(net.name(), Some("prod"));
        assert_eq!(net.region, "global");
        let Body::Subnet(s) =
            &find(&out, &format!("{P}/regions/us-east1/subnetworks/web")).body
        else {
            panic!()
        };
        assert_eq!(s.cidr_block.as_deref(), Some("10.0.1.0/24"));
        assert_eq!(s.ipv6_cidr_blocks, ["fd20::/64"]);
        assert_eq!(s.network_id.as_deref(), Some(net.arn.as_str()));
        let p = out.of_kind(ResourceKind::PeeringConnection).next().unwrap();
        let Body::PeeringConnection(p) = &p.body else { panic!() };
        assert_eq!(p.accepter.owner_id.as_deref(), Some("host"));
        assert_eq!(p.status.as_deref(), Some("ACTIVE"));
    }

    #[test]
    fn firewall_rules_expand_per_port_and_range_and_apply_by_tag() {
        let out = assemble(&batch(assets()));
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
        assert_eq!(
            rules.len(),
            5,
            "four web rules, one egress deny, none disabled"
        );
        let web = &rules[..4];
        assert!(web.iter().all(|r| !r.deny && r.priority == Some(900)));
        assert_eq!(web[2].from_port, Some(443));
        assert_eq!(web[1].cidr_ipv6.as_deref(), Some("::/0"));
        let out_deny = rules[4];
        assert!(out_deny.deny && out_deny.egress == Some(true));
        assert_eq!(out_deny.ip_protocol.as_deref(), Some("-1"));
        assert_eq!(out_deny.cidr_ipv4.as_deref(), Some("0.0.0.0/0"));
        assert_eq!(out_deny.priority, Some(DEFAULT_PRIORITY));
        let applied: BTreeSet<(String, String)> = out
            .relationships
            .iter()
            .filter(|r| r.edge == Edge::HasSecurityGroup)
            .map(|r| {
                (
                    name(r.from_arn.as_str()).to_string(),
                    name(r.to_arn.as_str()).to_string(),
                )
            })
            .collect();
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            applied,
            BTreeSet::from([
                pair("web-1", "allow-web"),
                pair("web-1", "deny-all-out"),
                pair("db-1", "deny-all-out"),
            ])
        );
    }

    #[test]
    fn source_tags_and_accounts_reference_instances() {
        let d = serde_json::json!({
            "sourceTags": ["lb"], "sourceServiceAccounts": ["a@x"],
            "allowed": [{"IPProtocol": "tcp", "ports": ["8000-8080"]}],
        });
        let got = rules(&Arn::from("fw"), &d);
        assert_eq!(got[0].referenced_group_id.as_deref(), Some("tag:lb"));
        assert_eq!(
            got[1].referenced_group_id.as_deref(),
            Some("serviceAccount:a@x")
        );
        assert_eq!(
            (got[0].from_port, got[0].to_port),
            (Some(8000), Some(8080))
        );
        assert_eq!(got[0].rule_id, "fw#1");
        let one = rules(
            &Arn::from("fw"),
            &serde_json::json!({
                "sourceRanges": ["10.0.0.0/8"], "allowed": [{"IPProtocol": "udp"}],
            }),
        );
        assert_eq!(
            one[0].rule_id, "fw",
            "a single rule keeps the firewall's id"
        );
        assert_eq!((one[0].from_port, one[0].to_port), (None, None));
    }

    #[test]
    fn a_networks_routes_are_one_table_and_nats_are_gateways() {
        let out = assemble(&batch(assets()));
        let Body::RouteTable(rt) =
            &out.of_kind(ResourceKind::RouteTable).next().unwrap().body
        else {
            panic!()
        };
        assert_eq!(rt.routes.len(), 2);
        assert_eq!(
            rt.routes[0].target.as_deref(),
            Some("default-internet-gateway")
        );
        assert_eq!(rt.routes[1].target.as_deref(), Some("10.0.1.9"));
        assert_eq!(
            rt.subnets,
            [format!("{P}/regions/us-east1/subnetworks/web")]
        );
        let nat = out.of_kind(ResourceKind::NatGateway).next().unwrap();
        assert_eq!(nat.region, "us-east1");
        let Body::NatGateway(n) = &nat.body else { panic!() };
        assert_eq!(
            n.nat_gateway_id,
            format!("{P}/regions/us-east1/routers/nat-router/nats/egress")
        );
        assert_eq!(
            n.public_ips,
            [format!("{P}/regions/us-east1/addresses/nat-ip")]
        );
        assert_eq!(
            n.subnet_id.as_deref(),
            Some(format!("{P}/regions/us-east1/subnetworks/web").as_str())
        );
    }
}
