//! Terraform for one workload and environment pair, written for the Oxide
//! provider. Pure over one scan's resources and the pair's members: an
//! `oxide_project` for the pair, the VPCs and subnets its instances use,
//! one `oxide_vpc_firewall_rules` per VPC from the security groups its
//! instances are in, custom routers for routes other than the default one,
//! and its disks, floating IPs and instances. What does not translate is
//! listed as a finding in the file's header instead of being guessed.

use hcl::expr::{Traversal, Variable};
use hcl::{Block, Body, Expression, Identifier, Number, Object, ObjectKey};
use precip_types::body::{
    self, Peer, Protocol, RouteTarget, RuleAction, TrafficDirection,
};
use precip_types::scan::{Edge, Relationship, Resource};
use precip_types::{Body as ResourceBody, PairKey, WorkloadMember};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const GIB: i64 = 1 << 30;
const MIB: i64 = 1 << 20;
/// Oxide's limits, from Omicron (see the RFD's working notes).
const MAX_DISK_GIB: i32 = 1023;
const MAX_DISKS: usize = 12;
const MAX_NICS: usize = 8;
const MAX_VCPUS: i32 = 254;
const MAX_MEMORY_MIB: i64 = 1536 * 1024;
/// The priority given to rules from providers that do not order them.
/// EC2 rules only allow, so any one priority keeps their meaning.
const UNORDERED_PRIORITY: i32 = 1000;
/// Oxide names are at most 63 characters.
const NAME_MAX: usize = 63;
/// The internet gateway every Oxide VPC is created with.
const DEFAULT_GATEWAY: &str = "default";

/// Everything rendering reads.
pub struct Input<'a> {
    pub resources: &'a [Resource],
    pub relationships: &'a [Relationship],
    pub members: &'a [WorkloadMember],
    pub pair: &'a PairKey,
}

/// A name Oxide accepts: lowercase letters, digits and dashes, starting
/// with a letter, at most 63 characters.
fn oxide_name(raw: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in raw.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let mut name = out.trim_matches('-').to_string();
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name = format!("{fallback}-{name}");
    }
    name.truncate(NAME_MAX);
    name.trim_end_matches('-').to_string()
}

/// The last segment of an ARN or resource id, for resources with no name.
fn last_segment(arn: &str) -> &str {
    arn.rsplit(['/', ':']).next().unwrap_or(arn)
}

/// Names unique among one kind of resource.
#[derive(Default)]
struct Names(HashSet<String>);

impl Names {
    fn unique(&mut self, base: String) -> String {
        let mut name = base.clone();
        let mut n = 2;
        while !self.0.insert(name.clone()) {
            let suffix = format!("-{n}");
            let mut stem = base.clone();
            stem.truncate(NAME_MAX - suffix.len());
            name = format!("{}{suffix}", stem.trim_end_matches('-'));
            n += 1;
        }
        name
    }
}

/// `<kind>.<label>.id`.
fn id_of(kind: &str, label: &str) -> Expression {
    Traversal::builder(Variable::unchecked(kind))
        .attr(Identifier::sanitized(label))
        .attr("id")
        .build()
        .into()
}

fn number(n: i64) -> Expression {
    Expression::from(Number::from(n))
}

fn object(fields: Vec<(&str, Expression)>) -> Expression {
    let o: Object<ObjectKey, Expression> = fields
        .into_iter()
        .map(|(k, v)| (ObjectKey::Identifier(Identifier::sanitized(k)), v))
        .collect();
    Expression::from(o)
}

fn typed(kind: &str, value: &str) -> Expression {
    object(vec![("type", kind.into()), ("value", value.into())])
}

fn resource(kind: &str, label: &str) -> hcl::BlockBuilder {
    Block::builder("resource").add_label(kind).add_label(label)
}

/// One rendered pair: blocks in order, and the findings for its header.
struct Plan<'a> {
    input: &'a Input<'a>,
    by_arn: HashMap<&'a str, &'a Resource>,
    blocks: Vec<Block>,
    findings: Vec<String>,
    /// ARN to the Oxide name given to it, by kind of Oxide resource.
    vpcs: BTreeMap<&'a str, String>,
    subnets: BTreeMap<&'a str, String>,
    instances: BTreeMap<&'a str, String>,
    disks: BTreeMap<&'a str, String>,
}

impl<'a> Plan<'a> {
    fn name_of(&self, arn: &str) -> String {
        self.by_arn
            .get(arn)
            .and_then(|r| r.tags.get("Name").map(String::as_str).or(r.name()))
            .unwrap_or_else(|| last_segment(arn))
            .to_string()
    }

    fn body_of(&self, arn: &str) -> Option<&'a ResourceBody> {
        self.by_arn.get(arn).map(|r| &r.body)
    }

    /// The rendered instances related to `group` by `has_security_group`.
    fn group_members(&self, group: &str) -> Vec<&'a str> {
        let mut out: Vec<&str> = self
            .input
            .relationships
            .iter()
            .filter(|r| r.edge == Edge::HasSecurityGroup && r.to_arn == group)
            .map(|r| r.from_arn.as_str())
            .filter(|a| self.instances.contains_key(a))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// The Terraform for `input.pair`, with its findings as a comment header.
pub fn render(input: &Input<'_>) -> String {
    let mut plan = Plan {
        input,
        by_arn: input.resources.iter().map(|r| (r.arn.as_str(), r)).collect(),
        blocks: Vec::new(),
        findings: Vec::new(),
        vpcs: BTreeMap::new(),
        subnets: BTreeMap::new(),
        instances: BTreeMap::new(),
        disks: BTreeMap::new(),
    };
    let members: BTreeSet<&str> = input
        .members
        .iter()
        .filter(|m| m.pair == *input.pair)
        .map(|m| m.arn.as_str())
        .collect();
    let mut instances: Vec<(&Resource, &body::Instance)> = Vec::new();
    let mut volumes: Vec<(&Resource, &body::Volume)> = Vec::new();
    let mut public_ips: Vec<(&Resource, &body::PublicIp)> = Vec::new();
    for arn in &members {
        let Some(r) = plan.by_arn.get(arn).copied() else { continue };
        if let ResourceBody::Instance(i) = &r.body {
            instances.push((r, i));
        } else if let ResourceBody::Volume(v) = &r.body {
            volumes.push((r, v));
        } else if let ResourceBody::PublicIp(p) = &r.body {
            public_ips.push((r, p));
        } else {
            untranslatable(&mut plan.findings, r, &r.body);
        }
    }

    let project = oxide_name(
        &format!("{}-{}", input.pair.workload, input.pair.environment),
        "p",
    );
    plan.blocks.push(
        resource("oxide_project", &project)
            .add_attribute(("name", project.clone()))
            .add_attribute((
                "description",
                format!(
                    "{} {}, migrated by precip",
                    input.pair.workload, input.pair.environment
                ),
            ))
            .add_attribute(("skip_default_vpc", true))
            .build(),
    );
    let project_id = id_of("oxide_project", &project);

    render_networks(&mut plan, &instances, &project_id);
    let mut instance_names = Names::default();
    for (r, _) in &instances {
        let name = instance_names
            .unique(oxide_name(&plan.name_of(r.arn.as_str()), "i"));
        plan.instances.insert(r.arn.as_str(), name);
    }
    render_firewalls(&mut plan, &instances);
    render_routers(&mut plan);
    render_disks(&mut plan, &volumes, &project_id);
    let floating = render_floating_ips(&mut plan, &public_ips, &project_id);
    render_instances(&mut plan, &instances, &floating, &project_id);

    let mut out = format!(
        "# Oxide Terraform for {} {}, generated by precip.\n\
         # Each oxide_vpc_firewall_rules replaces the VPC's default rules.\n\
         # Instances are created stopped with blank disks: import each disk's\n\
         # data (as a raw image) before starting them.\n",
        input.pair.workload, input.pair.environment
    );
    if !plan.findings.is_empty() {
        out.push_str("#\n# Findings:\n");
        for f in &plan.findings {
            out.push_str(&format!("# - {}\n", f.replace('\n', " ")));
        }
    }
    out.push('\n');
    let body = Body::builder().add_blocks(plan.blocks).build();
    // Serializing a body built from plain values cannot fail.
    out.push_str(&hcl::to_string(&body).unwrap_or_default());
    out
}

/// A finding for a member with no Oxide equivalent.
fn untranslatable(findings: &mut Vec<String>, r: &Resource, b: &ResourceBody) {
    let name = r.name().unwrap_or_else(|| last_segment(r.arn.as_str()));
    let what = match b {
        ResourceBody::LoadBalancer(_) => format!(
            "load balancer {name}: Oxide has none; run one on instances"
        ),
        ResourceBody::ScaleGroup(_) => format!(
            "scale group {name}: Oxide does not autoscale; its instances are fixed"
        ),
        ResourceBody::ManagedService(m) => format!(
            "{} {name} ({}): no Oxide equivalent; replatform it or keep it where it is",
            m.service.product(),
            m.service.as_str()
        ),
        ResourceBody::PeeringConnection(_) => {
            format!("peering {name}: Oxide VPCs do not peer")
        }
        ResourceBody::Account(_)
        | ResourceBody::Instance(_)
        | ResourceBody::InstanceType(_)
        | ResourceBody::Volume(_)
        | ResourceBody::Image(_)
        | ResourceBody::Network(_)
        | ResourceBody::Subnet(_)
        | ResourceBody::SecurityGroup(_)
        | ResourceBody::SecurityGroupRule(_)
        | ResourceBody::RouteTable(_)
        | ResourceBody::InternetGateway(_)
        | ResourceBody::NatGateway(_)
        | ResourceBody::PrefixList(_)
        | ResourceBody::LaunchTemplate(_)
        | ResourceBody::LaunchConfiguration(_)
        | ResourceBody::PublicIp(_)
        | ResourceBody::TargetGroup(_)
        | ResourceBody::DeploymentGroup(_) => return,
    };
    findings.push(what);
}

fn is_rfc1918(cidr: &str) -> bool {
    let Some(ip) = cidr.split('/').next() else { return false };
    let octets: Vec<u8> =
        ip.split('.').filter_map(|o| o.parse().ok()).collect();
    match octets.as_slice() {
        [10, ..] => true,
        [172, b, ..] => (16..=31).contains(b),
        [192, 168, ..] => true,
        _ => false,
    }
}

fn render_networks<'a>(
    plan: &mut Plan<'a>,
    instances: &[(&'a Resource, &'a body::Instance)],
    project_id: &Expression,
) {
    let subnet_arns: BTreeSet<&str> = instances
        .iter()
        .flat_map(|(_, i)| i.subnets.iter().map(String::as_str))
        .collect();
    let mut network_arns: BTreeSet<&str> =
        instances.iter().filter_map(|(_, i)| i.network_id.as_deref()).collect();
    for s in &subnet_arns {
        if let Some(ResourceBody::Subnet(b)) = plan.body_of(s) {
            network_arns.extend(b.network_id.as_deref());
        }
    }
    let mut vpc_names = Names::default();
    for arn in &network_arns {
        let name = vpc_names.unique(oxide_name(&plan.name_of(arn), "vpc"));
        plan.blocks.push(
            resource("oxide_vpc", &name)
                .add_attribute(("project_id", project_id.clone()))
                .add_attribute(("name", name.clone()))
                .add_attribute(("dns_name", name.clone()))
                .add_attribute(("description", format!("From {arn}")))
                .add_attribute(("skip_default_subnet", true))
                .build(),
        );
        plan.vpcs.insert(arn, name);
    }
    let mut subnet_names = Names::default();
    for arn in subnet_arns {
        let Some(ResourceBody::Subnet(s)) = plan.body_of(arn) else {
            plan.findings.push(format!("subnet {arn} was not collected"));
            continue;
        };
        let Some(vpc) =
            s.network_id.as_deref().and_then(|n| plan.vpcs.get(n)).cloned()
        else {
            plan.findings.push(format!("subnet {arn} has no known VPC"));
            continue;
        };
        let Some(block) = s.cidr_block.as_deref() else {
            plan.findings.push(format!("subnet {arn} has no IPv4 range"));
            continue;
        };
        if !is_rfc1918(block) {
            plan.findings.push(format!(
                "subnet {arn}: {block} is not an RFC 1918 range, which Oxide requires"
            ));
        }
        if !s.ipv6_cidr_blocks.is_empty() {
            plan.findings.push(format!(
                "subnet {arn}: IPv6 ranges are left for Oxide to assign from the VPC's prefix"
            ));
        }
        let name =
            subnet_names.unique(oxide_name(&plan.name_of(arn), "subnet"));
        plan.blocks.push(
            resource("oxide_vpc_subnet", &name)
                .add_attribute(("vpc_id", id_of("oxide_vpc", &vpc)))
                .add_attribute(("name", name.clone()))
                .add_attribute(("ipv4_block", block))
                .add_attribute(("description", format!("From {arn}")))
                .build(),
        );
        plan.subnets.insert(arn, name);
    }
}

/// The hosts a rule's peer stands for, or why it cannot be expressed.
fn hosts(
    plan: &Plan<'_>,
    peer: &Peer,
    vpc: &str,
) -> Result<Vec<Expression>, String> {
    match peer {
        Peer::IpNet(net) => Ok(vec![typed("ip_net", net)]),
        Peer::PrefixList(arn) => match plan.body_of(arn) {
            Some(ResourceBody::PrefixList(l)) => {
                Ok(l.entries.iter().map(|e| typed("ip_net", e)).collect())
            }
            Some(_) | None => {
                Err(format!("prefix list {arn} was not collected"))
            }
        },
        Peer::Group(arn) => {
            let names: Vec<Expression> = plan
                .group_members(arn)
                .iter()
                .filter_map(|i| plan.instances.get(i))
                .map(|n| typed("instance", n))
                .collect();
            if names.is_empty() {
                Err(format!(
                    "references security group {}, none of whose instances are in this workload",
                    plan.name_of(arn)
                ))
            } else {
                Ok(names)
            }
        }
        Peer::Tag(t) => Err(format!(
            "matches instances by network tag {t}; add them as hosts by hand"
        )),
        Peer::ServiceAccount(a) => Err(format!(
            "matches instances by service account {a}; add them as hosts by hand"
        )),
        Peer::ServiceTag(t) => match t.to_ascii_lowercase().as_str() {
            "internet" => Ok(vec![typed("ip_net", "0.0.0.0/0")]),
            "virtualnetwork" => Ok(vec![typed("vpc", vpc)]),
            _ => Err(format!("service tag {t} has no fixed addresses")),
        },
    }
}

fn protocols(
    rule: &body::SecurityGroupRule,
) -> Result<Option<Expression>, String> {
    let named = |p: &str| -> Expression {
        let mut fields = vec![("type", Expression::from(p))];
        if let Some((t, code)) = rule.icmp {
            fields.push(("icmp_type", number(i64::from(t))));
            if let Some(c) = code {
                fields.push(("icmp_code", c.to_string().into()));
            }
        }
        Expression::from(vec![object(fields)])
    };
    match &rule.protocol {
        Protocol::All => Ok(None),
        Protocol::Tcp => Ok(Some(named("tcp"))),
        Protocol::Udp => Ok(Some(named("udp"))),
        Protocol::Icmp => Ok(Some(named("icmp"))),
        Protocol::Icmp6 => Ok(Some(named("icmp6"))),
        Protocol::Other(p) => {
            Err(format!("filters protocol {p}, which Oxide cannot"))
        }
    }
}

/// One `oxide_vpc_firewall_rules` per VPC, holding a rule per collected
/// rule of each security group the pair's instances are in, targeting
/// those instances.
fn render_firewalls(
    plan: &mut Plan<'_>,
    instances: &[(&Resource, &body::Instance)],
) {
    let groups: BTreeSet<&str> = plan
        .input
        .relationships
        .iter()
        .filter(|r| {
            r.edge == Edge::HasSecurityGroup
                && plan.instances.contains_key(r.from_arn.as_str())
        })
        .map(|r| r.to_arn.as_str())
        .collect();
    let network_of: HashMap<&str, &str> = instances
        .iter()
        .filter_map(|(r, i)| Some((r.arn.as_str(), i.network_id.as_deref()?)))
        .collect();
    let mut rules_by_group: BTreeMap<
        &str,
        Vec<(&str, &body::SecurityGroupRule)>,
    > = BTreeMap::new();
    for r in plan.input.resources {
        if let ResourceBody::SecurityGroupRule(rule) = &r.body {
            if groups.contains(rule.group.as_str()) {
                rules_by_group
                    .entry(rule.group.as_str())
                    .or_default()
                    .push((r.arn.as_str(), rule));
            }
        }
    }
    // VPC to its rules, by Oxide rule name.
    let mut per_vpc: BTreeMap<String, Vec<(String, Expression)>> =
        BTreeMap::new();
    let mut names: HashMap<String, Names> = HashMap::new();
    for (group, rules) in rules_by_group {
        let members = plan.group_members(group);
        // A group's members can span VPCs; each VPC targets its own.
        let mut by_vpc: BTreeMap<String, Vec<Expression>> = BTreeMap::new();
        for m in &members {
            let vpc = network_of.get(m).and_then(|n| plan.vpcs.get(n));
            if let (Some(vpc), Some(name)) = (vpc, plan.instances.get(m)) {
                by_vpc
                    .entry(vpc.clone())
                    .or_default()
                    .push(typed("instance", name));
            }
        }
        let group_name = oxide_name(&plan.name_of(group), "sg");
        for (vpc, targets) in by_vpc {
            for (i, (arn, rule)) in rules.iter().enumerate() {
                let hosts = match hosts(plan, &rule.peer, &vpc) {
                    Ok(h) => h,
                    Err(why) => {
                        plan.findings.push(format!(
                            "rule {} of {}: {why}; left out",
                            last_segment(arn),
                            plan.name_of(group)
                        ));
                        continue;
                    }
                };
                let protocols = match protocols(rule) {
                    Ok(p) => p,
                    Err(why) => {
                        plan.findings.push(format!(
                            "rule {} of {}: {why}; left out",
                            last_segment(arn),
                            plan.name_of(group)
                        ));
                        continue;
                    }
                };
                let mut filters = vec![("hosts", Expression::from(hosts))];
                if let Some((from, to)) = rule.ports {
                    let range = if from == to {
                        from.to_string()
                    } else {
                        format!("{from}-{to}")
                    };
                    filters.push(("ports", Expression::from(vec![range])));
                }
                if let Some(p) = protocols {
                    filters.push(("protocols", p));
                }
                let name = names.entry(vpc.clone()).or_default().unique(
                    oxide_name(&format!("{group_name}-{}", i + 1), "rule"),
                );
                let value = object(vec![
                    (
                        "action",
                        match rule.action {
                            RuleAction::Allow => "allow",
                            RuleAction::Deny => "deny",
                        }
                        .into(),
                    ),
                    (
                        "direction",
                        match rule.direction {
                            TrafficDirection::Inbound => "inbound",
                            TrafficDirection::Outbound => "outbound",
                        }
                        .into(),
                    ),
                    (
                        "priority",
                        number(i64::from(
                            rule.priority.unwrap_or(UNORDERED_PRIORITY),
                        )),
                    ),
                    ("status", "enabled".into()),
                    (
                        "description",
                        rule.description
                            .clone()
                            .unwrap_or_else(|| format!("From {arn}"))
                            .into(),
                    ),
                    ("targets", Expression::from(targets.clone())),
                    ("filters", object(filters)),
                ]);
                per_vpc.entry(vpc.clone()).or_default().push((name, value));
            }
        }
    }
    for (vpc, rules) in per_vpc {
        let map: Object<ObjectKey, Expression> = rules
            .into_iter()
            .map(|(k, v)| (ObjectKey::Expression(k.into()), v))
            .collect();
        plan.blocks.push(
            resource("oxide_vpc_firewall_rules", &vpc)
                .add_attribute(("vpc_id", id_of("oxide_vpc", &vpc)))
                .add_attribute(("rules", Expression::from(map)))
                .build(),
        );
    }
}

/// Whether a route is the default route to the internet, which every
/// Oxide VPC's system router already has.
fn is_default_internet(route: &body::Route) -> bool {
    let default =
        matches!(route.destination.as_deref(), Some("0.0.0.0/0" | "::/0"));
    let internet = match &route.target {
        RouteTarget::InternetGateway(_)
        | RouteTarget::Internet
        | RouteTarget::NatGateway(_) => true,
        RouteTarget::Local
        | RouteTarget::Drop
        | RouteTarget::Instance(_)
        | RouteTarget::Ip(_)
        | RouteTarget::Peering(_)
        | RouteTarget::Other(_) => false,
    };
    default && internet
}

/// A custom router for each route table of a rendered VPC that has
/// routes beyond the VPC itself and the default route.
fn render_routers(plan: &mut Plan<'_>) {
    let mut router_names: HashMap<String, Names> = HashMap::new();
    let mut tables: Vec<(&str, &body::RouteTable)> = Vec::new();
    for r in plan.input.resources {
        if let ResourceBody::RouteTable(t) = &r.body {
            if t.network_id
                .as_deref()
                .is_some_and(|n| plan.vpcs.contains_key(n))
            {
                tables.push((r.arn.as_str(), t));
            }
        }
    }
    tables.sort_by_key(|(arn, _)| *arn);
    for (arn, table) in tables {
        let Some(vpc) =
            table.network_id.as_deref().and_then(|n| plan.vpcs.get(n)).cloned()
        else {
            continue;
        };
        let mut routes = Vec::new();
        for route in &table.routes {
            if route.target == RouteTarget::Local || is_default_internet(route)
            {
                continue;
            }
            let destination = route.destination.as_deref().unwrap_or_default();
            if !destination.contains('/') {
                plan.findings.push(format!(
                    "route to {destination} in {}: only CIDR destinations translate; left out",
                    plan.name_of(arn)
                ));
                continue;
            }
            let target = match &route.target {
                RouteTarget::Ip(ip) => typed("ip", ip),
                RouteTarget::InternetGateway(_)
                | RouteTarget::Internet
                | RouteTarget::NatGateway(_) => {
                    typed("internet_gateway", DEFAULT_GATEWAY)
                }
                RouteTarget::Drop => object(vec![("type", "drop".into())]),
                RouteTarget::Instance(i) => {
                    match plan.instances.get(i.as_str()) {
                        Some(name) => typed("instance", name),
                        None => {
                            plan.findings.push(format!(
                            "route to {destination} in {}: its next hop {} is not in this workload; left out",
                            plan.name_of(arn),
                            plan.name_of(i)
                        ));
                            continue;
                        }
                    }
                }
                RouteTarget::Peering(p) | RouteTarget::Other(p) => {
                    plan.findings.push(format!(
                        "route to {destination} in {} goes through {}, which Oxide has no equivalent for; left out",
                        plan.name_of(arn),
                        last_segment(p)
                    ));
                    continue;
                }
                RouteTarget::Local => continue,
            };
            routes.push((destination.to_string(), target));
        }
        if routes.is_empty() {
            continue;
        }
        let router = router_names
            .entry(vpc.clone())
            .or_default()
            .unique(oxide_name(&plan.name_of(arn), "router"));
        let label = format!("{vpc}-{router}");
        plan.blocks.push(
            resource("oxide_vpc_router", &label)
                .add_attribute(("vpc_id", id_of("oxide_vpc", &vpc)))
                .add_attribute(("name", router.clone()))
                .add_attribute(("description", format!("From {arn}")))
                .build(),
        );
        let mut route_names = Names::default();
        for (destination, target) in routes {
            let name = route_names
                .unique(oxide_name(&format!("to-{destination}"), "route"));
            plan.blocks.push(
                resource("oxide_vpc_router_route", &format!("{label}-{name}"))
                    .add_attribute((
                        "vpc_router_id",
                        id_of("oxide_vpc_router", &label),
                    ))
                    .add_attribute(("name", name))
                    .add_attribute(("description", format!("From {arn}")))
                    .add_attribute((
                        "destination",
                        typed("ip_net", &destination),
                    ))
                    .add_attribute(("target", target))
                    .build(),
            );
        }
        let attached: Vec<String> = table
            .subnets
            .iter()
            .filter_map(|s| plan.subnets.get(s.as_str()).cloned())
            .collect();
        plan.findings.push(format!(
            "router {router} in VPC {vpc}: the provider cannot attach it to subnets; attach it to {} with `oxide vpc subnet update --custom-router`",
            if attached.is_empty() {
                "the subnets that should use it".to_string()
            } else {
                attached.join(", ")
            }
        ));
    }
}

fn render_disks<'a>(
    plan: &mut Plan<'a>,
    volumes: &[(&'a Resource, &'a body::Volume)],
    project_id: &Expression,
) {
    let mut names = Names::default();
    for (r, v) in volumes {
        let arn = r.arn.as_str();
        let name = names.unique(oxide_name(&plan.name_of(arn), "disk"));
        let Some(gib) = v.size_gib else {
            plan.findings.push(format!("disk {name}: size unknown; left out"));
            continue;
        };
        if gib > MAX_DISK_GIB {
            plan.findings.push(format!(
                "disk {name} is {gib} GiB; Oxide disks are at most {MAX_DISK_GIB} GiB, so split it"
            ));
        }
        plan.blocks.push(
            resource("oxide_disk", &name)
                .add_attribute(("project_id", project_id.clone()))
                .add_attribute(("name", name.clone()))
                .add_attribute(("description", format!("From {arn}")))
                .add_attribute(("size", number(i64::from(gib) * GIB)))
                .add_attribute(("block_size", number(512)))
                .build(),
        );
        plan.disks.insert(arn, name);
    }
}

/// Floating IPs, by the instance each is attached to, with its old
/// address.
fn render_floating_ips<'a>(
    plan: &mut Plan<'a>,
    public_ips: &[(&'a Resource, &'a body::PublicIp)],
    project_id: &Expression,
) -> BTreeMap<&'a str, Vec<(String, Option<&'a str>)>> {
    let mut names = Names::default();
    let mut by_instance: BTreeMap<&str, Vec<(String, Option<&str>)>> =
        BTreeMap::new();
    for (r, ip) in public_ips {
        let name =
            names.unique(oxide_name(&plan.name_of(r.arn.as_str()), "ip"));
        plan.blocks.push(
            resource("oxide_floating_ip", &name)
                .add_attribute(("project_id", project_id.clone()))
                .add_attribute(("name", name.clone()))
                .add_attribute((
                    "description",
                    format!(
                        "Was {} ({})",
                        ip.public_ip.as_deref().unwrap_or("unknown"),
                        r.arn
                    ),
                ))
                .build(),
        );
        if let Some(instance) = ip.instance.as_deref() {
            by_instance
                .entry(instance)
                .or_default()
                .push((name, ip.public_ip.as_deref()));
        }
    }
    by_instance
}

fn render_instances(
    plan: &mut Plan<'_>,
    instances: &[(&Resource, &body::Instance)],
    floating: &BTreeMap<&str, Vec<(String, Option<&str>)>>,
    project_id: &Expression,
) {
    let types: HashMap<(&str, &str), &body::InstanceType> = plan
        .input
        .resources
        .iter()
        .filter_map(|r| {
            let ResourceBody::InstanceType(t) = &r.body else { return None };
            Some(((r.region.as_str(), t.name.as_str()), t))
        })
        .collect();
    for (r, i) in instances {
        let arn = r.arn.as_str();
        let Some(name) = plan.instances.get(arn).cloned() else { continue };
        let size = i
            .instance_type
            .as_deref()
            .and_then(|t| types.get(&(r.region.as_str(), t)))
            .and_then(|t| Some((t.vcpus?, t.memory_mib?)));
        let Some((vcpus, memory_mib)) = size else {
            plan.findings.push(format!(
                "instance {name}: the size of {} is unknown; left out",
                i.instance_type.as_deref().unwrap_or("its type")
            ));
            continue;
        };
        if vcpus > MAX_VCPUS || memory_mib > MAX_MEMORY_MIB {
            plan.findings.push(format!(
                "instance {name}: {vcpus} vCPUs and {} GiB exceed Oxide's {MAX_VCPUS} vCPUs and 1.5 TiB",
                memory_mib / 1024
            ));
        }
        if i.local_disk_count > 0 {
            plan.findings.push(format!(
                "instance {name}: {} local disks are not migrated",
                i.local_disk_count
            ));
        }
        if let Some(image) = &i.image_id {
            plan.findings.push(format!(
                "instance {name}: boots from image {}; convert its boot disk to a raw image and import it",
                last_segment(image)
            ));
        }
        let disks: Vec<(&str, &String)> = plan
            .disks
            .iter()
            .filter(|(d, _)| {
                matches!(plan.body_of(d), Some(ResourceBody::Volume(v)) if v.attached_to.iter().any(|a| a == arn))
            })
            .map(|(d, n)| (*d, n))
            .collect();
        if disks.len() > MAX_DISKS {
            plan.findings.push(format!(
                "instance {name}: {} disks exceed Oxide's {MAX_DISKS}",
                disks.len()
            ));
        }
        let mut nics = Vec::new();
        for (n, subnet) in i.subnets.iter().enumerate() {
            let vpc = plan
                .body_of(subnet)
                .and_then(|b| {
                    let ResourceBody::Subnet(s) = b else { return None };
                    s.network_id.as_deref()
                })
                .and_then(|v| plan.vpcs.get(v));
            let (Some(subnet_name), Some(vpc)) =
                (plan.subnets.get(subnet.as_str()), vpc)
            else {
                continue;
            };
            nics.push(object(vec![
                ("name", format!("nic{n}").into()),
                ("description", format!("From {subnet}").into()),
                ("vpc_id", id_of("oxide_vpc", vpc)),
                ("subnet_id", id_of("oxide_vpc_subnet", subnet_name)),
                (
                    "ip_config",
                    object(vec![("v4", object(vec![("ip", "auto".into())]))]),
                ),
            ]));
        }
        if nics.len() > MAX_NICS {
            plan.findings.push(format!(
                "instance {name}: {} network interfaces exceed Oxide's {MAX_NICS}",
                nics.len()
            ));
        }
        let mut block = resource("oxide_instance", &name)
            .add_attribute(("project_id", project_id.clone()))
            .add_attribute(("name", name.clone()))
            .add_attribute(("hostname", name.clone()))
            .add_attribute(("description", format!("From {arn}")))
            .add_attribute(("ncpus", number(i64::from(vcpus))))
            .add_attribute(("memory", number(memory_mib * MIB)))
            .add_attribute((
                "disk_attachments",
                Expression::from(
                    disks
                        .iter()
                        .map(|(_, n)| id_of("oxide_disk", n))
                        .collect::<Vec<_>>(),
                ),
            ));
        if let Some(boot) =
            i.boot_disk.as_deref().and_then(|b| plan.disks.get(b))
        {
            block = block
                .add_attribute(("boot_disk_id", id_of("oxide_disk", boot)));
        }
        if !nics.is_empty() {
            block = block
                .add_attribute(("network_interfaces", Expression::from(nics)));
        }
        let own_floating =
            floating.get(arn).map(Vec::as_slice).unwrap_or_default();
        // A static address is the floating IP; any other public address
        // was ephemeral.
        let ephemeral = i.public_ip.as_deref().is_some_and(|ip| {
            !own_floating.iter().any(|(_, addr)| *addr == Some(ip))
        });
        if ephemeral || !own_floating.is_empty() {
            let mut ips = Vec::new();
            if ephemeral {
                ips.push((
                    "ephemeral",
                    Expression::from(vec![object(vec![(
                        "ip_version",
                        "v4".into(),
                    )])]),
                ));
            }
            if !own_floating.is_empty() {
                ips.push((
                    "floating",
                    Expression::from(
                        own_floating
                            .iter()
                            .map(|(n, _)| {
                                object(vec![(
                                    "id",
                                    id_of("oxide_floating_ip", n),
                                )])
                            })
                            .collect::<Vec<_>>(),
                    ),
                ));
            }
            block = block.add_attribute(("external_ips", object(ips)));
        }
        plan.blocks
            .push(block.add_attribute(("start_on_create", false)).build());
    }
}

#[cfg(test)]
mod tests;
