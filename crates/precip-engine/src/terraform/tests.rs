use super::*;
use precip_types::scan::ResourceKind;
use precip_types::{EnvName, GroupingSignal, WorkloadName};

const P: &str = "arn:aws:ec2:us-east-1:111111111111";

fn arn(kind: &str, id: &str) -> String {
    format!("{P}:{kind}/{id}")
}

fn res(
    arn: &str,
    kind: ResourceKind,
    tags: &[(&str, &str)],
    body: serde_json::Value,
) -> Resource {
    Resource {
        arn: arn.into(),
        account_id: "111111111111".into(),
        region: "us-east-1".into(),
        tags: tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: ResourceBody::from_value(kind, body)
            .expect("fixture matches its kind"),
    }
}

fn rule(group: &str, id: &str, body: serde_json::Value) -> Resource {
    let mut v = serde_json::json!({
        "group": arn("security-group", group),
        "direction": "inbound", "action": "allow", "priority": null,
        "protocol": "tcp", "ports": null, "icmp": null,
        "peer": {"ip_net": "0.0.0.0/0"}, "description": null,
    });
    if let (Some(o), Some(e)) = (v.as_object_mut(), body.as_object()) {
        o.extend(e.clone());
    }
    res(
        &arn("security-group-rule", id),
        ResourceKind::SecurityGroupRule,
        &[],
        v,
    )
}

fn rel(from: &str, edge: Edge, to: &str) -> Relationship {
    Relationship { from_arn: from.into(), edge, to_arn: to.into() }
}

fn pair() -> PairKey {
    PairKey {
        workload: WorkloadName::from("shop"),
        environment: EnvName::from("prod"),
    }
}

struct Fixture {
    resources: Vec<Resource>,
    relationships: Vec<Relationship>,
    members: Vec<WorkloadMember>,
}

fn fixture() -> Fixture {
    let vpc = arn("vpc", "vpc-1");
    let subnet = arn("subnet", "subnet-1");
    let web = arn("instance", "i-web");
    let db = arn("instance", "i-db");
    let other = arn("instance", "i-other");
    let instance = |name: &str, extra: serde_json::Value| {
        let mut v = serde_json::json!({
            "name": name, "instance_type": "m5.large", "network_id": vpc,
            "subnets": [subnet], "network_interface_count": 1, "volume_count": 1,
        });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        v
    };
    let resources = vec![
        res(
            &vpc,
            ResourceKind::Network,
            &[("Name", "Shop VPC")],
            serde_json::json!({}),
        ),
        res(
            &subnet,
            ResourceKind::Subnet,
            &[("Name", "app")],
            serde_json::json!({
                "network_id": vpc, "cidr_block": "10.0.1.0/24", "ipv6_cidr_blocks": ["2600:1f18::/64"],
            }),
        ),
        res(
            &format!("{P}::instance-type/m5.large"),
            ResourceKind::InstanceType,
            &[],
            serde_json::json!({
                "name": "m5.large", "vcpus": 2, "memory_mib": 8192,
            }),
        ),
        res(
            &web,
            ResourceKind::Instance,
            &[],
            instance(
                "web-1",
                serde_json::json!({
                    "boot_disk": arn("volume", "vol-web"), "public_ip": "3.3.3.3",
                    "image_id": arn("image", "ami-1"), "local_disk_count": 1,
                }),
            ),
        ),
        res(
            &db,
            ResourceKind::Instance,
            &[],
            instance("db-1", serde_json::json!({"public_ip": "4.4.4.4"})),
        ),
        res(
            &other,
            ResourceKind::Instance,
            &[],
            instance("other-1", serde_json::json!({})),
        ),
        res(
            &arn("volume", "vol-web"),
            ResourceKind::Volume,
            &[],
            serde_json::json!({
                "name": "web-root", "size_gib": 20, "attached_to": [web],
            }),
        ),
        res(
            &arn("volume", "vol-db"),
            ResourceKind::Volume,
            &[],
            serde_json::json!({
                "size_gib": 2000, "attached_to": [db],
            }),
        ),
        res(
            &arn("elastic-ip", "eipalloc-1"),
            ResourceKind::PublicIp,
            &[],
            serde_json::json!({
                "name": "db-ip", "public_ip": "4.4.4.4", "instance": db,
            }),
        ),
        res(
            &arn("security-group", "sg-web"),
            ResourceKind::SecurityGroup,
            &[],
            serde_json::json!({"name": "web"}),
        ),
        res(
            &arn("security-group", "sg-db"),
            ResourceKind::SecurityGroup,
            &[],
            serde_json::json!({"name": "db"}),
        ),
        rule(
            "sg-web",
            "r-https",
            serde_json::json!({"ports": [443, 443], "description": "HTTPS"}),
        ),
        rule(
            "sg-web",
            "r-out",
            serde_json::json!({"direction": "outbound", "protocol": "all"}),
        ),
        rule(
            "sg-db",
            "r-pg",
            serde_json::json!({
                "ports": [5432, 5432], "peer": {"group": arn("security-group", "sg-web")},
            }),
        ),
        rule("sg-db", "r-tag", serde_json::json!({"peer": {"tag": "bastion"}})),
        rule(
            "sg-db",
            "r-gre",
            serde_json::json!({"protocol": {"other": "47"}}),
        ),
        rule(
            "sg-db",
            "r-ping",
            serde_json::json!({"protocol": "icmp", "icmp": [8, null], "priority": 10, "action": "deny"}),
        ),
        res(
            &arn("route-table", "rtb-1"),
            ResourceKind::RouteTable,
            &[("Name", "main")],
            serde_json::json!({
                "network_id": vpc, "main": true, "subnets": [subnet],
                "routes": [
                    {"destination": "10.0.0.0/16", "target": "local"},
                    {"destination": "0.0.0.0/0", "target": {"internet_gateway": arn("internet-gateway", "igw-1")}},
                    {"destination": "10.9.0.0/16", "target": {"ip": "10.0.1.5"}},
                    {"destination": "172.16.0.0/12", "target": {"peering": arn("vpc-peering-connection", "pcx-1")}},
                ],
            }),
        ),
        res(
            "arn:aws:elasticloadbalancing:us-east-1:111111111111:loadbalancer/app/shop/1",
            ResourceKind::LoadBalancer,
            &[],
            serde_json::json!({"name": "shop-alb"}),
        ),
        res(
            "arn:aws:rds:us-east-1:111111111111:db:orders",
            ResourceKind::ManagedService,
            &[],
            serde_json::json!({"name": "orders", "service": "database", "product": "rds",
                               "engine": null, "engine_version": null, "instance_class": null,
                               "allocated_storage_gib": null, "high_availability": null, "network_id": null}),
        ),
    ];
    let relationships = vec![
        rel(&web, Edge::HasSecurityGroup, &arn("security-group", "sg-web")),
        rel(&db, Edge::HasSecurityGroup, &arn("security-group", "sg-db")),
        rel(&other, Edge::HasSecurityGroup, &arn("security-group", "sg-web")),
    ];
    let member = |a: &str, pair: PairKey| WorkloadMember {
        arn: a.into(),
        pair,
        signal: GroupingSignal::Tag,
    };
    let in_pair = [
        web.clone(),
        db.clone(),
        arn("volume", "vol-web"),
        arn("volume", "vol-db"),
        arn("elastic-ip", "eipalloc-1"),
        "arn:aws:elasticloadbalancing:us-east-1:111111111111:loadbalancer/app/shop/1".into(),
        "arn:aws:rds:us-east-1:111111111111:db:orders".into(),
    ];
    let mut members: Vec<WorkloadMember> =
        in_pair.iter().map(|a| member(a, pair())).collect();
    members.push(member(
        &other,
        PairKey {
            workload: WorkloadName::from("other"),
            environment: EnvName::from("prod"),
        },
    ));
    Fixture { resources, relationships, members }
}

fn rendered() -> String {
    let f = fixture();
    render(&Input {
        resources: &f.resources,
        relationships: &f.relationships,
        members: &f.members,
        pair: &pair(),
    })
}

/// The block of `kind` labelled `label`, as HCL parsed back.
fn block(body: &Body, kind: &str, label: &str) -> Block {
    body.blocks()
        .find(|b| {
            b.labels.first().map(|l| l.as_str()) == Some(kind)
                && b.labels.get(1).map(|l| l.as_str()) == Some(label)
        })
        .cloned()
        .unwrap_or_else(|| panic!("no {kind}.{label}"))
}

fn attr<'b>(b: &'b Block, key: &str) -> &'b Expression {
    b.body
        .attributes()
        .find(|a| a.key.as_str() == key)
        .map(|a| &a.expr)
        .unwrap_or_else(|| panic!("no {key}"))
}

/// A block as the file writes it.
fn text(b: &Block) -> String {
    hcl::to_string(&Body::builder().add_block(b.clone()).build()).unwrap()
}

fn parsed() -> Body {
    hcl::from_str(&rendered()).expect("the output is valid HCL")
}

#[test]
fn a_pair_becomes_a_project_with_its_networks_disks_and_instances() {
    let body = parsed();
    let kinds: BTreeSet<String> = body
        .blocks()
        .filter_map(|b| b.labels.first().map(|l| l.as_str().to_string()))
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from(
            [
                "oxide_project",
                "oxide_vpc",
                "oxide_vpc_subnet",
                "oxide_vpc_firewall_rules",
                "oxide_vpc_router",
                "oxide_vpc_router_route",
                "oxide_disk",
                "oxide_floating_ip",
                "oxide_instance",
            ]
            .map(String::from)
        )
    );
    let project = block(&body, "oxide_project", "shop-prod");
    assert_eq!(attr(&project, "name"), &Expression::from("shop-prod"));
    let vpc = block(&body, "oxide_vpc", "shop-vpc");
    assert_eq!(attr(&vpc, "dns_name"), &Expression::from("shop-vpc"));
    let subnet = block(&body, "oxide_vpc_subnet", "app");
    assert_eq!(attr(&subnet, "ipv4_block"), &Expression::from("10.0.1.0/24"));
    assert_eq!(
        body.blocks()
            .filter(|b| b.labels.first().map(|l| l.as_str())
                == Some("oxide_instance"))
            .count(),
        2,
        "the other pair's instance is left out"
    );
}

#[test]
fn instances_take_their_size_disks_nics_and_addresses() {
    let body = parsed();
    let web = block(&body, "oxide_instance", "web-1");
    assert_eq!(attr(&web, "ncpus"), &number(2));
    assert_eq!(attr(&web, "memory"), &number(8192 * MIB));
    assert_eq!(attr(&web, "boot_disk_id"), &id_of("oxide_disk", "web-root"));
    assert_eq!(attr(&web, "start_on_create"), &Expression::from(false));
    let out = rendered();
    assert!(out.contains("ephemeral"), "web-1's address was not a static one");
    let db = block(&body, "oxide_instance", "db-1");
    let ips = text(&db);
    assert!(ips.contains("oxide_floating_ip.db-ip.id"), "{ips}");
    assert!(
        !ips.contains("ephemeral"),
        "db-1's address is its floating IP: {ips}"
    );
    let nics = text(&db);
    assert!(
        nics.contains("oxide_vpc_subnet.app.id")
            && nics.contains("oxide_vpc.shop-vpc.id"),
        "{nics}"
    );
    let disk = block(&body, "oxide_disk", "web-root");
    assert_eq!(attr(&disk, "size"), &number(20 * GIB));
}

#[test]
fn security_groups_become_rules_targeting_their_instances() {
    let body = parsed();
    let fw = block(&body, "oxide_vpc_firewall_rules", "shop-vpc");
    let rules = text(&fw);
    for name in ["web-1", "web-2", "db-1", "db-4"] {
        assert!(
            rules.contains(&format!("\"{name}\"")),
            "no rule {name} in {rules}"
        );
    }
    assert!(
        !rules.contains("\"db-2\"") && !rules.contains("\"db-3\""),
        "the tag and GRE rules are left out"
    );
    assert!(rules.contains("\"443\""), "{rules}");
    assert!(
        rules.contains("value = \"web-1\""),
        "group peers become instance hosts: {rules}"
    );
    assert!(rules.contains("icmp_type = 8"), "{rules}");
    assert!(
        rules.contains("priority = 10") && rules.contains("priority = 1000"),
        "{rules}"
    );
    assert!(
        !rules.contains("other-1"),
        "instances outside the pair are not targets"
    );
}

#[test]
fn only_routes_beyond_the_default_get_a_custom_router() {
    let body = parsed();
    let routes: Vec<Block> = body
        .blocks()
        .filter(|b| {
            b.labels.first().map(|l| l.as_str())
                == Some("oxide_vpc_router_route")
        })
        .cloned()
        .collect();
    assert_eq!(
        routes.len(),
        1,
        "local, default and peering routes are not rendered"
    );
    let target = text(&routes[0]);
    assert!(
        target.contains("\"ip\"") && target.contains("10.0.1.5"),
        "{target}"
    );
}

#[test]
fn what_does_not_translate_is_listed_in_the_header() {
    let out = rendered();
    let header: String = out
        .lines()
        .take_while(|l| l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "load balancer shop-alb",
        "rds orders (database)",
        "network tag bastion",
        "protocol 47",
        "goes through pcx-1",
        "router main in VPC shop-vpc",
        "attach it to app",
        "2000 GiB",
        "1 local disks",
        "image ami-1",
        "IPv6 ranges",
    ] {
        assert!(
            header.contains(expected),
            "no finding {expected:?} in\n{header}"
        );
    }
}

#[test]
fn a_pair_without_resources_still_renders_its_project() {
    let f = fixture();
    let out = render(&Input {
        resources: &f.resources,
        relationships: &f.relationships,
        members: &[],
        pair: &pair(),
    });
    let body: Body = hcl::from_str(&out).unwrap();
    assert_eq!(body.blocks().count(), 1);
}

#[test]
fn names_follow_oxides_rules() {
    assert_eq!(oxide_name("Shop VPC", "vpc"), "shop-vpc");
    assert_eq!(oxide_name("  --x__y--  ", "vpc"), "x-y");
    assert_eq!(oxide_name("10.0.0.0/8", "route"), "route-10-0-0-0-8");
    assert_eq!(oxide_name("", "disk"), "disk");
    assert_eq!(oxide_name(&"a".repeat(80), "x").len(), NAME_MAX);
    let mut names = Names::default();
    assert_eq!(names.unique("web".into()), "web");
    assert_eq!(names.unique("web".into()), "web-2");
    assert_eq!(names.unique("web".into()), "web-3");
    let mut long = Names::default();
    let base = "b".repeat(NAME_MAX);
    long.unique(base.clone());
    assert!(long.unique(base).len() <= NAME_MAX);
}

#[test]
fn private_ranges_and_default_routes_are_recognised() {
    assert!(is_rfc1918("10.1.0.0/16"));
    assert!(is_rfc1918("172.31.0.0/16"));
    assert!(!is_rfc1918("172.32.0.0/16"));
    assert!(is_rfc1918("192.168.0.0/24"));
    assert!(!is_rfc1918("100.64.0.0/10"));
    assert!(!is_rfc1918("fd00::/8"));
    let route = |d: &str, t: RouteTarget| body::Route {
        destination: Some(d.into()),
        target: t,
        state: None,
    };
    assert!(is_default_internet(&route("0.0.0.0/0", RouteTarget::Internet)));
    assert!(is_default_internet(&route(
        "::/0",
        RouteTarget::NatGateway("n".into())
    )));
    assert!(!is_default_internet(&route(
        "0.0.0.0/0",
        RouteTarget::Ip("10.0.0.1".into())
    )));
    assert!(!is_default_internet(&route("10.0.0.0/8", RouteTarget::Internet)));
}
