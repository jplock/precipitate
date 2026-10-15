use super::*;
use precip_types::scan::Relationship;

const ACCT: &str = "123456789012";
const I1: &str = "arn:aws:ec2:us-east-1:123456789012:instance/i-1";
const I2: &str = "arn:aws:ec2:us-east-1:123456789012:instance/i-2";
const I3: &str = "arn:aws:ec2:us-east-1:123456789012:instance/i-3";
const V1: &str = "arn:aws:ec2:us-east-1:123456789012:volume/vol-1";

fn res(
    arn: &str,
    kind: ResourceKind,
    tags: &[(&str, &str)],
    body: serde_json::Value,
) -> Resource {
    Resource {
        arn: arn.into(),
        account_id: ACCT.into(),
        region: "us-east-1".into(),
        tags: tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: precip_types::Body::from_value(kind, body)
            .expect("fixture body matches its kind"),
    }
}

fn inst(id: &str, tags: &[(&str, &str)]) -> Resource {
    res(
        &format!("arn:aws:ec2:us-east-1:{ACCT}:instance/{id}"),
        ResourceKind::Instance,
        tags,
        serde_json::json!({ "instance_type": "t3.micro", "network_id": "vpc-1" }),
    )
}

fn rel(from: &str, edge: Edge, to: &str) -> Relationship {
    Relationship { from_arn: from.into(), edge, to_arn: to.into() }
}

fn run(
    resources: &[Resource],
    relationships: &[Relationship],
) -> Vec<WorkloadMember> {
    run_with(resources, relationships, &BTreeMap::new(), &BTreeMap::new())
}

fn run_with(
    resources: &[Resource],
    relationships: &[Relationship],
    overrides: &BTreeMap<Arn, PairKey>,
    aliases: &BTreeMap<WorkloadName, WorkloadName>,
) -> Vec<WorkloadMember> {
    group(
        &Input { resources, relationships, overrides, aliases },
        &GroupingConfig::default(),
    )
}

fn find<'a>(m: &'a [WorkloadMember], arn: &str) -> &'a WorkloadMember {
    m.iter()
        .find(|x| x.arn == arn)
        .unwrap_or_else(|| panic!("{arn} not placed"))
}

fn wl(m: &[WorkloadMember], arn: &str) -> (String, String, GroupingSignal) {
    let p = find(m, arn);
    (p.pair.workload.to_string(), p.pair.environment.to_string(), p.signal)
}

#[test]
fn override_tag_beats_stack_and_conventional_tags() {
    let m = run(
        &[inst(
            "i-1",
            &[
                (OVERRIDE_TAG, "Billing"),
                (
                    STACK_ID_TAG,
                    "arn:aws:cloudformation:us-east-1:1:stack/other/x",
                ),
                ("Application", "other"),
            ],
        )],
        &[],
    );
    assert_eq!(
        wl(&m, I1),
        ("billing".into(), "default".into(), GroupingSignal::OverrideTag)
    );
}

#[test]
fn nested_stack_resolves_to_root_and_strips_environment() {
    let root = "arn:aws:cloudformation:us-east-1:1:stack/billing-prod/root";
    let child =
        "arn:aws:cloudformation:us-east-1:1:stack/billing-prod-db/child";
    let m = run(
        &[
            res(
                root,
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": "billing-prod", "root_id": null }),
            ),
            res(
                child,
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": "billing-prod-db", "root_id": root }),
            ),
            inst("i-1", &[(STACK_ID_TAG, child)]),
        ],
        &[],
    );
    assert_eq!(
        wl(&m, I1),
        ("billing".into(), "prod".into(), GroupingSignal::DeploymentGroup)
    );
}

#[test]
fn asg_members_and_the_group_itself_share_a_workload() {
    let asg = "arn:aws:autoscaling:us-east-1:1:autoScalingGroup:x:autoScalingGroupName/web-staging";
    let m = run(
        &[
            res(
                asg,
                ResourceKind::ScaleGroup,
                &[],
                serde_json::json!({ "name": "web-staging" }),
            ),
            inst("i-1", &[]),
            inst("i-2", &[]),
        ],
        &[
            rel(I1, Edge::MemberOfScaleGroup, asg),
            rel(I2, Edge::MemberOfScaleGroup, asg),
        ],
    );
    assert_eq!(
        wl(&m, I1),
        ("web".into(), "staging".into(), GroupingSignal::ScaleGroup)
    );
    assert_eq!(wl(&m, I2).0, "web");
    assert_eq!(
        wl(&m, asg),
        ("web".into(), "staging".into(), GroupingSignal::Attached)
    );
}

#[test]
fn asg_created_by_a_stack_groups_under_the_stack() {
    let stack = "arn:aws:cloudformation:us-east-1:1:stack/api/x";
    let asg = "arn:aws:autoscaling:us-east-1:1:autoScalingGroup:x:autoScalingGroupName/api-asg";
    let m = run(
        &[
            res(
                stack,
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": "api", "root_id": null }),
            ),
            res(
                asg,
                ResourceKind::ScaleGroup,
                &[(STACK_ID_TAG, stack)],
                serde_json::json!({ "name": "api-asg" }),
            ),
            inst("i-1", &[("Environment", "Production")]),
        ],
        &[rel(I1, Edge::MemberOfScaleGroup, asg)],
    );
    assert_eq!(
        wl(&m, I1),
        ("api".into(), "prod".into(), GroupingSignal::DeploymentGroup)
    );
}

#[test]
fn load_balancer_names_the_workload_and_inherits_it_back() {
    let lb =
        "arn:aws:elasticloadbalancing:us-east-1:1:loadbalancer/app/shop-prod/x";
    let tg = "arn:aws:elasticloadbalancing:us-east-1:1:targetgroup/shop-web/y";
    let m = run(
        &[
            res(
                lb,
                ResourceKind::LoadBalancer,
                &[],
                serde_json::json!({ "name": "shop-prod" }),
            ),
            res(
                tg,
                ResourceKind::TargetGroup,
                &[],
                serde_json::json!({ "name": "shop-web" }),
            ),
            inst("i-1", &[]),
        ],
        &[rel(I1, Edge::TargetOf, tg), rel(tg, Edge::AttachedTo, lb)],
    );
    assert_eq!(
        wl(&m, I1),
        ("shop".into(), "prod".into(), GroupingSignal::LoadBalancer)
    );
    assert_eq!(wl(&m, tg).2, GroupingSignal::Attached);
    assert_eq!(
        wl(&m, lb),
        ("shop".into(), "prod".into(), GroupingSignal::Attached)
    );
}

#[test]
fn volumes_elastic_ips_and_single_use_security_groups_follow_their_instance() {
    let sg = "arn:aws:ec2:us-east-1:123456789012:security-group/sg-1";
    let shared = "arn:aws:ec2:us-east-1:123456789012:security-group/sg-shared";
    let eip = "arn:aws:ec2:us-east-1:123456789012:elastic-ip/eipalloc-1";
    let m = run(
        &[
            inst("i-1", &[("app", "api"), ("env", "prod")]),
            inst("i-2", &[("app", "batch"), ("env", "prod")]),
            res(
                V1,
                ResourceKind::Volume,
                &[],
                serde_json::json!({ "size_gib": 8 }),
            ),
            res(sg, ResourceKind::SecurityGroup, &[], serde_json::json!({})),
            res(
                shared,
                ResourceKind::SecurityGroup,
                &[],
                serde_json::json!({}),
            ),
            res(eip, ResourceKind::PublicIp, &[], serde_json::json!({})),
        ],
        &[
            rel(V1, Edge::AttachedTo, I1),
            rel(eip, Edge::AttachedTo, I1),
            rel(I1, Edge::HasSecurityGroup, sg),
            rel(I1, Edge::HasSecurityGroup, shared),
            rel(I2, Edge::HasSecurityGroup, shared),
        ],
    );
    for arn in [V1, eip, sg] {
        assert_eq!(
            wl(&m, arn),
            ("api".into(), "prod".into(), GroupingSignal::Attached),
            "{arn}"
        );
    }
    assert!(
        m.iter().all(|x| x.arn != shared),
        "a group used by two workloads is not placed"
    );
}

#[test]
fn environment_from_tag_then_name_token_then_account_then_default() {
    let account = res(
        "arn:aws:organizations::1:account/o-x/123456789012",
        ResourceKind::Account,
        &[],
        serde_json::json!({ "name": "payments-prod", "parent_name": "Prod" }),
    );
    let m = run(
        &[
            account,
            inst(
                "i-1",
                &[("app", "ledger-dev"), ("Environment", "Production")],
            ),
            inst("i-2", &[("app", "ledger")]),
            inst("i-3", &[("app", "staging-ledger")]),
        ],
        &[],
    );
    assert_eq!(
        (wl(&m, I1).0, wl(&m, I1).1),
        ("ledger-dev".into(), "prod".into()),
        "tag wins, name untouched"
    );
    assert_eq!(
        (wl(&m, I2).0, wl(&m, I2).1),
        ("ledger".into(), "prod".into()),
        "account name token"
    );
    assert_eq!(
        (wl(&m, I3).0, wl(&m, I3).1),
        ("ledger".into(), "staging".into()),
        "name prefix beats account"
    );
}

#[test]
fn name_token_applies_when_nothing_else_names_the_environment() {
    let m = run(
        &[
            inst("i-1", &[("app", "ledger-dev")]),
            inst("i-2", &[("app", "qa-ledger")]),
            inst("i-3", &[("app", "ledger")]),
        ],
        &[],
    );
    assert_eq!((wl(&m, I1).0, wl(&m, I1).1), ("ledger".into(), "dev".into()));
    assert_eq!((wl(&m, I2).0, wl(&m, I2).1), ("ledger".into(), "qa".into()));
    assert_eq!(
        (wl(&m, I3).0, wl(&m, I3).1),
        ("ledger".into(), "default".into())
    );
}

#[test]
fn vpc_then_unassigned_are_the_last_resorts() {
    let vpc = "arn:aws:ec2:us-east-1:123456789012:vpc/vpc-1";
    let bare = res(
        I2,
        ResourceKind::Instance,
        &[],
        serde_json::json!({ "instance_type": "t3.micro" }),
    );
    let m = run(
        &[
            res(
                vpc,
                ResourceKind::Network,
                &[("Name", "Core Network")],
                serde_json::json!({ "network_id": "vpc-1" }),
            ),
            inst("i-1", &[]),
            bare,
        ],
        &[],
    );
    assert_eq!(
        (wl(&m, I1).0, wl(&m, I1).2),
        ("core".into(), GroupingSignal::Network)
    );
    assert_eq!(
        (wl(&m, I2).0, wl(&m, I2).2),
        ("core".into(), GroupingSignal::AccountOnly)
    );
}

#[test]
fn managed_services_group_by_stack_tag_or_name_prefix() {
    let stack = "arn:aws:cloudformation:us-east-1:1:stack/inbox-prod/x";
    let tagged =
        "arn:aws:dynamodb:us-east-1:123456789012:table/inbox-prod-rMailTable";
    let prefixed = "arn:aws:lambda:us-east-1:123456789012:function/inbox-prod-rWebhookFunction-Goztl7VNr7oo";
    let by_tag = "arn:aws:lambda:us-east-1:123456789012:function/cert-renewal";
    let lone = "arn:aws:lambda:us-east-1:123456789012:function/ami-cleanup";
    let m = run(
        &[
            res(
                stack,
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": "inbox-prod", "root_id": null }),
            ),
            res(
                tagged,
                ResourceKind::ManagedService,
                &[(STACK_ID_TAG, stack)],
                serde_json::json!({ "service": "key_value", "product": "dynamodb", "name": "inbox-prod-rMailTable" }),
            ),
            res(
                prefixed,
                ResourceKind::ManagedService,
                &[],
                serde_json::json!({ "service": "functions", "product": "lambda", "name": "inbox-prod-rWebhookFunction-Goztl7VNr7oo" }),
            ),
            res(
                by_tag,
                ResourceKind::ManagedService,
                &[("Service", "Vouch")],
                serde_json::json!({ "service": "functions", "product": "lambda", "name": "cert-renewal" }),
            ),
            res(
                lone,
                ResourceKind::ManagedService,
                &[],
                serde_json::json!({ "service": "functions", "product": "lambda", "name": "ami-cleanup" }),
            ),
        ],
        &[],
    );
    assert_eq!(
        wl(&m, tagged),
        ("inbox".into(), "prod".into(), GroupingSignal::DeploymentGroup)
    );
    assert_eq!(
        wl(&m, prefixed),
        ("inbox".into(), "prod".into(), GroupingSignal::NamePrefix)
    );
    assert_eq!(
        (wl(&m, by_tag).0, wl(&m, by_tag).2),
        ("vouch".into(), GroupingSignal::Tag)
    );
    assert_eq!(
        (wl(&m, lone).0, wl(&m, lone).2),
        ("ami-cleanup".into(), GroupingSignal::Unassigned)
    );
}

#[test]
fn manual_override_by_arn_wins() {
    let overrides =
        BTreeMap::from([(Arn::from(I1), PairKey::new("legacy", "prod"))]);
    let m = run_with(
        &[inst("i-1", &[(OVERRIDE_TAG, "x")])],
        &[],
        &overrides,
        &BTreeMap::new(),
    );
    assert_eq!(
        wl(&m, I1),
        ("legacy".into(), "prod".into(), GroupingSignal::Override)
    );
}

#[test]
fn aliases_merge_workloads_after_placement() {
    let asg = "arn:aws:autoscaling:us-east-1:1:autoScalingGroup:x:autoScalingGroupName/vouch-us-prod";
    let aliases = BTreeMap::from([(
        WorkloadName::from("vouch-us"),
        WorkloadName::from("vouch"),
    )]);
    let m = run_with(
        &[
            res(
                asg,
                ResourceKind::ScaleGroup,
                &[],
                serde_json::json!({ "name": "vouch-us-prod" }),
            ),
            inst("i-1", &[]),
            inst("i-2", &[("Service", "vouch"), ("env", "prod")]),
        ],
        &[rel(I1, Edge::MemberOfScaleGroup, asg)],
        &BTreeMap::new(),
        &aliases,
    );
    assert_eq!((wl(&m, I1).0, wl(&m, I1).1), ("vouch".into(), "prod".into()));
    assert_eq!((wl(&m, I2).0, wl(&m, I2).1), ("vouch".into(), "prod".into()));
    assert_eq!(
        wl(&m, asg).0,
        "vouch",
        "inherited placement sees the alias too"
    );
}

#[test]
fn managed_services_without_a_stack_share_a_root_across_environments() {
    let f = |name: &str| {
        res(
            &format!("arn:aws:lambda:us-east-1:123456789012:function/{name}"),
            ResourceKind::ManagedService,
            &[],
            serde_json::json!({ "service": "functions", "product": "lambda", "name": name }),
        )
    };
    let t = |name: &str| {
        res(
            &format!("arn:aws:dynamodb:us-east-1:123456789012:table/{name}"),
            ResourceKind::ManagedService,
            &[],
            serde_json::json!({ "service": "key_value", "product": "dynamodb", "name": name }),
        )
    };
    // The account is named for prod; resources whose own names say dev
    // must still land in dev.
    let account = res(
        "arn:aws:organizations::1:account/o-x/123456789012",
        ResourceKind::Account,
        &[],
        serde_json::json!({ "name": "fairfieldct-ai-prod", "parent_name": "Prod" }),
    );
    let m = run(
        &[
            account,
            t("fairfieldct-ai-dev"),
            t("fairfieldct-ai-prod"),
            f("fairfieldct-ai-dev-api"),
            f("fairfieldct-ai-prod-api"),
            f("fairfieldct-ai-prod-discord-reminders"),
            t("aws-messaging-webhook-dev-rMailTable-KA7II0CGC6FJ"),
            t("aws-messaging-webhook-dev-rMailTable-U2CPZXC1LNZV"),
        ],
        &[],
    );
    let pairs: std::collections::BTreeMap<(String, String), usize> =
        m.iter().fold(Default::default(), |mut acc, x| {
            *acc.entry((
                x.pair.workload.to_string(),
                x.pair.environment.to_string(),
            ))
            .or_default() += 1;
            acc
        });
    assert_eq!(
        pairs,
        [
            (("fairfieldct-ai".to_string(), "dev".to_string()), 2),
            (("fairfieldct-ai".to_string(), "prod".to_string()), 3),
            (
                (
                    "aws-messaging-webhook-rmailtable".to_string(),
                    "dev".to_string()
                ),
                2
            ),
        ]
        .into_iter()
        .collect(),
        "{m:#?}"
    );
}

#[test]
fn list_stack_resources_edge_places_untagged_resources() {
    let stack = "arn:aws:cloudformation:us-east-1:1:stack/vouch-prod/x";
    let func = "arn:aws:lambda:us-east-1:123456789012:function/cert-renewal";
    let sg = "arn:aws:ec2:us-east-1:123456789012:security-group/sg-9";
    let m = run(
        &[
            res(
                stack,
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": "vouch-prod", "root_id": null }),
            ),
            res(
                func,
                ResourceKind::ManagedService,
                &[],
                serde_json::json!({ "service": "functions", "product": "lambda", "name": "cert-renewal" }),
            ),
            res(sg, ResourceKind::SecurityGroup, &[], serde_json::json!({})),
        ],
        &[
            rel(func, Edge::InDeploymentGroup, stack),
            rel(sg, Edge::InDeploymentGroup, stack),
        ],
    );
    assert_eq!(
        wl(&m, func),
        ("vouch".into(), "prod".into(), GroupingSignal::DeploymentGroup)
    );
    assert_eq!(
        wl(&m, sg),
        ("vouch".into(), "prod".into(), GroupingSignal::DeploymentGroup)
    );
}

#[test]
fn a_known_workload_inside_a_name_is_the_last_resort_before_unassigned() {
    let func = "arn:aws:lambda:us-east-1:123456789012:function/cert-renewal-us-vouch-sh";
    let m = run(
        &[
            inst("i-1", &[("Service", "vouch"), ("env", "prod")]),
            res(
                func,
                ResourceKind::ManagedService,
                &[],
                serde_json::json!({ "service": "functions", "product": "lambda", "name": "cert-renewal-us-vouch-sh" }),
            ),
        ],
        &[],
    );
    assert_eq!(
        wl(&m, func),
        ("vouch".into(), "prod".into(), GroupingSignal::NameContains)
    );
}

#[test]
fn platform_stacks_collapse_into_one_workload() {
    let stack = "arn:aws:cloudformation:us-east-1:1:stack/StackSet-AWS-QuickSetup-SSM-LA-tyi62-39cf/x";
    let func = "arn:aws:lambda:us-east-1:123456789012:function/aws-quicksetup-lifecycle-LA-tyi62";
    let m = run(
        &[
            res(
                stack,
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": "StackSet-AWS-QuickSetup-SSM-LA-tyi62-39cf", "root_id": null }),
            ),
            res(
                func,
                ResourceKind::ManagedService,
                &[],
                serde_json::json!({ "service": "functions", "product": "lambda", "name": "aws-quicksetup-lifecycle-LA-tyi62" }),
            ),
        ],
        &[rel(func, Edge::InDeploymentGroup, stack)],
    );
    assert_eq!(
        (wl(&m, func).0, wl(&m, func).2),
        ("cloud-platform".into(), GroupingSignal::DeploymentGroup)
    );
}

#[test]
fn terraform_managed_siblings_share_a_root() {
    let f = |name: &str| {
        res(
            &format!("arn:aws:lambda:us-east-1:123456789012:function/{name}"),
            ResourceKind::ManagedService,
            &[],
            serde_json::json!({ "service": "functions", "product": "lambda", "name": name }),
        )
    };
    let m = run(
        &[
            f("serverless-create-root-ca-prod"),
            f("serverless-create-issuing-ca-prod"),
            f("serverless-expiry-prod"),
            f("serverless-tls-cert-prod"),
        ],
        &[],
    );
    assert!(m.iter().all(|x| x.pair.is("serverless", "prod")), "{m:#?}");
    assert!(m.iter().all(|x| x.signal == GroupingSignal::NamePrefix));
}

#[test]
fn layer_stacks_of_one_workload_group_together() {
    let app = "arn:aws:cloudformation:us-east-1:1:stack/playdate-app-prod/a";
    let vpc = "arn:aws:cloudformation:us-east-1:1:stack/playdate-vpc-prod/v";
    let sg = "arn:aws:ec2:us-east-1:123456789012:security-group/sg-1";
    let m = run(
        &[
            res(
                app,
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": "playdate-app-prod", "root_id": null }),
            ),
            res(
                vpc,
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": "playdate-vpc-prod", "root_id": null }),
            ),
            inst("i-1", &[]),
            res(sg, ResourceKind::SecurityGroup, &[], serde_json::json!({})),
        ],
        &[
            rel(I1, Edge::InDeploymentGroup, app),
            rel(sg, Edge::InDeploymentGroup, vpc),
        ],
    );
    assert_eq!(
        (wl(&m, I1).0, wl(&m, I1).1),
        ("playdate".into(), "prod".into())
    );
    assert_eq!(
        (wl(&m, sg).0, wl(&m, sg).1),
        ("playdate".into(), "prod".into())
    );
}

#[test]
fn an_account_with_one_workload_claims_its_orphans() {
    let func = "arn:aws:lambda:us-east-1:123456789012:function/ami-cleanup";
    let m = run(
        &[
            inst("i-1", &[("Service", "vouch"), ("env", "prod")]),
            res(
                func,
                ResourceKind::ManagedService,
                &[],
                serde_json::json!({ "service": "functions", "product": "lambda", "name": "ami-cleanup" }),
            ),
        ],
        &[],
    );
    assert_eq!(
        wl(&m, func),
        ("vouch".into(), "prod".into(), GroupingSignal::AccountOnly)
    );
}

#[test]
fn an_account_with_two_workloads_leaves_orphans_alone() {
    let func = "arn:aws:lambda:us-east-1:123456789012:function/ami-cleanup";
    let m = run(
        &[
            inst("i-1", &[("Service", "vouch")]),
            inst("i-2", &[("Service", "shop")]),
            res(
                func,
                ResourceKind::ManagedService,
                &[],
                serde_json::json!({ "service": "functions", "product": "lambda", "name": "ami-cleanup" }),
            ),
        ],
        &[],
    );
    assert_eq!(wl(&m, func).2, GroupingSignal::Unassigned);
}

#[test]
fn unanimous_needs_at_least_one_and_full_agreement() {
    assert_eq!(unanimous(Vec::<u8>::new()), None);
    assert_eq!(unanimous([3]), Some(3));
    assert_eq!(unanimous([3, 3, 3]), Some(3));
    assert_eq!(unanimous([3, 4, 3]), None);
}

mod azure {
    use super::*;

    const RG: &str = "/subscriptions/s1/resourcegroups";

    fn az(id: &str, kind: ResourceKind, body: serde_json::Value) -> Resource {
        Resource {
            account_id: "s1".into(),
            region: "eastus".into(),
            ..res(id, kind, &[], body)
        }
    }

    fn group(name: &str, root: Option<&str>) -> Resource {
        az(
            &format!("{RG}/{}", name.to_lowercase()),
            ResourceKind::DeploymentGroup,
            serde_json::json!({ "name": name, "root_id": root }),
        )
    }

    fn vm(group: &str, name: &str) -> Resource {
        az(
            &format!(
                "{RG}/{group}/providers/microsoft.compute/virtualmachines/{name}"
            ),
            ResourceKind::Instance,
            serde_json::json!({ "instance_id": name }),
        )
    }

    fn member_of(r: &Resource, group: &str) -> Relationship {
        rel(r.arn.as_str(), Edge::InDeploymentGroup, &format!("{RG}/{group}"))
    }

    #[test]
    fn a_vm_takes_its_resource_groups_name_and_environment() {
        let web = vm("shop-prod", "web-1");
        let resources = [group("shop-prod", None), web.clone()];
        let m = run(&resources, &[member_of(&web, "shop-prod")]);
        assert_eq!(
            wl(&m, web.arn.as_str()),
            ("shop".into(), "prod".into(), GroupingSignal::DeploymentGroup)
        );
    }

    #[test]
    fn aks_node_groups_join_their_cluster_and_tooling_groups_the_platform() {
        let aks = format!(
            "{RG}/shop/providers/microsoft.containerservice/managedclusters/orders-aks"
        );
        let node = vm("mc_shop_orders-aks_eastus", "aks-nodepool1-0");
        let watcher = az(
            &format!(
                "{RG}/networkwatcherrg/providers/microsoft.network/networkwatchers/nw"
            ),
            ResourceKind::Instance,
            serde_json::json!({}),
        );
        let resources = [
            az(
                &aks,
                ResourceKind::ManagedService,
                serde_json::json!({
                    "service": "kubernetes", "product": "aks", "name": "orders-aks",
                }),
            ),
            group("MC_shop_orders-aks_eastus", Some(&aks)),
            group("NetworkWatcherRG", None),
            node.clone(),
            watcher.clone(),
        ];
        let m = run(
            &resources,
            &[
                member_of(&node, "mc_shop_orders-aks_eastus"),
                member_of(&watcher, "networkwatcherrg"),
            ],
        );
        assert_eq!(wl(&m, node.arn.as_str()).0, "orders-aks");
        assert_eq!(wl(&m, watcher.arn.as_str()).0, "cloud-platform");
    }

    #[test]
    fn without_a_group_a_vm_takes_its_networks_own_name() {
        let net = format!(
            "{RG}/net/providers/microsoft.network/virtualnetworks/billing-vnet"
        );
        let lone = az(
            &format!("{RG}/x/providers/microsoft.compute/virtualmachines/lone"),
            ResourceKind::Instance,
            serde_json::json!({ "network_id": net }),
        );
        let resources = [
            az(
                &net,
                ResourceKind::Network,
                serde_json::json!({ "network_id": net, "name": "billing-vnet" }),
            ),
            lone.clone(),
        ];
        let m = run(&resources, &[]);
        let (workload, _, signal) = wl(&m, lone.arn.as_str());
        assert_eq!(
            (workload.as_str(), signal),
            ("billing", GroupingSignal::Network)
        );
    }
}

#[test]
fn azure_naming_prefixes_and_suffixes_are_not_the_workload() {
    for name in ["rg-shop-prod", "shop-prod-rg", "shop-rg-prod"] {
        let g = Resource {
            account_id: "s1".into(),
            ..res(
                &format!("/subscriptions/s1/resourcegroups/{name}"),
                ResourceKind::DeploymentGroup,
                &[],
                serde_json::json!({ "name": name }),
            )
        };
        let vm = Resource {
            account_id: "s1".into(),
            ..res(
                &format!(
                    "/subscriptions/s1/resourcegroups/{name}/providers/microsoft.compute/virtualmachines/web"
                ),
                ResourceKind::Instance,
                &[],
                serde_json::json!({}),
            )
        };
        let m = run(
            &[g.clone(), vm.clone()],
            &[rel(vm.arn.as_str(), Edge::InDeploymentGroup, g.arn.as_str())],
        );
        let (w, e, _) = wl(&m, vm.arn.as_str());
        assert_eq!((w.as_str(), e.as_str()), ("shop", "prod"), "{name}");
    }
}

mod gcp {
    use super::*;

    const C: &str = "//compute.googleapis.com/projects";

    fn gc(
        id: &str,
        project: &str,
        region: &str,
        kind: ResourceKind,
        body: serde_json::Value,
    ) -> Resource {
        Resource {
            account_id: project.into(),
            region: region.into(),
            ..res(id, kind, &[], body)
        }
    }

    /// A shared VPC is global and lives in its host project, so neither
    /// its account nor its region is the instance's.
    #[test]
    fn a_vm_takes_the_name_of_a_shared_vpc_in_another_project() {
        let net = format!("{C}/net-host/global/networks/billing-vpc");
        let vm = gc(
            &format!("{C}/billing-prod/zones/us-central1-a/instances/vm-1"),
            "billing-prod",
            "us-central1",
            ResourceKind::Instance,
            serde_json::json!({ "network_id": net }),
        );
        let resources = [
            gc(
                &net,
                "net-host",
                "global",
                ResourceKind::Network,
                serde_json::json!({ "network_id": net, "name": "billing-vpc" }),
            ),
            vm.clone(),
        ];
        let m = run(&resources, &[]);
        let (workload, _, signal) = wl(&m, vm.arn.as_str());
        assert_eq!(
            (workload.as_str(), signal),
            ("billing", GroupingSignal::Network)
        );
    }

    #[test]
    fn gke_nodes_and_their_group_join_the_cluster() {
        let cluster = "//container.googleapis.com/projects/shop/locations/us-central1/clusters/orders";
        let nodes = format!("{cluster}#nodes");
        let mig = gc(
            &format!(
                "{C}/shop/zones/us-central1-a/instanceGroupManagers/gke-orders-pool-1a2b3c4d-grp"
            ),
            "shop",
            "us-central1",
            ResourceKind::ScaleGroup,
            serde_json::json!({ "name": "gke-orders-pool-1a2b3c4d-grp" }),
        );
        let node = gc(
            &format!(
                "{C}/shop/zones/us-central1-a/instances/gke-orders-pool-1a2b3c4d-x1y2"
            ),
            "shop",
            "us-central1",
            ResourceKind::Instance,
            serde_json::json!({}),
        );
        let resources = [
            gc(
                cluster,
                "shop",
                "us-central1",
                ResourceKind::ManagedService,
                serde_json::json!({
                    "service": "kubernetes", "product": "gke", "name": "orders",
                }),
            ),
            gc(
                &nodes,
                "shop",
                "us-central1",
                ResourceKind::DeploymentGroup,
                serde_json::json!({ "name": "orders", "root_id": cluster }),
            ),
            mig.clone(),
            node.clone(),
        ];
        let m = run(
            &resources,
            &[
                rel(node.arn.as_str(), Edge::InDeploymentGroup, &nodes),
                rel(mig.arn.as_str(), Edge::InDeploymentGroup, &nodes),
                rel(cluster, Edge::InDeploymentGroup, &nodes),
                rel(
                    node.arn.as_str(),
                    Edge::MemberOfScaleGroup,
                    mig.arn.as_str(),
                ),
            ],
        );
        assert_eq!(
            wl(&m, node.arn.as_str()),
            (
                "orders".into(),
                "default".into(),
                GroupingSignal::DeploymentGroup
            )
        );
        assert_eq!(wl(&m, cluster).0, "orders");
    }
}
