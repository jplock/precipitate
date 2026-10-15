//! CloudFormation stacks, for the stack hierarchy. Membership of a
//! resource in a stack comes from the `aws:cloudformation:stack-id` tag
//! CloudFormation puts on everything it creates.

use precip_types::scan::{Edge, Resource, ResourceKind};
use precip_types::{Arn, Body, body};
use std::collections::HashMap;

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, next_page, own,
    tags,
};

pub(crate) struct Stacks;

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
                    tags(stack.tags()),
                    Body::CloudFormationStack(body::Stack {
                        name: own(stack.stack_name()),
                        parent_id: own(stack.parent_id()),
                        root_id: own(stack.root_id()),
                        status: own(stack.stack_status().map(|s| s.as_str())),
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
pub(crate) struct StackResources;

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
                    let (name, service) = if let Body::ElasticIp(e) = &r.body {
                        (e.public_ip.as_deref()?, None)
                    } else if let Body::ManagedService(m) = &r.body {
                        (m.name.as_deref()?, Some(m.service.as_str()))
                    } else {
                        (r.name()?, None)
                    };
                    Some(((r.kind(), service, name), r.arn.as_str()))
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
    use crate::accounts::CallerIdentity;

    fn managed(service: body::Service, name: &str, arn: &str) -> Resource {
        Resource {
            arn: arn.into(),
            account_id: "123456789012".into(),
            region: "us-east-1".into(),
            tags: Default::default(),
            body: Body::ManagedService(body::Managed {
                name: Some(name.into()),
                service,
            }),
        }
    }

    fn rds_service() -> body::Service {
        body::Service::Rds {
            engine: None,
            engine_version: None,
            instance_class: None,
            allocated_storage_gib: None,
            multi_az: None,
            vpc_id: None,
        }
    }

    fn cache_service() -> body::Service {
        body::Service::ElastiCache {
            engine: None,
            node_type: None,
            nodes: None,
        }
    }

    #[test]
    fn cache_clusters_resolve_only_to_elasticache() {
        let ctx = RegionContext {
            config: aws_config::SdkConfig::builder().build(),
            identity: CallerIdentity {
                account_id: "123456789012".into(),
                arn: "arn:aws:iam::123456789012:user/x".into(),
                partition: "aws".into(),
            },
            region: "us-east-1".into(),
        };
        let cache = "arn:aws:elasticache:us-east-1:123456789012:cluster:shared";
        let rds = "arn:aws:rds:us-east-1:123456789012:db:shared";
        let resources = vec![
            managed(rds_service(), "shared", rds),
            managed(cache_service(), "shared", cache),
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
        let only_rds = vec![managed(rds_service(), "shared", rds)];
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
