//! Load balancers and target groups (ELBv2). Instances behind one load
//! balancer are one workload.

use precip_types::scan::{Edge, Relationship, ResourceKind};

use crate::collect::{
    BoxFuture, CollectError, Collected, Collector, RegionContext,
};

pub struct LoadBalancers;

impl Collector for LoadBalancers {
    fn name(&self) -> &'static str {
        "elasticloadbalancing:DescribeLoadBalancers"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client =
                aws_sdk_elasticloadbalancingv2::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages =
                client.describe_load_balancers().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for lb in page.load_balancers() {
                    let Some(arn) = lb.load_balancer_arn() else { continue };
                    out.resources.push(ctx.resource(
                        arn.to_string(),
                        ResourceKind::LoadBalancer,
                        Default::default(),
                        serde_json::json!({
                            "name": lb.load_balancer_name(),
                            "type": lb.r#type().map(|t| t.as_str()),
                            "scheme": lb.scheme().map(|s| s.as_str()),
                            "vpc_id": lb.vpc_id(),
                            "dns_name": lb.dns_name(),
                            "security_groups": lb.security_groups(),
                        }),
                    ));
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

/// Target groups plus their healthy and unhealthy instance targets, as
/// `target_of` edges from instance to target group.
pub struct TargetGroups;

impl Collector for TargetGroups {
    fn name(&self) -> &'static str {
        "elasticloadbalancing:DescribeTargetGroups"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client =
                aws_sdk_elasticloadbalancingv2::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages =
                client.describe_target_groups().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for tg in page.target_groups() {
                    let Some(arn) = tg.target_group_arn() else { continue };
                    let is_instance_target = tg
                        .target_type()
                        .is_some_and(|t| t.as_str() == "instance");
                    let mut targets = 0usize;
                    if is_instance_target {
                        let health = client
                            .describe_target_health()
                            .target_group_arn(arn)
                            .send()
                            .await
                            .map_err(CollectError::new(
                                "elasticloadbalancing:DescribeTargetHealth",
                            ))?;
                        for d in health.target_health_descriptions() {
                            let Some(id) = d.target().and_then(|t| t.id())
                            else {
                                continue;
                            };
                            targets += 1;
                            out.relationships.push(Relationship {
                                from_arn: ctx
                                    .arn("ec2", &format!("instance/{id}")),
                                edge: Edge::TargetOf,
                                to_arn: arn.to_string(),
                            });
                        }
                    }
                    for lb in tg.load_balancer_arns() {
                        out.relationships.push(Relationship {
                            from_arn: arn.to_string(),
                            edge: Edge::AttachedTo,
                            to_arn: lb.clone(),
                        });
                    }
                    out.resources.push(ctx.resource(
                        arn.to_string(),
                        ResourceKind::TargetGroup,
                        Default::default(),
                        serde_json::json!({
                            "name": tg.target_group_name(),
                            "target_type": tg.target_type().map(|t| t.as_str()),
                            "protocol": tg.protocol().map(|p| p.as_str()),
                            "port": tg.port(),
                            "vpc_id": tg.vpc_id(),
                            "load_balancer_arns": tg.load_balancer_arns(),
                            "instance_targets": targets,
                        }),
                    ));
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
