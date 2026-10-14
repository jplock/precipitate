//! CloudFormation stacks, for the stack hierarchy. Membership of a
//! resource in a stack comes from the `aws:cloudformation:stack-id` tag
//! CloudFormation puts on everything it creates.

use precip_types::Arn;
use precip_types::scan::{Edge, Resource, ResourceKind};
use std::collections::HashMap;

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, next_page, tags,
};

pub struct Stacks;

impl Collect for Stacks {
    const API: &'static str = "cloudformation:DescribeStacks";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_cloudformation::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = client.describe_stacks().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for stack in page.stacks() {
                let Some(id) = stack.stack_id() else { continue };
                out.resources.push(ctx.resource(
                    Arn::from(id),
                    ResourceKind::CloudFormationStack,
                    tags(stack.tags()),
                    serde_json::json!({
                        "name": stack.stack_name(),
                        "parent_id": stack.parent_id(),
                        "root_id": stack.root_id(),
                        "status": stack.stack_status().map(|s| s.as_str()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// Every physical resource in every stack, as `in_stack` edges. This is
/// the authoritative stack membership: it covers resource types whose own
/// list calls return no tags. Runs after the stacks and the resources
/// themselves, so physical ids can be matched to collected ARNs.
pub struct StackResources;

impl Collect for StackResources {
    const API: &'static str = "cloudformation:ListStackResources";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut out = Collected::default();
        let stacks: Vec<&str> = prior
            .of_kind(ResourceKind::CloudFormationStack)
            .map(|s| s.arn.as_str())
            .collect();
        if stacks.is_empty() {
            return Ok(out);
        }
        let by_name = ByName::new(&prior.resources);
        let client = aws_sdk_cloudformation::Client::new(&ctx.config);
        for stack in stacks {
            let mut pages = client
                .list_stack_resources()
                .stack_name(stack)
                .into_paginator()
                .send();
            while let Some(page) = next_page(Self::API, &mut pages).await? {
                for r in page.stack_resource_summaries() {
                    let (Some(ty), Some(id)) =
                        (r.resource_type(), r.physical_resource_id())
                    else {
                        continue;
                    };
                    if let Some(arn) = by_name.member_arn(ctx, ty, id) {
                        out.relate(arn, Edge::InStack, Arn::from(stack));
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Collected ARNs keyed by kind, managed service and name. Resources whose
/// ARN embeds a name rather than an id are matched by name against what
/// was collected. Managed services share one kind, so the service is part
/// of the key: an ElastiCache cluster and an RDS instance with the same
/// name stay apart.
struct ByName<'a>(HashMap<(ResourceKind, Option<&'a str>, &'a str), &'a str>);

impl<'a> ByName<'a> {
    fn new(resources: &'a [Resource]) -> Self {
        Self(
            resources
                .iter()
                .filter_map(|r| {
                    let name = if r.kind == ResourceKind::ElasticIp {
                        r.body_str("public_ip")
                    } else {
                        r.name()
                    }?;
                    Some((
                        (r.kind, r.body_str("service"), name),
                        r.arn.as_str(),
                    ))
                })
                .collect(),
        )
    }

    fn get(
        &self,
        kind: ResourceKind,
        service: Option<&str>,
        name: &str,
    ) -> Option<Arn> {
        self.0.get(&(kind, service, name)).map(|a| Arn::from(*a))
    }

    /// The ARN precip uses for a stack resource, from its CloudFormation
    /// type and physical id. Types precip does not collect return `None`.
    fn member_arn(
        &self,
        ctx: &RegionContext,
        resource_type: &str,
        physical_id: &str,
    ) -> Option<Arn> {
        if let Some(ty) = ArnType::from_cloudformation(resource_type) {
            return Some(ctx.arn(ty, physical_id));
        }
        match resource_type {
            // Physical id of an EIP is its address.
            "AWS::EC2::EIP" => {
                self.get(ResourceKind::ElasticIp, None, physical_id)
            }
            // Physical id is already an ARN.
            "AWS::ECS::Cluster"
            | "AWS::ElasticLoadBalancingV2::LoadBalancer"
            | "AWS::ElasticLoadBalancingV2::TargetGroup"
                if physical_id.starts_with("arn:") =>
            {
                Some(Arn::from(physical_id))
            }
            // Physical id is a name; the ARN carries an opaque id.
            "AWS::AutoScaling::AutoScalingGroup" => {
                self.get(ResourceKind::AutoScalingGroup, None, physical_id)
            }
            "AWS::ElastiCache::CacheCluster" => self.get(
                ResourceKind::ManagedService,
                Some("elasticache"),
                physical_id,
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(service: &str, name: &str, arn: &str) -> Resource {
        Resource {
            arn: arn.into(),
            kind: ResourceKind::ManagedService,
            account_id: "123456789012".into(),
            region: "us-east-1".into(),
            tags: Default::default(),
            body: serde_json::json!({ "service": service, "name": name }),
        }
    }

    #[test]
    fn cache_clusters_resolve_only_to_elasticache() {
        let ctx = RegionContext {
            config: aws_config::SdkConfig::builder().build(),
            identity: crate::accounts::CallerIdentity {
                account_id: "123456789012".into(),
                arn: "arn:aws:iam::123456789012:user/x".into(),
                partition: "aws".into(),
            },
            region: "us-east-1".into(),
        };
        let cache = "arn:aws:elasticache:us-east-1:123456789012:cluster:shared";
        let rds = "arn:aws:rds:us-east-1:123456789012:db:shared";
        let resources = vec![
            managed("rds", "shared", rds),
            managed("elasticache", "shared", cache),
        ];
        let by_name = ByName::new(&resources);
        assert_eq!(
            by_name.member_arn(
                &ctx,
                "AWS::ElastiCache::CacheCluster",
                "shared"
            ),
            Some(Arn::from(cache))
        );
        assert_eq!(
            by_name.member_arn(&ctx, "AWS::RDS::DBInstance", "shared"),
            Some(Arn::from(rds)),
            "id-based types build the ARN without a lookup"
        );
        assert_eq!(by_name.member_arn(&ctx, "AWS::S3::Bucket", "b"), None);
        let only_rds = vec![managed("rds", "shared", rds)];
        let by_name = ByName::new(&only_rds);
        assert_eq!(
            by_name.member_arn(
                &ctx,
                "AWS::ElastiCache::CacheCluster",
                "shared"
            ),
            None
        );
    }
}
