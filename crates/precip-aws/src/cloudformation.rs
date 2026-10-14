//! CloudFormation stacks, for the stack hierarchy. Membership of a
//! resource in a stack comes from the `aws:cloudformation:stack-id` tag
//! CloudFormation puts on everything it creates.

use precip_types::Arn;
use precip_types::scan::{Edge, Relationship, Resource, ResourceKind};

use crate::collect::{
    BoxFuture, CollectError, Collected, Collector, RegionContext, tags,
};

pub struct Stacks;

impl Collector for Stacks {
    fn name(&self) -> &'static str {
        "cloudformation:DescribeStacks"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_cloudformation::Client::new(&ctx.config);
            let mut out = Collected::default();

            let mut pages = client.describe_stacks().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for stack in page.stacks() {
                    let Some(id) = stack.stack_id() else { continue };
                    out.resources.push(Resource {
                        arn: Arn::from(id),
                        kind: ResourceKind::CloudFormationStack,
                        account_id: ctx.identity.account_id.clone(),
                        region: ctx.region.clone(),
                        tags: tags(stack.tags(), |t| t.key(), |t| t.value()),
                        body: serde_json::json!({
                            "name": stack.stack_name(),
                            "parent_id": stack.parent_id(),
                            "root_id": stack.root_id(),
                            "status": stack.stack_status().map(|s| s.as_str()),
                        }),
                    });
                }
            }
            tracing::info!(
                api = self.name(),
                count = out.resources.len(),
                "collected"
            );
            Ok(out)
        })
    }
}

/// Every physical resource in every stack, as `in_stack` edges. This is
/// the authoritative stack membership: it covers resource types whose own
/// list calls return no tags. Runs after the stacks and the resources
/// themselves, so physical ids can be matched to collected ARNs.
pub struct StackResources;

impl Collector for StackResources {
    fn needs_prior(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "cloudformation:ListStackResources"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let mut out = Collected::default();
            let stacks: Vec<&str> = prior
                .of_kind(ResourceKind::CloudFormationStack)
                .map(|s| s.arn.as_str())
                .collect();
            if stacks.is_empty() {
                return Ok(out);
            }
            let by_name = arns_by_name(&prior.resources);
            let client = aws_sdk_cloudformation::Client::new(&ctx.config);
            for stack in stacks {
                let mut pages = client
                    .list_stack_resources()
                    .stack_name(stack)
                    .into_paginator()
                    .send();
                while let Some(page) = pages.next().await {
                    let page = page.map_err(CollectError::new(self.name()))?;
                    for r in page.stack_resource_summaries() {
                        let (Some(ty), Some(id)) =
                            (r.resource_type(), r.physical_resource_id())
                        else {
                            continue;
                        };
                        if let Some(arn) = member_arn(ctx, ty, id, &by_name) {
                            out.relationships.push(Relationship {
                                from_arn: arn,
                                edge: Edge::InStack,
                                to_arn: Arn::from(stack),
                            });
                        }
                    }
                }
            }
            tracing::info!(
                api = self.name(),
                count = out.relationships.len(),
                "collected"
            );
            Ok(out)
        })
    }
}

/// Collected ARNs keyed by kind, managed service and name.
type ByName<'a> = std::collections::HashMap<
    (ResourceKind, Option<&'a str>, &'a str),
    &'a str,
>;

/// Resources whose ARN embeds a name rather than an id are matched by name
/// against what was collected. Managed services share one kind, so the
/// service is part of the key: an ElastiCache cluster and an RDS instance
/// with the same name stay apart.
fn arns_by_name(resources: &[Resource]) -> ByName<'_> {
    resources
        .iter()
        .filter_map(|r| {
            let key = if r.kind == ResourceKind::ElasticIp {
                "public_ip"
            } else {
                "name"
            };
            let name = r.body.get(key)?.as_str()?;
            let service = r.body.get("service").and_then(|v| v.as_str());
            Some(((r.kind, service, name), r.arn.as_str()))
        })
        .collect()
}

/// The ARN precip uses for a stack resource, from its CloudFormation type
/// and physical id. Types precip does not collect return `None`.
fn member_arn(
    ctx: &RegionContext,
    resource_type: &str,
    physical_id: &str,
    by_name: &ByName<'_>,
) -> Option<Arn> {
    let ec2 = |resource: String| Some(ctx.arn("ec2", &resource));
    match resource_type {
        "AWS::EC2::Instance" => ec2(format!("instance/{physical_id}")),
        "AWS::EC2::Volume" => ec2(format!("volume/{physical_id}")),
        "AWS::EC2::SecurityGroup" => {
            ec2(format!("security-group/{physical_id}"))
        }
        "AWS::EC2::VPC" => ec2(format!("vpc/{physical_id}")),
        "AWS::EC2::Subnet" => ec2(format!("subnet/{physical_id}")),
        // Physical id of an EIP is its address, matched below by name map
        // keyed on public_ip.
        "AWS::EC2::EIP" => by_name
            .get(&(ResourceKind::ElasticIp, None, physical_id))
            .map(|a| Arn::from(*a)),
        "AWS::Lambda::Function" => {
            Some(ctx.arn("lambda", &format!("function:{physical_id}")))
        }
        "AWS::DynamoDB::Table" => {
            Some(ctx.arn("dynamodb", &format!("table/{physical_id}")))
        }
        "AWS::RDS::DBInstance" => {
            Some(ctx.arn("rds", &format!("db:{physical_id}")))
        }
        "AWS::EKS::Cluster" => {
            Some(ctx.arn("eks", &format!("cluster/{physical_id}")))
        }
        "AWS::EFS::FileSystem" => Some(
            ctx.arn("elasticfilesystem", &format!("file-system/{physical_id}")),
        ),
        // Physical id is already an ARN.
        "AWS::ECS::Cluster"
        | "AWS::ElasticLoadBalancingV2::LoadBalancer"
        | "AWS::ElasticLoadBalancingV2::TargetGroup"
            if physical_id.starts_with("arn:") =>
        {
            Some(Arn::from(physical_id))
        }
        // Physical id is a name; the ARN carries an opaque id.
        "AWS::AutoScaling::AutoScalingGroup" => by_name
            .get(&(ResourceKind::AutoScalingGroup, None, physical_id))
            .map(|a| Arn::from(*a)),
        "AWS::ElastiCache::CacheCluster" => by_name
            .get(&(
                ResourceKind::ManagedService,
                Some("elasticache"),
                physical_id,
            ))
            .map(|a| Arn::from(*a)),
        _ => None,
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
        let by_name = arns_by_name(&resources);
        assert_eq!(
            member_arn(
                &ctx,
                "AWS::ElastiCache::CacheCluster",
                "shared",
                &by_name
            ),
            Some(Arn::from(cache))
        );
        let only_rds = vec![managed("rds", "shared", rds)];
        let by_name = arns_by_name(&only_rds);
        assert_eq!(
            member_arn(
                &ctx,
                "AWS::ElastiCache::CacheCluster",
                "shared",
                &by_name
            ),
            None
        );
    }
}
