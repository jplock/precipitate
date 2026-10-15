//! VPCs, subnets, security groups and their rules.

use aws_sdk_ec2::types::SecurityGroupRule as SdkRule;
use precip_types::body::{
    Peer, Protocol, RouteTarget, RuleAction, TrafficDirection,
};
use precip_types::scan::Edge;
use precip_types::{AccountId, Arn, Body, body};

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, name_tag,
    next_page, own, owned, tags,
};

pub(crate) struct Vpcs;

impl Collect for Vpcs {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeVpcs";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = ec2.describe_vpcs().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for vpc in page.vpcs() {
                let Some(id) = vpc.vpc_id() else { continue };
                let tags = tags(vpc.tags());
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::Vpc, id),
                        tags.clone(),
                        Body::Network(body::Network {
                            name: name_tag(&tags),
                            cidr_block: own(vpc.cidr_block()),
                            cidr_blocks: owned(
                                vpc.cidr_block_association_set()
                                    .iter()
                                    .filter_map(|c| c.cidr_block()),
                            ),
                            is_default: vpc.is_default(),
                            owner_id: own(vpc.owner_id()),
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

pub(crate) struct Subnets;

impl Collect for Subnets {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeSubnets";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = ec2.describe_subnets().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for s in page.subnets() {
                let Some(id) = s.subnet_id() else { continue };
                let tags = tags(s.tags());
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::Subnet, id),
                        tags.clone(),
                        Body::Subnet(body::Subnet {
                            name: name_tag(&tags),
                            network_id: ctx.refer(ArnType::Vpc, s.vpc_id()),
                            cidr_block: own(s.cidr_block()),
                            ipv6_cidr_blocks: owned(
                                s.ipv6_cidr_block_association_set()
                                    .iter()
                                    .filter_map(|c| c.ipv6_cidr_block()),
                            ),
                            availability_zone: own(s.availability_zone()),
                            map_public_ip_on_launch: s
                                .map_public_ip_on_launch(),
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

pub(crate) struct SecurityGroups;

impl Collect for SecurityGroups {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeSecurityGroups";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = ec2.describe_security_groups().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for sg in page.security_groups() {
                let Some(id) = sg.group_id() else { continue };
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::SecurityGroup, id),
                    tags(sg.tags()),
                    Body::SecurityGroup(body::SecurityGroup {
                        name: own(sg.group_name()),
                        description: own(sg.description()),
                        network_id: ctx.refer(ArnType::Vpc, sg.vpc_id()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// One row per rule, with its peer kept distinct: a CIDR, a prefix list
/// or another security group, which may be in another account.
pub(crate) struct SecurityGroupRules;

/// An EC2 rule in Oxide's terms. EC2 rules only allow and are unordered.
/// For ICMP, EC2 puts the type in the from port and the code in the to
/// port; -1 means every one.
fn rule_body(
    ctx: &RegionContext,
    rule: &SdkRule,
) -> Option<body::SecurityGroupRule> {
    let group = ctx.arn(ArnType::SecurityGroup, rule.group_id()?).to_string();
    let any = |n: Option<i32>| n.filter(|p| *p >= 0);
    let protocol = Protocol::parse(rule.ip_protocol().unwrap_or("-1"));
    let (ports, icmp) = match protocol {
        Protocol::Tcp | Protocol::Udp => {
            (any(rule.from_port()).zip(any(rule.to_port())), None)
        }
        Protocol::Icmp | Protocol::Icmp6 => {
            (None, any(rule.from_port()).map(|t| (t, any(rule.to_port()))))
        }
        Protocol::All | Protocol::Other(_) => (None, None),
    };
    let peer = if let Some(c) = rule.cidr_ipv4().or(rule.cidr_ipv6()) {
        Peer::IpNet(c.to_string())
    } else if let Some(pl) = rule.prefix_list_id() {
        Peer::PrefixList(ctx.arn(ArnType::PrefixList, pl).to_string())
    } else {
        let r = rule.referenced_group_info()?;
        // A group in a peered VPC can belong to another account.
        let account = r
            .user_id()
            .map_or_else(|| ctx.identity.account_id.clone(), AccountId::from);
        Peer::Group(
            ArnType::SecurityGroup
                .arn(
                    &ctx.identity.partition,
                    &ctx.region,
                    &account,
                    r.group_id()?,
                )
                .to_string(),
        )
    };
    Some(body::SecurityGroupRule {
        group,
        direction: if rule.is_egress() == Some(true) {
            TrafficDirection::Outbound
        } else {
            TrafficDirection::Inbound
        },
        action: RuleAction::Allow,
        priority: None,
        protocol,
        ports,
        icmp,
        peer,
        description: own(rule.description()),
    })
}

impl Collect for SecurityGroupRules {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeSecurityGroupRules";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages =
            ec2.describe_security_group_rules().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for rule in page.security_group_rules() {
                let Some(id) = rule.security_group_rule_id() else {
                    continue;
                };
                let Some(body) = rule_body(ctx, rule) else { continue };
                let arn = ctx.arn(ArnType::SecurityGroupRule, id);
                out.relate(
                    arn.clone(),
                    Edge::RuleOf,
                    Arn::from(body.group.as_str()),
                );
                out.resources.push(ctx.resource(
                    arn,
                    tags(rule.tags()),
                    Body::SecurityGroupRule(body),
                ));
            }
        }
        Ok(out)
    }
}

pub(crate) struct RouteTables;

/// Where an EC2 route sends traffic. `local` is the VPC itself; a virtual
/// private gateway, transit gateway or network interface has no Oxide
/// equivalent.
fn route_target(
    ctx: &RegionContext,
    r: &aws_sdk_ec2::types::Route,
) -> RouteTarget {
    let arn = |ty, id: &str| ctx.arn(ty, id).to_string();
    if let Some(g) = r.gateway_id() {
        return match g {
            "local" => RouteTarget::Local,
            g if g.starts_with("igw-") => {
                RouteTarget::InternetGateway(arn(ArnType::InternetGateway, g))
            }
            other => RouteTarget::Other(other.to_string()),
        };
    }
    if let Some(n) = r.nat_gateway_id() {
        RouteTarget::NatGateway(arn(ArnType::NatGateway, n))
    } else if let Some(p) = r.vpc_peering_connection_id() {
        RouteTarget::Peering(arn(ArnType::VpcPeeringConnection, p))
    } else if let Some(i) = r.instance_id() {
        RouteTarget::Instance(arn(ArnType::Instance, i))
    } else if let Some(other) = r
        .transit_gateway_id()
        .or(r.network_interface_id())
        .or(r.egress_only_internet_gateway_id())
        .or(r.carrier_gateway_id())
        .or(r.local_gateway_id())
    {
        RouteTarget::Other(other.to_string())
    } else {
        RouteTarget::Local
    }
}

impl Collect for RouteTables {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeRouteTables";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = ec2.describe_route_tables().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for rt in page.route_tables() {
                let Some(id) = rt.route_table_id() else { continue };
                let routes = rt
                    .routes()
                    .iter()
                    .map(|r| body::Route {
                        destination: own(r
                            .destination_cidr_block()
                            .or(r.destination_ipv6_cidr_block()))
                        .or_else(|| {
                            ctx.refer(
                                ArnType::PrefixList,
                                r.destination_prefix_list_id(),
                            )
                        }),
                        target: route_target(ctx, r),
                        state: own(r.state().map(|s| s.as_str())),
                    })
                    .collect();
                let tags = tags(rt.tags());
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::RouteTable, id),
                        tags.clone(),
                        Body::RouteTable(body::RouteTable {
                            name: name_tag(&tags),
                            network_id: ctx.refer(ArnType::Vpc, rt.vpc_id()),
                            main: rt
                                .associations()
                                .iter()
                                .any(|a| a.main() == Some(true)),
                            subnets: ctx.refer_all(
                                ArnType::Subnet,
                                rt.associations()
                                    .iter()
                                    .filter_map(|a| a.subnet_id()),
                            ),
                            routes,
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

pub(crate) struct InternetGateways;

impl Collect for InternetGateways {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeInternetGateways";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages =
            ec2.describe_internet_gateways().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for igw in page.internet_gateways() {
                let Some(id) = igw.internet_gateway_id() else { continue };
                let tags = tags(igw.tags());
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::InternetGateway, id),
                    tags.clone(),
                    Body::InternetGateway(body::InternetGateway {
                        name: name_tag(&tags),
                        networks: ctx.refer_all(
                            ArnType::Vpc,
                            igw.attachments().iter().filter_map(|a| a.vpc_id()),
                        ),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub(crate) struct NatGateways;

impl Collect for NatGateways {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeNatGateways";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = ec2.describe_nat_gateways().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for nat in page.nat_gateways() {
                let Some(id) = nat.nat_gateway_id() else { continue };
                let tags = tags(nat.tags());
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::NatGateway, id),
                        tags.clone(),
                        Body::NatGateway(body::NatGateway {
                            name: name_tag(&tags),
                            network_id: ctx.refer(ArnType::Vpc, nat.vpc_id()),
                            subnet_id: ctx
                                .refer(ArnType::Subnet, nat.subnet_id()),
                            state: own(nat.state().map(|s| s.as_str())),
                            connectivity: own(nat
                                .connectivity_type()
                                .map(|c| c.as_str())),
                            public_ips: ctx.refer_all(
                                ArnType::ElasticIp,
                                nat.nat_gateway_addresses()
                                    .iter()
                                    .filter_map(|a| a.allocation_id()),
                            ),
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

pub(crate) struct PeeringConnections;

impl Collect for PeeringConnections {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeVpcPeeringConnections";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages =
            ec2.describe_vpc_peering_connections().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for pc in page.vpc_peering_connections() {
                let Some(id) = pc.vpc_peering_connection_id() else {
                    continue;
                };
                // Each side is in its own account and region.
                let side = |v: Option<
                    &aws_sdk_ec2::types::VpcPeeringConnectionVpcInfo,
                >| {
                    let network = v.and_then(|x| {
                        let region = x.region().map_or_else(
                            || ctx.region.clone(),
                            precip_types::Region::from,
                        );
                        let account = x.owner_id().map_or_else(
                            || ctx.identity.account_id.clone(),
                            AccountId::from,
                        );
                        Some(
                            ArnType::Vpc
                                .arn(
                                    &ctx.identity.partition,
                                    &region,
                                    &account,
                                    x.vpc_id()?,
                                )
                                .to_string(),
                        )
                    });
                    body::PeeringSide {
                        network_id: network,
                        owner_id: own(v.and_then(|x| x.owner_id())),
                        region: own(v.and_then(|x| x.region())),
                        cidr_block: own(v.and_then(|x| x.cidr_block())),
                    }
                };
                let tags = tags(pc.tags());
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::VpcPeeringConnection, id),
                        tags.clone(),
                        Body::PeeringConnection(body::PeeringConnection {
                            name: name_tag(&tags),
                            status: own(pc
                                .status()
                                .and_then(|s| s.code())
                                .map(|c| c.as_str())),
                            requester: side(pc.requester_vpc_info()),
                            accepter: side(pc.accepter_vpc_info()),
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

/// Managed prefix lists with their entries, so a security group rule that
/// names one can be expanded to CIDRs. Only lists referenced by a rule
/// are described, in the second stage.
pub(crate) struct PrefixLists;

impl Collect for PrefixLists {
    type Context = RegionContext;
    const API: &'static str = "ec2:GetManagedPrefixListEntries";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut out = Collected::default();
        let ids = prior.distinct(|b| {
            let Body::SecurityGroupRule(r) = b else { return None };
            let Peer::PrefixList(arn) = &r.peer else { return None };
            arn.rsplit_once("prefix-list/").map(|(_, id)| id)
        });
        if ids.is_empty() {
            return Ok(out);
        }
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let lists = ec2
            .describe_managed_prefix_lists()
            .set_prefix_list_ids(Some(ids))
            .send()
            .await
            .map_err(CollectError::new("ec2:DescribeManagedPrefixLists"))?;
        for pl in lists.prefix_lists() {
            let Some(id) = pl.prefix_list_id() else { continue };
            let mut entries = Vec::new();
            let mut pages = ec2
                .get_managed_prefix_list_entries()
                .prefix_list_id(id)
                .into_paginator()
                .send();
            while let Some(page) = next_page(Self::API, &mut pages).await? {
                entries.extend(owned(
                    page.entries().iter().filter_map(|e| e.cidr()),
                ));
            }
            // Named as rules name it, in this account, even for lists AWS
            // owns, so a rule's peer finds its list.
            out.resources.push(ctx.resource(
                ctx.arn(ArnType::PrefixList, id),
                tags(pl.tags()),
                Body::PrefixList(body::PrefixList {
                    name: own(pl.prefix_list_name()),
                    owner_id: own(pl.owner_id()),
                    address_family: own(pl.address_family()),
                    entries,
                }),
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ec2::types::{ReferencedSecurityGroup, Route};

    fn rule() -> aws_sdk_ec2::types::builders::SecurityGroupRuleBuilder {
        SdkRule::builder().group_id("sg-web").is_egress(false)
    }

    #[test]
    fn rules_name_their_group_peer_and_ports_by_arn() {
        let ctx = RegionContext::test();
        let https = rule_body(
            &ctx,
            &rule()
                .ip_protocol("tcp")
                .from_port(443)
                .to_port(443)
                .cidr_ipv4("0.0.0.0/0")
                .build(),
        )
        .unwrap();
        assert_eq!(
            https.group,
            "arn:aws:ec2:us-east-1:111111111111:security-group/sg-web"
        );
        assert_eq!(https.direction, TrafficDirection::Inbound);
        assert_eq!((https.action, https.priority), (RuleAction::Allow, None));
        assert_eq!(
            (https.protocol, https.ports),
            (Protocol::Tcp, Some((443, 443)))
        );
        assert_eq!(https.peer, Peer::IpNet("0.0.0.0/0".into()));

        let from_peer_account = rule_body(
            &ctx,
            &rule()
                .ip_protocol("-1")
                .from_port(-1)
                .to_port(-1)
                .is_egress(true)
                .referenced_group_info(
                    ReferencedSecurityGroup::builder()
                        .group_id("sg-db")
                        .user_id("222222222222")
                        .build(),
                )
                .build(),
        )
        .unwrap();
        assert_eq!(from_peer_account.direction, TrafficDirection::Outbound);
        assert_eq!(
            (from_peer_account.protocol, from_peer_account.ports),
            (Protocol::All, None)
        );
        assert_eq!(
            from_peer_account.peer,
            Peer::Group(
                "arn:aws:ec2:us-east-1:222222222222:security-group/sg-db"
                    .into()
            )
        );

        let listed = rule_body(
            &ctx,
            &rule().ip_protocol("6").prefix_list_id("pl-1").build(),
        )
        .unwrap();
        assert_eq!(
            listed.peer,
            Peer::PrefixList(
                "arn:aws:ec2:us-east-1:111111111111:prefix-list/pl-1".into()
            )
        );
        assert_eq!(listed.ports, None, "no port range is every port");
    }

    #[test]
    fn icmp_rules_carry_type_and_code_and_a_rule_without_peer_is_skipped() {
        let ctx = RegionContext::test();
        let ping = rule_body(
            &ctx,
            &rule()
                .ip_protocol("icmp")
                .from_port(8)
                .to_port(-1)
                .cidr_ipv4("10.0.0.0/8")
                .build(),
        )
        .unwrap();
        assert_eq!((ping.ports, ping.icmp), (None, Some((8, None))));
        let all_icmp6 = rule_body(
            &ctx,
            &rule()
                .ip_protocol("58")
                .from_port(-1)
                .to_port(-1)
                .cidr_ipv6("::/0")
                .build(),
        )
        .unwrap();
        assert_eq!(
            (all_icmp6.protocol, all_icmp6.icmp),
            (Protocol::Icmp6, None)
        );
        assert!(rule_body(&ctx, &rule().ip_protocol("tcp").build()).is_none());
        assert!(
            rule_body(&ctx, &SdkRule::builder().cidr_ipv4("0.0.0.0/0").build())
                .is_none()
        );
    }

    #[test]
    fn route_targets_are_typed_and_referenced_by_arn() {
        let ctx = RegionContext::test();
        let t = |r: Route| route_target(&ctx, &r);
        assert_eq!(
            t(Route::builder().gateway_id("local").build()),
            RouteTarget::Local
        );
        assert_eq!(
            t(Route::builder().gateway_id("igw-1").build()),
            RouteTarget::InternetGateway(
                "arn:aws:ec2:us-east-1:111111111111:internet-gateway/igw-1"
                    .into()
            )
        );
        assert_eq!(
            t(Route::builder().gateway_id("vgw-1").build()),
            RouteTarget::Other("vgw-1".into())
        );
        assert_eq!(
            t(Route::builder().nat_gateway_id("nat-1").build()),
            RouteTarget::NatGateway(
                "arn:aws:ec2:us-east-1:111111111111:natgateway/nat-1".into()
            )
        );
        assert_eq!(
            t(Route::builder().vpc_peering_connection_id("pcx-1").build()),
            RouteTarget::Peering("arn:aws:ec2:us-east-1:111111111111:vpc-peering-connection/pcx-1".into())
        );
        assert_eq!(
            t(Route::builder().instance_id("i-1").build()),
            RouteTarget::Instance(
                "arn:aws:ec2:us-east-1:111111111111:instance/i-1".into()
            )
        );
        assert_eq!(
            t(Route::builder().transit_gateway_id("tgw-1").build()),
            RouteTarget::Other("tgw-1".into())
        );
    }
}
