//! Workload grouping from RFD 0001, section "Workloads". Pure over the
//! resources and relationships present at one scan, so every rule is
//! unit-testable and every placement records the signal that made it.
//!
//! Instances are placed first, by the first signal that matches. Managed
//! services are placed by stack, tags, or a name prefix. Everything else
//! that belongs to an instance inherits its placement.

mod names;

use precip_types::scan::{
    Direction, Edge, Relationship, Resource, ResourceKind,
};
use precip_types::{
    AccountId, Arn, Body, EnvName, GroupingConfig, GroupingSignal, PairKey,
    WorkloadMember, WorkloadName, folded,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use names::{AccountNames, Slug};

pub(crate) const OVERRIDE_TAG: &str = "precip:workload";
const STACK_ID_TAG: &str = "aws:cloudformation:stack-id";

/// Everything grouping reads, fetched by the store for one scan.
pub struct Input<'a> {
    pub resources: &'a [Resource],
    pub relationships: &'a [Relationship],
    /// Manual placement by ARN. Wins over every signal.
    pub overrides: &'a BTreeMap<Arn, PairKey>,
    /// Workload names that are really another workload, applied last.
    pub aliases: &'a BTreeMap<WorkloadName, WorkloadName>,
}

/// Assign every groupable resource to a workload and environment.
pub fn group(input: &Input<'_>, cfg: &GroupingConfig) -> Vec<WorkloadMember> {
    Grouper::new(input, cfg).run()
}

/// The one value every item agrees on; `None` when there are no items or
/// they disagree.
fn unanimous<T: PartialEq>(items: impl IntoIterator<Item = T>) -> Option<T> {
    let mut items = items.into_iter();
    let first = items.next()?;
    items.all(|x| x == first).then_some(first)
}

/// Lookups over one scan's resources and relationships.
struct Index<'a> {
    by_arn: HashMap<&'a str, &'a Resource>,
    /// Stack id to the root stack's name.
    stack_root_name: HashMap<&'a str, String>,
    /// From ARN to (edge, to ARN).
    out: HashMap<&'a str, Vec<(Edge, &'a str)>>,
    /// To ARN to (edge, from ARN).
    inbound: HashMap<&'a str, Vec<(Edge, &'a str)>>,
    accounts: HashMap<&'a str, AccountNames<'a>>,
    /// Network ARN to the network's name. A network can be in another
    /// account (a shared VPC) or region (a global Google Cloud network)
    /// than its instances.
    vpc_names: HashMap<&'a str, &'a str>,
}

impl<'a> Index<'a> {
    fn new(input: &Input<'a>, cfg: &GroupingConfig) -> Self {
        let by_arn: HashMap<&str, &Resource> =
            input.resources.iter().map(|r| (r.arn.as_str(), r)).collect();

        // Cloud tooling groups all collapse into one platform workload.
        let is_platform = |name: &str| {
            cfg.platform_group_prefixes
                .iter()
                .any(|p| name.to_lowercase().starts_with(&p.to_lowercase()))
        };
        let mut stack_root_name = HashMap::new();
        let mut accounts = HashMap::new();
        let mut vpc_names = HashMap::new();
        for r in input.resources {
            match &r.body {
                Body::DeploymentGroup(stack) => {
                    let root = stack
                        .root_id
                        .as_deref()
                        .and_then(|id| by_arn.get(id))
                        .copied()
                        .unwrap_or(r);
                    if let Some(name) = root.name() {
                        let name = if is_platform(name) {
                            cfg.platform_workload.to_string()
                        } else {
                            name.to_string()
                        };
                        stack_root_name.insert(r.arn.as_str(), name);
                    }
                }
                Body::Account(account) => {
                    accounts.insert(
                        r.account_id.as_str(),
                        AccountNames {
                            name: Some(account.name.as_str()),
                            ou: account.parent_name.as_deref(),
                        },
                    );
                }
                Body::Network(vpc) => {
                    let name = r.tags.get("Name").or(vpc.name.as_ref());
                    if let Some(name) = name {
                        vpc_names.insert(r.arn.as_str(), name.as_str());
                    }
                }
                Body::Instance(_)
                | Body::InstanceType(_)
                | Body::Volume(_)
                | Body::Image(_)
                | Body::Subnet(_)
                | Body::SecurityGroup(_)
                | Body::SecurityGroupRule(_)
                | Body::RouteTable(_)
                | Body::InternetGateway(_)
                | Body::NatGateway(_)
                | Body::PeeringConnection(_)
                | Body::PrefixList(_)
                | Body::ScaleGroup(_)
                | Body::LaunchTemplate(_)
                | Body::LaunchConfiguration(_)
                | Body::PublicIp(_)
                | Body::LoadBalancer(_)
                | Body::TargetGroup(_)
                | Body::ManagedService(_) => {}
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

        Self { by_arn, stack_root_name, out, inbound, accounts, vpc_names }
    }

    fn get(&self, arn: &str) -> Option<&'a Resource> {
        self.by_arn.get(arn).copied()
    }

    /// The ARNs related to `arn` by `edge`, followed in `direction`.
    fn related(
        &self,
        arn: &str,
        edge: Edge,
        direction: Direction,
    ) -> impl Iterator<Item = &'a str> + '_ {
        let map = match direction {
            Direction::Out => &self.out,
            Direction::In => &self.inbound,
        };
        map.get(arn)
            .into_iter()
            .flatten()
            .filter(move |(e, _)| *e == edge)
            .map(|(_, other)| *other)
    }

    /// The root stack a resource belongs to: by an `in_deployment_group` edge from
    /// `ListStackResources`, else by the stack-id tag CloudFormation sets.
    fn stack_of(&self, r: &Resource) -> Option<&str> {
        self.related(r.arn.as_str(), Edge::InDeploymentGroup, Direction::Out)
            .find_map(|stack| self.stack_root_name.get(stack))
            .or_else(|| {
                r.tags
                    .get(STACK_ID_TAG)
                    .and_then(|id| self.stack_root_name.get(id.as_str()))
            })
            .map(String::as_str)
    }

    /// The name of the network an instance is in: its `Name` tag, else
    /// its own name, else its ARN.
    fn vpc_name(&self, r: &'a Resource) -> Option<&'a str> {
        let Body::Instance(instance) = &r.body else { return None };
        let id = instance.network_id.as_deref()?;
        Some(self.vpc_names.get(id).copied().unwrap_or(id))
    }
}

/// A raw placement before environment resolution.
struct Placed {
    arn: Arn,
    signal: GroupingSignal,
    slug: Slug,
    override_env: Option<EnvName>,
    env_tag: Option<String>,
    /// Environment token found in the resource's own name when the
    /// workload name came from somewhere else (a stack, a shared root).
    name_env: Option<EnvName>,
    /// The only environment of the workload this resource joined by name,
    /// when that workload has exactly one.
    joined_env: Option<EnvName>,
    account_id: AccountId,
}

impl Placed {
    fn new(
        r: &Resource,
        signal: GroupingSignal,
        slug: Slug,
        cfg: &GroupingConfig,
    ) -> Self {
        Self {
            arn: r.arn.clone(),
            signal,
            slug,
            override_env: None,
            env_tag: r.first_tag(&cfg.environment_tags).map(str::to_string),
            name_env: None,
            joined_env: None,
            account_id: r.account_id.clone(),
        }
    }
}

/// What placing managed services needs from the instances placed before
/// them.
struct ManagedContext {
    /// Names a managed service may be prefixed with: stack roots and
    /// placed instance workloads.
    strong_names: Vec<Slug>,
    /// Each managed resource's own name without its environment token;
    /// empty for resources a stack already accounts for.
    own_roots: Vec<Slug>,
    /// The environments each placed workload has.
    workload_envs: HashMap<WorkloadName, BTreeSet<EnvName>>,
}

impl ManagedContext {
    /// The roots of every managed resource other than the `i`th.
    fn siblings(&self, i: usize) -> Vec<Slug> {
        self.own_roots
            .iter()
            .enumerate()
            .filter(|(j, n)| *j != i && !n.is_empty())
            .map(|(_, n)| n.clone())
            .collect()
    }

    /// The single environment of a workload joined by name.
    fn only_env(&self, workload: &Slug) -> Option<EnvName> {
        let envs =
            self.workload_envs.get(&WorkloadName::from(workload.clone()))?;
        match envs.len() {
            1 => envs.first().cloned(),
            _ => None,
        }
    }
}

struct Grouper<'a> {
    input: &'a Input<'a>,
    cfg: &'a GroupingConfig,
    index: Index<'a>,
}

impl<'a> Grouper<'a> {
    fn new(input: &'a Input<'a>, cfg: &'a GroupingConfig) -> Self {
        Self { input, cfg, index: Index::new(input, cfg) }
    }

    fn resources(
        &self,
        kind: ResourceKind,
    ) -> impl Iterator<Item = &'a Resource> + '_ {
        self.input.resources.iter().filter(move |r| r.kind() == kind)
    }

    fn run(&self) -> Vec<WorkloadMember> {
        let mut members: Vec<WorkloadMember> = self
            .resources(ResourceKind::Instance)
            .map(|r| self.resolve(self.place_instance(r)))
            .collect();
        let managed = self.place_managed_services(&members);
        members.extend(managed);

        let mut placed: HashSet<Arn> =
            members.iter().map(|m| m.arn.clone()).collect();
        // Load balancers follow target groups, so they inherit after them.
        for stage in [false, true] {
            let inherited = self.inherit(&members, &placed, |kind| {
                (kind == ResourceKind::LoadBalancer) == stage
            });
            placed.extend(inherited.iter().map(|m| m.arn.clone()));
            members.extend(inherited);
        }
        let by_stack = self.place_by_stack(&placed);
        members.extend(by_stack);
        self.claim_for_single_workload_accounts(&mut members);

        members.sort_by(|a, b| (&a.pair, &a.arn).cmp(&(&b.pair, &b.arn)));
        members
    }

    fn place_instance(&self, r: &Resource) -> Placed {
        if let Some(o) = self.input.overrides.get(&r.arn) {
            let mut p = Placed::new(
                r,
                GroupingSignal::Override,
                Slug::new(o.workload.as_str()),
                self.cfg,
            );
            p.override_env = Some(o.environment.clone());
            return p;
        }

        let index = &self.index;
        let asg = || {
            index
                .related(
                    r.arn.as_str(),
                    Edge::MemberOfScaleGroup,
                    Direction::Out,
                )
                .find_map(|arn| index.get(arn))
                .map(|g| match index.stack_of(g) {
                    Some(root) => {
                        (root.to_string(), GroupingSignal::DeploymentGroup)
                    }
                    None => (
                        g.name().unwrap_or(g.arn.as_str()).to_string(),
                        GroupingSignal::ScaleGroup,
                    ),
                })
        };
        // The load balancer in front of the instance's target group, else
        // the target group itself.
        let target_group = || {
            index
                .related(r.arn.as_str(), Edge::TargetOf, Direction::Out)
                .filter_map(|tg_arn| {
                    let tg = index.get(tg_arn)?;
                    index
                        .related(tg_arn, Edge::AttachedTo, Direction::Out)
                        .filter_map(|lb| index.get(lb))
                        .filter(|lb| lb.kind() == ResourceKind::LoadBalancer)
                        .filter_map(Resource::name)
                        .min()
                        .or_else(|| tg.name())
                })
                .min()
        };

        let (raw, signal) = if let Some(name) = r.first_tag(&[OVERRIDE_TAG]) {
            (name.to_string(), GroupingSignal::OverrideTag)
        } else if let Some(root) = index.stack_of(r) {
            (root.to_string(), GroupingSignal::DeploymentGroup)
        } else if let Some((name, signal)) = asg() {
            (name, signal)
        } else if let Some(name) = target_group() {
            (name.to_string(), GroupingSignal::LoadBalancer)
        } else if let Some(name) = r.first_tag(&self.cfg.workload_tags) {
            (name.to_string(), GroupingSignal::Tag)
        } else if let Some(name) = index.vpc_name(r) {
            (name.to_string(), GroupingSignal::Network)
        } else {
            ("unassigned".to_string(), GroupingSignal::Unassigned)
        };
        Placed::new(r, signal, Slug::new(&raw), self.cfg)
    }

    /// A managed service's own name, with any generated suffix removed.
    fn managed_own_name(r: &Resource) -> Slug {
        Slug::new(r.name().unwrap_or("unassigned")).without_generated_suffix()
    }

    fn place_managed_services(
        &self,
        instances: &[WorkloadMember],
    ) -> Vec<WorkloadMember> {
        let tokens = &self.cfg.environment_tokens;
        let mut strong_names: Vec<Slug> = self
            .index
            .stack_root_name
            .values()
            .map(|n| Slug::new(n))
            .chain(
                instances
                    .iter()
                    .filter(|m| m.signal != GroupingSignal::Unassigned)
                    .map(|m| Slug::new(m.pair.workload.as_str())),
            )
            .filter(|n| *n != self.cfg.platform_workload)
            .collect();
        strong_names.sort();
        strong_names.dedup();

        let managed: Vec<&Resource> =
            self.resources(ResourceKind::ManagedService).collect();
        // Resources a stack already accounts for do not offer their names
        // as roots for the rest.
        let own_roots = managed
            .iter()
            .map(|r| {
                if self.index.stack_of(r).is_some() {
                    return Slug::default();
                }
                let own = Self::managed_own_name(r);
                match own.split_environment(tokens) {
                    Some((stripped, _)) => stripped,
                    None => own,
                }
            })
            .collect();

        let mut workload_envs: HashMap<WorkloadName, BTreeSet<EnvName>> =
            HashMap::new();
        for m in instances {
            workload_envs
                .entry(m.pair.workload.clone())
                .or_default()
                .insert(m.pair.environment.clone());
        }

        let ctx = ManagedContext { strong_names, own_roots, workload_envs };
        managed
            .iter()
            .enumerate()
            .map(|(i, r)| self.resolve(self.place_managed(r, &ctx, i)))
            .collect()
    }

    /// Place the `i`th managed service: its stack, a workload tag, the
    /// longest strong name its name starts with, the shortest root it
    /// shares with other managed resources, a strong name inside its name,
    /// a root most siblings share, else itself.
    fn place_managed(
        &self,
        r: &Resource,
        ctx: &ManagedContext,
        i: usize,
    ) -> Placed {
        let tokens = &self.cfg.environment_tokens;
        let own = Self::managed_own_name(r);
        let (own_root, name_env) = match own.split_environment(tokens) {
            Some((root, env)) => (root, Some(env)),
            None => (own.clone(), None),
        };
        let siblings = ctx.siblings(i);
        let (slug, signal) = if let Some(root) = self.index.stack_of(r) {
            (Slug::new(root), GroupingSignal::DeploymentGroup)
        } else if let Some(name) = r.first_tag(&self.cfg.workload_tags) {
            (Slug::new(name), GroupingSignal::Tag)
        } else if let Some(prefix) = own.longest_prefix(&ctx.strong_names) {
            (prefix.clone(), GroupingSignal::NamePrefix)
        } else if let Some(root) = own_root.shortest_prefix(&siblings) {
            (root.clone(), GroupingSignal::NamePrefix)
        } else if let Some(name) = own.contains_segments(&ctx.strong_names) {
            (name.clone(), GroupingSignal::NameContains)
        } else if let Some(root) = own_root.shared_root(&siblings, tokens) {
            (root, GroupingSignal::NamePrefix)
        } else {
            (own, GroupingSignal::Unassigned)
        };
        let joined_env =
            if signal.is_name_match() { ctx.only_env(&slug) } else { None };
        let mut p = Placed::new(r, signal, slug, self.cfg);
        p.name_env = name_env;
        p.joined_env = joined_env;
        p
    }

    /// Resolve the environment and final workload name for a placement.
    /// Order: override, environment tag, a token in the workload name, a
    /// token in the resource's own name, the single environment of a
    /// workload joined by name, a token in the account or OU name, then
    /// the default. The name token is stripped only when it agrees with
    /// the environment chosen.
    fn resolve(&self, p: Placed) -> WorkloadMember {
        let cfg = self.cfg;
        let split = p.slug.split_environment(&cfg.environment_tokens);
        let from_tag = p.env_tag.as_deref().map(|t| {
            let t = Slug::new(t);
            cfg.environment_tokens
                .get(t.as_str())
                .cloned()
                .unwrap_or_else(|| EnvName::from(t))
        });
        let from_account = || {
            self.index
                .accounts
                .get(p.account_id.as_str())
                .and_then(|a| a.environment(&cfg.environment_tokens))
        };

        let environment = p
            .override_env
            .or(from_tag)
            .or_else(|| split.as_ref().map(|(_, env)| env.clone()))
            .or(p.name_env)
            .or(p.joined_env)
            .or_else(from_account)
            .unwrap_or_else(|| cfg.default_environment.clone());

        let workload = match split {
            Some((stripped, env)) if env == environment => stripped,
            Some(_) | None => p.slug,
        };
        let workload = WorkloadName::from(
            workload
                .without_layer(&cfg.layer_tokens)
                .without_kind_prefix(&cfg.kind_prefixes),
        );
        let workload = folded(self.input.aliases, &workload);

        WorkloadMember {
            arn: p.arn,
            pair: PairKey { workload, environment },
            signal: p.signal,
        }
    }

    /// Resources of the kinds `stage` selects that join the workload of
    /// the placed resources they are related to, when those all agree.
    fn inherit(
        &self,
        members: &[WorkloadMember],
        placed: &HashSet<Arn>,
        stage: impl Fn(ResourceKind) -> bool,
    ) -> Vec<WorkloadMember> {
        let owner: HashMap<&str, &PairKey> =
            members.iter().map(|m| (m.arn.as_str(), &m.pair)).collect();
        let mut inherited = Vec::new();
        for r in self.input.resources {
            if !stage(r.kind()) || placed.contains(&r.arn) {
                continue;
            }
            let Some(via) = r.kind().inherits_via() else { continue };
            let owners = self
                .index
                .related(r.arn.as_str(), via.edge, via.direction)
                .filter_map(|other| owner.get(other).copied());
            if let Some(pair) = unanimous(owners) {
                inherited.push(WorkloadMember {
                    arn: r.arn.clone(),
                    pair: pair.clone(),
                    signal: GroupingSignal::Attached,
                });
            }
        }
        inherited
    }

    /// Anything that would inherit but is still unplaced, and that
    /// CloudFormation says belongs to a stack: security groups no collected
    /// instance uses, volumes with no attachment, and the like.
    fn place_by_stack(&self, placed: &HashSet<Arn>) -> Vec<WorkloadMember> {
        let mut out = Vec::new();
        for r in self.input.resources {
            if r.kind().inherits_via().is_none() || placed.contains(&r.arn) {
                continue;
            }
            if let Some(root) = self.index.stack_of(r) {
                out.push(self.resolve(Placed::new(
                    r,
                    GroupingSignal::DeploymentGroup,
                    Slug::new(root),
                    self.cfg,
                )));
            }
        }
        out
    }

    /// Last resort: an account that hosts exactly one workload claims the
    /// unassigned resources in it. Platform resources never count as that
    /// workload.
    fn claim_for_single_workload_accounts(
        &self,
        members: &mut [WorkloadMember],
    ) {
        let account_of = |m: &WorkloadMember| {
            self.index.get(m.arn.as_str()).map(|r| r.account_id.as_str())
        };
        let mut per_account: HashMap<&str, BTreeSet<PairKey>> = HashMap::new();
        for m in members.iter() {
            if m.signal == GroupingSignal::Unassigned
                || m.pair.workload == self.cfg.platform_workload
            {
                continue;
            }
            let Some(account) = account_of(m) else { continue };
            per_account.entry(account).or_default().insert(m.pair.clone());
        }
        for m in members.iter_mut() {
            if m.signal != GroupingSignal::Unassigned {
                continue;
            }
            let Some(pairs) = account_of(m).and_then(|a| per_account.get(a))
            else {
                continue;
            };
            if unanimous(pairs.iter().map(|p| &p.workload)).is_none() {
                continue;
            }
            let Some(only) = pairs.first() else { continue };
            m.pair.workload = only.workload.clone();
            // Keep the resource's own environment when it had evidence for
            // one; otherwise take the account's single pair.
            if m.pair.environment == self.cfg.default_environment {
                m.pair.environment = only.environment.clone();
            }
            m.signal = GroupingSignal::AccountOnly;
        }
    }
}

#[cfg(test)]
mod tests;
