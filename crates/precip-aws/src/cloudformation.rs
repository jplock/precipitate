//! CloudFormation stacks, for the stack hierarchy. Membership of a
//! resource in a stack comes from the `aws:cloudformation:stack-id` tag
//! CloudFormation puts on everything it creates.

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
                        arn: id.to_string(),
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
            // Resources whose ARN embeds a name rather than an id are
            // matched by name against what was collected.
            let by_name: std::collections::HashMap<(ResourceKind, &str), &str> =
                prior
                    .resources
                    .iter()
                    .filter_map(|r| {
                        let key = match r.kind {
                            ResourceKind::ElasticIp => "public_ip",
                            _ => "name",
                        };
                        let name = r.body.get(key)?.as_str()?;
                        Some(((r.kind, name), r.arn.as_str()))
                    })
                    .collect();
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
                                to_arn: stack.to_string(),
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

/// The ARN precip uses for a stack resource, from its CloudFormation type
/// and physical id. Types precip does not collect return `None`.
fn member_arn(
    ctx: &RegionContext,
    resource_type: &str,
    physical_id: &str,
    by_name: &std::collections::HashMap<(ResourceKind, &str), &str>,
) -> Option<String> {
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
            .get(&(ResourceKind::ElasticIp, physical_id))
            .map(|a| a.to_string()),
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
            Some(physical_id.to_string())
        }
        // Physical id is a name; the ARN carries an opaque id.
        "AWS::AutoScaling::AutoScalingGroup" => by_name
            .get(&(ResourceKind::AutoScalingGroup, physical_id))
            .map(|a| a.to_string()),
        "AWS::ElastiCache::CacheCluster" => by_name
            .get(&(ResourceKind::ManagedService, physical_id))
            .map(|a| a.to_string()),
        _ => None,
    }
}
