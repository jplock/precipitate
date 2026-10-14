//! Workload grouping from RFD 0001, section "Workloads". Pure over the
//! resources and relationships present at one scan, so every rule is
//! unit-testable and every placement records the signal that made it.
//!
//! Instances are placed first, by the first signal that matches. Managed
//! services are placed by stack, tags, or a name prefix. Everything else
//! that belongs to an instance inherits its placement.

mod names;

use precip_types::scan::{Edge, Relationship, Resource, ResourceKind};
use precip_types::{GroupingConfig, GroupingSignal, Override, WorkloadMember};
use std::collections::{BTreeMap, HashMap, HashSet};

pub use names::slug;

pub const OVERRIDE_TAG: &str = "precip:workload";
const STACK_ID_TAG: &str = "aws:cloudformation:stack-id";

/// Everything grouping reads, fetched by the store for one scan.
pub struct Input<'a> {
    pub resources: &'a [Resource],
    pub relationships: &'a [Relationship],
    /// Manual placement by ARN. Wins over every signal.
    pub overrides: &'a BTreeMap<String, Override>,
    /// Workload names that are really another workload, applied last.
    pub aliases: &'a BTreeMap<String, String>,
}

struct Index<'a> {
    by_arn: HashMap<&'a str, &'a Resource>,
    platform_workload: String,
    /// Stack id to the root stack's name.
    stack_root_name: HashMap<&'a str, String>,
    /// From ARN to (edge, to ARN).
    out: HashMap<&'a str, Vec<(Edge, &'a str)>>,
    /// To ARN to (edge, from ARN).
    inbound: HashMap<&'a str, Vec<(Edge, &'a str)>>,
    account: HashMap<&'a str, (Option<String>, Option<String>)>,
}

impl<'a> Index<'a> {
    fn build(input: &Input<'a>, cfg: &GroupingConfig) -> Self {
        let by_arn: HashMap<&str, &Resource> =
            input.resources.iter().map(|r| (r.arn.as_str(), r)).collect();

        // AWS tooling stacks all collapse into one platform workload.
        let is_platform = |name: &str| {
            cfg.platform_stack_prefixes
                .iter()
                .any(|p| name.to_lowercase().starts_with(&p.to_lowercase()))
        };
        let mut stack_root_name = HashMap::new();
        for r in input
            .resources
            .iter()
            .filter(|r| r.kind == ResourceKind::CloudFormationStack)
        {
            let root = r
                .body
                .get("root_id")
                .and_then(|v| v.as_str())
                .and_then(|id| by_arn.get(id))
                .unwrap_or(&r);
            if let Some(name) = root.body.get("name").and_then(|v| v.as_str()) {
                let name = if is_platform(name) {
                    cfg.platform_workload.clone()
                } else {
                    name.to_string()
                };
                stack_root_name.insert(r.arn.as_str(), name);
            }
        }

        let mut out: HashMap<&str, Vec<(Edge, &str)>> = HashMap::new();
        let mut inbound: HashMap<&str, Vec<(Edge, &str)>> = HashMap::new();
        for rel in input.relationships {
            out.entry(rel.from_arn.as_str())
                .or_default()
                .push((rel.edge, rel.to_arn.as_str()));
            inbound
                .entry(rel.to_arn.as_str())
                .or_default()
                .push((rel.edge, rel.from_arn.as_str()));
        }

        let account = input
            .resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Account)
            .map(|r| {
                let name = r
                    .body
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let ou = r
                    .body
                    .get("parent_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                (r.account_id.as_str(), (name, ou))
            })
            .collect();

        Self {
            by_arn,
            platform_workload: cfg.platform_workload.clone(),
            stack_root_name,
            out,
            inbound,
            account,
        }
    }

    fn edges_from(
        &self,
        arn: &str,
        edge: Edge,
    ) -> impl Iterator<Item = &'a str> + '_ {
        self.out
            .get(arn)
            .into_iter()
            .flatten()
            .filter(move |(e, _)| *e == edge)
            .map(|(_, to)| *to)
    }

    fn edges_to(
        &self,
        arn: &str,
        edge: Edge,
    ) -> impl Iterator<Item = &'a str> + '_ {
        self.inbound
            .get(arn)
            .into_iter()
            .flatten()
            .filter(move |(e, _)| *e == edge)
            .map(|(_, from)| *from)
    }

    fn root_stack_name(&self, stack_id: &str) -> Option<String> {
        self.stack_root_name.get(stack_id).cloned()
    }

    /// The root stack a resource belongs to: by an `in_stack` edge from
    /// `ListStackResources`, else by the stack-id tag CloudFormation sets.
    fn stack_of(&self, r: &Resource) -> Option<String> {
        self.edges_from(&r.arn, Edge::InStack)
            .find_map(|stack| self.root_stack_name(stack))
            .or_else(|| {
                r.tags.get(STACK_ID_TAG).and_then(|id| self.root_stack_name(id))
            })
    }
}

fn tag<'a>(r: &'a Resource, keys: &[String]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| r.tags.get(k).map(String::as_str))
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn body_str<'a>(r: &'a Resource, key: &str) -> Option<&'a str> {
    r.body.get(key).and_then(|v| v.as_str())
}

/// A raw placement before environment resolution.
struct Placed {
    arn: String,
    signal: GroupingSignal,
    slug: String,
    override_env: Option<String>,
    env_tag: Option<String>,
    /// Environment token found in the resource's own name when the
    /// workload name came from somewhere else (a stack, a shared root).
    name_env: Option<String>,
    /// The only environment of the workload this resource joined by name,
    /// when that workload has exactly one.
    joined_env: Option<String>,
    account_id: String,
}

fn place_instance(
    r: &Resource,
    index: &Index<'_>,
    input: &Input<'_>,
    cfg: &GroupingConfig,
) -> Placed {
    if let Some(o) = input.overrides.get(&r.arn) {
        return Placed {
            arn: r.arn.clone(),
            signal: GroupingSignal::Override,
            slug: slug(&o.workload),
            override_env: Some(o.environment.clone()),
            env_tag: None,
            name_env: None,
            joined_env: None,
            account_id: r.account_id.clone(),
        };
    }

    let asg = index
        .edges_from(&r.arn, Edge::MemberOfAsg)
        .find_map(|arn| index.by_arn.get(arn))
        .map(|g| match index.stack_of(g) {
            Some(root) => (root, GroupingSignal::CloudFormationStack),
            None => (
                body_str(g, "name").unwrap_or(&g.arn).to_string(),
                GroupingSignal::AutoScalingGroup,
            ),
        });

    let target_group = index
        .edges_from(&r.arn, Edge::TargetOf)
        .filter_map(|tg_arn| {
            let tg = index.by_arn.get(tg_arn)?;
            let lb_name = index
                .edges_from(tg_arn, Edge::AttachedTo)
                .filter_map(|lb| index.by_arn.get(lb))
                .filter(|lb| lb.kind == ResourceKind::LoadBalancer)
                .filter_map(|lb| body_str(lb, "name"))
                .min();
            lb_name.or_else(|| body_str(tg, "name")).map(str::to_string)
        })
        .min();

    let vpc = body_str(r, "vpc_id").map(|vpc_id| {
        input
            .resources
            .iter()
            .find(|v| {
                v.kind == ResourceKind::Vpc
                    && v.account_id == r.account_id
                    && v.region == r.region
                    && body_str(v, "vpc_id") == Some(vpc_id)
            })
            .and_then(|v| v.tags.get("Name").map(String::as_str))
            .unwrap_or(vpc_id)
            .to_string()
    });

    let (raw, signal) = if let Some(name) =
        r.tags.get(OVERRIDE_TAG).map(|s| s.trim()).filter(|s| !s.is_empty())
    {
        (name.to_string(), GroupingSignal::OverrideTag)
    } else if let Some(root) = index.stack_of(r) {
        (root, GroupingSignal::CloudFormationStack)
    } else if let Some((name, signal)) = asg {
        (name, signal)
    } else if let Some(name) = target_group {
        (name, GroupingSignal::LoadBalancer)
    } else if let Some(name) = tag(r, &cfg.workload_tags) {
        (name.to_string(), GroupingSignal::Tag)
    } else if let Some(name) = vpc {
        (name, GroupingSignal::Vpc)
    } else {
        ("unassigned".to_string(), GroupingSignal::Unassigned)
    };

    Placed {
        arn: r.arn.clone(),
        signal,
        slug: slug(&raw),
        override_env: None,
        env_tag: tag(r, &cfg.environment_tags).map(str::to_string),
        name_env: None,
        joined_env: None,
        account_id: r.account_id.clone(),
    }
}

/// A managed service's own name, slugified with any generated suffix
/// removed.
fn managed_own_name(r: &Resource) -> String {
    names::strip_generated_suffix(&slug(
        body_str(r, "name").unwrap_or("unassigned"),
    ))
    .to_string()
}

/// Place a managed service: its stack, a workload tag, the longest strong
/// name (a stack or an instance workload) its name starts with, the
/// shortest root it shares with other managed resources, else itself.
fn place_managed(
    r: &Resource,
    index: &Index<'_>,
    cfg: &GroupingConfig,
    strong_names: &[String],
    sibling_roots: &[String],
    workload_envs: &HashMap<String, std::collections::BTreeSet<String>>,
) -> Placed {
    let own = managed_own_name(r);
    let (own_root, name_env) =
        match names::split_environment(&own, &cfg.environment_tokens) {
            Some((root, env)) => (root, Some(env)),
            None => (own.clone(), None),
        };
    let (slug, signal) = if let Some(root) = index.stack_of(r) {
        (slug(&root), GroupingSignal::CloudFormationStack)
    } else if let Some(name) = tag(r, &cfg.workload_tags) {
        (slug(name), GroupingSignal::Tag)
    } else if let Some(prefix) = names::longest_prefix(&own, strong_names) {
        (prefix.to_string(), GroupingSignal::NamePrefix)
    } else if let Some(root) = names::shortest_prefix(&own_root, sibling_roots)
    {
        (root.to_string(), GroupingSignal::NamePrefix)
    } else if let Some(name) = names::contains_segment(&own, strong_names) {
        (name.to_string(), GroupingSignal::NameContains)
    } else if let Some(root) =
        names::shared_root(&own_root, sibling_roots, &cfg.environment_tokens)
    {
        (root, GroupingSignal::NamePrefix)
    } else {
        (own, GroupingSignal::Unassigned)
    };
    let joined_env = match signal {
        GroupingSignal::NamePrefix | GroupingSignal::NameContains => {
            workload_envs.get(&slug).and_then(|envs| {
                (envs.len() == 1).then(|| envs.iter().next().cloned()).flatten()
            })
        }
        GroupingSignal::Override
        | GroupingSignal::OverrideTag
        | GroupingSignal::CloudFormationStack
        | GroupingSignal::AutoScalingGroup
        | GroupingSignal::LoadBalancer
        | GroupingSignal::Tag
        | GroupingSignal::AccountOnly
        | GroupingSignal::Vpc
        | GroupingSignal::Unassigned
        | GroupingSignal::Attached => None,
    };
    Placed {
        arn: r.arn.clone(),
        signal,
        slug,
        override_env: None,
        env_tag: tag(r, &cfg.environment_tags).map(str::to_string),
        name_env,
        joined_env,
        account_id: r.account_id.clone(),
    }
}

/// Resolve the environment and final workload name for a placement.
/// Order: override, environment tag, a token in the workload name, a
/// token in the resource's own name, the single environment of a workload
/// joined by name, a token in the account or OU name, then the default. The name token is
/// stripped only when it agrees with the environment chosen.
fn resolve(
    p: Placed,
    index: &Index<'_>,
    cfg: &GroupingConfig,
    aliases: &BTreeMap<String, String>,
) -> WorkloadMember {
    let split = names::split_environment(&p.slug, &cfg.environment_tokens);
    let from_tag = p.env_tag.as_deref().map(|t| {
        let t = slug(t);
        cfg.environment_tokens.get(&t).cloned().unwrap_or(t)
    });
    let from_account =
        index.account.get(p.account_id.as_str()).and_then(|(name, ou)| {
            names::environment_in_account(
                name.as_deref(),
                ou.as_deref(),
                &cfg.environment_tokens,
            )
        });

    let environment = p
        .override_env
        .clone()
        .or(from_tag)
        .or_else(|| split.as_ref().map(|(_, env)| env.clone()))
        .or(p.name_env.clone())
        .or(p.joined_env.clone())
        .or(from_account)
        .unwrap_or_else(|| cfg.default_environment.clone());

    let workload = match split {
        Some((stripped, env)) if env == environment => stripped,
        _ => p.slug,
    };
    let workload = names::strip_layer(&workload, &cfg.layer_tokens);
    let workload = aliases.get(&workload).cloned().unwrap_or(workload);

    WorkloadMember { arn: p.arn, workload, environment, signal: p.signal }
}

/// Assign every groupable resource to a workload and environment.
pub fn group(input: &Input<'_>, cfg: &GroupingConfig) -> Vec<WorkloadMember> {
    let index = Index::build(input, cfg);
    let mut members: Vec<WorkloadMember> = Vec::new();
    let mut placed: HashSet<String> = HashSet::new();

    // Instances first.
    for r in
        input.resources.iter().filter(|r| r.kind == ResourceKind::Ec2Instance)
    {
        let m = resolve(
            place_instance(r, &index, input, cfg),
            &index,
            cfg,
            input.aliases,
        );
        placed.insert(m.arn.clone());
        members.push(m);
    }

    // Strong names a managed service may be prefixed with: stack roots
    // and placed instance workloads.
    let mut strong_names: Vec<String> = index
        .stack_root_name
        .values()
        .map(|n| slug(n))
        .chain(
            members
                .iter()
                .filter(|m| m.signal != GroupingSignal::Unassigned)
                .map(|m| m.workload.clone()),
        )
        .collect();
    strong_names.sort();
    strong_names.dedup();
    strong_names.retain(|n| *n != index.platform_workload);

    // Roots shared among managed services themselves: each resource's own
    // name with its environment token removed. A name only counts as a
    // root for resources other than the one it came from.
    let managed: Vec<&Resource> = input
        .resources
        .iter()
        .filter(|r| r.kind == ResourceKind::ManagedService)
        .collect();
    // Resources a stack already accounts for do not offer their names as
    // roots for the rest.
    let own_roots: Vec<String> = managed
        .iter()
        .map(|r| {
            if index.stack_of(r).is_some() {
                return String::new();
            }
            let own = managed_own_name(r);
            names::split_environment(&own, &cfg.environment_tokens)
                .map(|(stripped, _)| stripped)
                .unwrap_or(own)
        })
        .collect();

    let mut workload_envs: HashMap<String, std::collections::BTreeSet<String>> =
        HashMap::new();
    for m in &members {
        workload_envs
            .entry(m.workload.clone())
            .or_default()
            .insert(m.environment.clone());
    }

    for (i, r) in managed.iter().enumerate() {
        let siblings: Vec<String> = own_roots
            .iter()
            .enumerate()
            .filter(|(j, n)| *j != i && !n.is_empty())
            .map(|(_, n)| n.clone())
            .collect();
        let m = resolve(
            place_managed(
                r,
                &index,
                cfg,
                &strong_names,
                &siblings,
                &workload_envs,
            ),
            &index,
            cfg,
            input.aliases,
        );
        placed.insert(m.arn.clone());
        members.push(m);
    }

    // Inheritance. A resource follows its instances when they all agree.
    let by_arn: HashMap<&str, (&str, &str)> = members
        .iter()
        .map(|m| {
            (m.arn.as_str(), (m.workload.as_str(), m.environment.as_str()))
        })
        .collect();
    let mut inherited: Vec<WorkloadMember> = Vec::new();
    let mut inherit = |arn: &str, owners: Vec<(&str, &str)>| {
        if placed.contains(arn) || owners.is_empty() {
            return;
        }
        let first = owners[0];
        if owners.iter().all(|o| *o == first) {
            placed.insert(arn.to_string());
            inherited.push(WorkloadMember {
                arn: arn.to_string(),
                workload: first.0.to_string(),
                environment: first.1.to_string(),
                signal: GroupingSignal::Attached,
            });
        }
    };

    for r in input.resources {
        match r.kind {
            // Attached to an instance: volumes, Elastic IPs.
            ResourceKind::EbsVolume | ResourceKind::ElasticIp => {
                let owners: Vec<_> = index
                    .edges_from(&r.arn, Edge::AttachedTo)
                    .filter_map(|to| by_arn.get(to).copied())
                    .collect();
                inherit(&r.arn, owners);
            }
            // Instances point at these.
            ResourceKind::AutoScalingGroup => {
                let owners: Vec<_> = index
                    .edges_to(&r.arn, Edge::MemberOfAsg)
                    .filter_map(|from| by_arn.get(from).copied())
                    .collect();
                inherit(&r.arn, owners);
            }
            ResourceKind::TargetGroup => {
                let owners: Vec<_> = index
                    .edges_to(&r.arn, Edge::TargetOf)
                    .filter_map(|from| by_arn.get(from).copied())
                    .collect();
                inherit(&r.arn, owners);
            }
            ResourceKind::SecurityGroup => {
                let owners: Vec<_> = index
                    .edges_to(&r.arn, Edge::HasSecurityGroup)
                    .filter_map(|from| by_arn.get(from).copied())
                    .collect();
                inherit(&r.arn, owners);
            }
            // Placed by their own signals, or never placed by inheritance.
            ResourceKind::Account
            | ResourceKind::Ec2Instance
            | ResourceKind::InstanceType
            | ResourceKind::Ami
            | ResourceKind::Vpc
            | ResourceKind::Subnet
            | ResourceKind::SecurityGroupRule
            | ResourceKind::RouteTable
            | ResourceKind::InternetGateway
            | ResourceKind::NatGateway
            | ResourceKind::VpcPeeringConnection
            | ResourceKind::PrefixList
            | ResourceKind::LaunchTemplate
            | ResourceKind::LaunchConfiguration
            | ResourceKind::LoadBalancer
            | ResourceKind::CloudFormationStack
            | ResourceKind::ManagedService => {}
        }
    }
    members.extend(inherited);

    // Load balancers follow their target groups, which were just placed.
    let by_arn: HashMap<&str, (&str, &str)> = members
        .iter()
        .map(|m| {
            (m.arn.as_str(), (m.workload.as_str(), m.environment.as_str()))
        })
        .collect();
    let mut lbs = Vec::new();
    for r in
        input.resources.iter().filter(|r| r.kind == ResourceKind::LoadBalancer)
    {
        if placed.contains(&r.arn) {
            continue;
        }
        let owners: Vec<_> = index
            .edges_to(&r.arn, Edge::AttachedTo)
            .filter(|from| {
                index
                    .by_arn
                    .get(from)
                    .is_some_and(|tg| tg.kind == ResourceKind::TargetGroup)
            })
            .filter_map(|from| by_arn.get(from).copied())
            .collect();
        if let Some(first) = owners.first().copied() {
            if owners.iter().all(|o| *o == first) {
                lbs.push(WorkloadMember {
                    arn: r.arn.clone(),
                    workload: first.0.to_string(),
                    environment: first.1.to_string(),
                    signal: GroupingSignal::Attached,
                });
            }
        }
    }
    members.extend(lbs);

    // Anything groupable still unplaced that CloudFormation says belongs to
    // a stack: security groups shared by nothing collected, volumes with no
    // attachment, and the like.
    let mut by_stack = Vec::new();
    for r in input.resources {
        let groupable = matches!(
            r.kind,
            ResourceKind::EbsVolume
                | ResourceKind::ElasticIp
                | ResourceKind::AutoScalingGroup
                | ResourceKind::TargetGroup
                | ResourceKind::LoadBalancer
                | ResourceKind::SecurityGroup
        );
        if !groupable || placed.contains(&r.arn) {
            continue;
        }
        if let Some(root) = index.stack_of(r) {
            placed.insert(r.arn.clone());
            by_stack.push(resolve(
                Placed {
                    arn: r.arn.clone(),
                    signal: GroupingSignal::CloudFormationStack,
                    slug: slug(&root),
                    override_env: None,
                    env_tag: tag(r, &cfg.environment_tags).map(str::to_string),
                    name_env: None,
                    joined_env: None,
                    account_id: r.account_id.clone(),
                },
                &index,
                cfg,
                input.aliases,
            ));
        }
    }
    members.extend(by_stack);

    // Last resort: an account that hosts exactly one workload claims the
    // unassigned managed services in it. Platform resources never count
    // as that workload.
    let mut per_account: HashMap<
        &str,
        std::collections::BTreeSet<(String, String)>,
    > = HashMap::new();
    for m in &members {
        if m.signal == GroupingSignal::Unassigned
            || m.workload == index.platform_workload
        {
            continue;
        }
        let Some(r) = index.by_arn.get(m.arn.as_str()) else { continue };
        per_account
            .entry(r.account_id.as_str())
            .or_default()
            .insert((m.workload.clone(), m.environment.clone()));
    }
    for m in members.iter_mut() {
        if m.signal != GroupingSignal::Unassigned {
            continue;
        }
        let Some(r) = index.by_arn.get(m.arn.as_str()) else { continue };
        let Some(pairs) = per_account.get(r.account_id.as_str()) else {
            continue;
        };
        let workloads: std::collections::BTreeSet<&str> =
            pairs.iter().map(|(w, _)| w.as_str()).collect();
        if workloads.len() != 1 {
            continue;
        }
        let (workload, environment) =
            pairs.iter().next().cloned().unwrap_or_default();
        m.workload = workload;
        // Keep the resource's own environment when it had evidence for one;
        // otherwise take the account's single pair.
        if m.environment == cfg.default_environment {
            m.environment = environment;
        }
        m.signal = GroupingSignal::AccountOnly;
    }

    members.sort_by(|a, b| {
        (&a.workload, &a.environment, &a.arn).cmp(&(
            &b.workload,
            &b.environment,
            &b.arn,
        ))
    });
    members
}

#[cfg(test)]
mod tests;
