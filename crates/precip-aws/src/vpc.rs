//! VPCs, subnets, security groups and their rules.

use precip_types::Arn;
use precip_types::scan::{Edge, ResourceKind};

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, next_page, tags,
};

pub struct Vpcs;

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
                let cidrs: Vec<&str> = vpc
                    .cidr_block_association_set()
                    .iter()
                    .filter_map(|c| c.cidr_block())
                    .collect();
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::Vpc, id),
                    ResourceKind::Vpc,
                    tags(vpc.tags()),
                    serde_json::json!({
                        "vpc_id": id,
                        "cidr_block": vpc.cidr_block(),
                        "cidr_blocks": cidrs,
                        "is_default": vpc.is_default(),
                        "owner_id": vpc.owner_id(),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct Subnets;

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
                let ipv6: Vec<&str> = s
                    .ipv6_cidr_block_association_set()
                    .iter()
                    .filter_map(|c| c.ipv6_cidr_block())
                    .collect();
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::Subnet, id),
                    ResourceKind::Subnet,
                    tags(s.tags()),
                    serde_json::json!({
                        "subnet_id": id,
                        "vpc_id": s.vpc_id(),
                        "cidr_block": s.cidr_block(),
                        "ipv6_cidr_blocks": ipv6,
                        "availability_zone": s.availability_zone(),
                        "map_public_ip_on_launch": s.map_public_ip_on_launch(),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct SecurityGroups;

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
                    ResourceKind::SecurityGroup,
                    tags(sg.tags()),
                    serde_json::json!({
                        "group_id": id,
                        "group_name": sg.group_name(),
                        "description": sg.description(),
                        "vpc_id": sg.vpc_id(),
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
pub struct SecurityGroupRules;

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
                let referenced_group =
                    rule.referenced_group_info().and_then(|r| r.group_id());
                out.resources.push(ctx.resource(
                    arn,
                    ResourceKind::SecurityGroupRule,
                    tags(rule.tags()),
                    serde_json::json!({
                        "rule_id": id,
                        "group_id": rule.group_id(),
                        "egress": rule.is_egress(),
                        "ip_protocol": rule.ip_protocol(),
                        "from_port": rule.from_port(),
                        "to_port": rule.to_port(),
                        "cidr_ipv4": rule.cidr_ipv4(),
                        "cidr_ipv6": rule.cidr_ipv6(),
                        "prefix_list_id": rule.prefix_list_id(),
                        "referenced_group_id": referenced_group,
                        "description": rule.description(),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct RouteTables;

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
                let routes: Vec<serde_json::Value> = rt
                    .routes()
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "destination": r.destination_cidr_block()
                                .or(r.destination_ipv6_cidr_block())
                                .or(r.destination_prefix_list_id()),
                            "target": r.gateway_id()
                                .or(r.nat_gateway_id())
                                .or(r.transit_gateway_id())
                                .or(r.vpc_peering_connection_id())
                                .or(r.instance_id())
                                .or(r.network_interface_id()),
                            "state": r.state().map(|s| s.as_str()),
                        })
                    })
                    .collect();
                let subnets: Vec<&str> = rt
                    .associations()
                    .iter()
                    .filter_map(|a| a.subnet_id())
                    .collect();
                let is_main =
                    rt.associations().iter().any(|a| a.main() == Some(true));
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::RouteTable, id),
                    ResourceKind::RouteTable,
                    tags(rt.tags()),
                    serde_json::json!({
                        "route_table_id": id,
                        "vpc_id": rt.vpc_id(),
                        "main": is_main,
                        "subnets": subnets,
                        "routes": routes,
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct InternetGateways;

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
                let vpcs: Vec<&str> = igw
                    .attachments()
                    .iter()
                    .filter_map(|a| a.vpc_id())
                    .collect();
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::InternetGateway, id),
                    ResourceKind::InternetGateway,
                    tags(igw.tags()),
                    serde_json::json!({ "internet_gateway_id": id, "vpcs": vpcs }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct NatGateways;

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
                let public_ips: Vec<&str> = nat
                    .nat_gateway_addresses()
                    .iter()
                    .filter_map(|a| a.public_ip())
                    .collect();
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::NatGateway, id),
                    ResourceKind::NatGateway,
                    tags(nat.tags()),
                    serde_json::json!({
                        "nat_gateway_id": id,
                        "vpc_id": nat.vpc_id(),
                        "subnet_id": nat.subnet_id(),
                        "state": nat.state().map(|s| s.as_str()),
                        "connectivity": nat.connectivity_type().map(|c| c.as_str()),
                        "public_ips": public_ips,
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct PeeringConnections;

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
                    serde_json::json!({
                        "vpc_id": v.and_then(|x| x.vpc_id()),
                        "owner_id": v.and_then(|x| x.owner_id()),
                        "region": v.and_then(|x| x.region()),
                        "cidr_block": v.and_then(|x| x.cidr_block()),
                    })
                };
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::VpcPeeringConnection, id),
                    ResourceKind::VpcPeeringConnection,
                    tags(pc.tags()),
                    serde_json::json!({
                        "vpc_peering_connection_id": id,
                        "status": pc.status().and_then(|s| s.code()).map(|c| c.as_str()),
                        "requester": side(pc.requester_vpc_info()),
                        "accepter": side(pc.accepter_vpc_info()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// Managed prefix lists with their entries, so a security group rule that
/// names one can be expanded to CIDRs. Only lists referenced by a rule
/// are described, in the second stage.
pub struct PrefixLists;

impl Collect for PrefixLists {
    const API: &'static str = "ec2:GetManagedPrefixListEntries";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut out = Collected::default();
        let ids = prior
            .distinct_field(ResourceKind::SecurityGroupRule, "prefix_list_id");
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
                entries.extend(
                    page.entries()
                        .iter()
                        .filter_map(|e| e.cidr().map(str::to_string)),
                );
            }
            let arn = pl
                .prefix_list_arn()
                .map_or_else(|| ctx.arn(ArnType::PrefixList, id), Arn::from);
            out.resources.push(ctx.resource(
                arn,
                ResourceKind::PrefixList,
                tags(pl.tags()),
                serde_json::json!({
                    "prefix_list_id": id,
                    "name": pl.prefix_list_name(),
                    "owner_id": pl.owner_id(),
                    "address_family": pl.address_family(),
                    "entries": entries,
                }),
            ));
        }
        Ok(out)
    }
}
