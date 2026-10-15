//! VPCs, subnets, security groups and their rules.

use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, next_page, own,
    owned, tags,
};

pub(crate) struct Vpcs;

impl Collect for Vpcs {
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
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::Vpc, id),
                        tags(vpc.tags()),
                        Body::Vpc(body::Vpc {
                            vpc_id: id.to_string(),
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
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::Subnet, id),
                        tags(s.tags()),
                        Body::Subnet(body::Subnet {
                            subnet_id: id.to_string(),
                            vpc_id: own(s.vpc_id()),
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
                        group_id: id.to_string(),
                        group_name: own(sg.group_name()),
                        description: own(sg.description()),
                        vpc_id: own(sg.vpc_id()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// One row per rule, with the source kept distinct: CIDR, prefix list or
/// another security group. The group-to-group case is what the firewall
/// translation has to flag.
pub(crate) struct SecurityGroupRules;

impl Collect for SecurityGroupRules {
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
                let arn = ctx.arn(ArnType::SecurityGroupRule, id);
                if let Some(g) = rule.group_id() {
                    out.relate(
                        arn.clone(),
                        Edge::RuleOf,
                        ctx.arn(ArnType::SecurityGroup, g),
                    );
                }
                out.resources.push(
                    ctx.resource(
                        arn,
                        tags(rule.tags()),
                        Body::SecurityGroupRule(body::SecurityGroupRule {
                            rule_id: id.to_string(),
                            group_id: own(rule.group_id()),
                            egress: rule.is_egress(),
                            ip_protocol: own(rule.ip_protocol()),
                            from_port: rule.from_port(),
                            to_port: rule.to_port(),
                            cidr_ipv4: own(rule.cidr_ipv4()),
                            cidr_ipv6: own(rule.cidr_ipv6()),
                            prefix_list_id: own(rule.prefix_list_id()),
                            referenced_group_id: own(rule
                                .referenced_group_info()
                                .and_then(|r| r.group_id())),
                            description: own(rule.description()),
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

pub(crate) struct RouteTables;

impl Collect for RouteTables {
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
                            .or(r.destination_ipv6_cidr_block())
                            .or(r.destination_prefix_list_id())),
                        target: own(r
                            .gateway_id()
                            .or(r.nat_gateway_id())
                            .or(r.transit_gateway_id())
                            .or(r.vpc_peering_connection_id())
                            .or(r.instance_id())
                            .or(r.network_interface_id())),
                        state: own(r.state().map(|s| s.as_str())),
                    })
                    .collect();
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::RouteTable, id),
                        tags(rt.tags()),
                        Body::RouteTable(body::RouteTable {
                            route_table_id: id.to_string(),
                            vpc_id: own(rt.vpc_id()),
                            main: rt
                                .associations()
                                .iter()
                                .any(|a| a.main() == Some(true)),
                            subnets: owned(
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
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::InternetGateway, id),
                    tags(igw.tags()),
                    Body::InternetGateway(body::InternetGateway {
                        internet_gateway_id: id.to_string(),
                        vpcs: owned(
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
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::NatGateway, id),
                        tags(nat.tags()),
                        Body::NatGateway(body::NatGateway {
                            nat_gateway_id: id.to_string(),
                            vpc_id: own(nat.vpc_id()),
                            subnet_id: own(nat.subnet_id()),
                            state: own(nat.state().map(|s| s.as_str())),
                            connectivity: own(nat
                                .connectivity_type()
                                .map(|c| c.as_str())),
                            public_ips: owned(
                                nat.nat_gateway_addresses()
                                    .iter()
                                    .filter_map(|a| a.public_ip()),
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
                let side = |v: Option<
                    &aws_sdk_ec2::types::VpcPeeringConnectionVpcInfo,
                >| {
                    body::PeeringSide {
                        vpc_id: own(v.and_then(|x| x.vpc_id())),
                        owner_id: own(v.and_then(|x| x.owner_id())),
                        region: own(v.and_then(|x| x.region())),
                        cidr_block: own(v.and_then(|x| x.cidr_block())),
                    }
                };
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::VpcPeeringConnection, id),
                        tags(pc.tags()),
                        Body::VpcPeeringConnection(body::PeeringConnection {
                            vpc_peering_connection_id: id.to_string(),
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
    const API: &'static str = "ec2:GetManagedPrefixListEntries";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut out = Collected::default();
        let ids = prior.distinct(|b| {
            if let Body::SecurityGroupRule(r) = b {
                r.prefix_list_id.as_deref()
            } else {
                None
            }
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
            let arn = pl
                .prefix_list_arn()
                .map_or_else(|| ctx.arn(ArnType::PrefixList, id), Arn::from);
            out.resources.push(ctx.resource(
                arn,
                tags(pl.tags()),
                Body::PrefixList(body::PrefixList {
                    prefix_list_id: id.to_string(),
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
