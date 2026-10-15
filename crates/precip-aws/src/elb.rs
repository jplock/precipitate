//! Load balancers and target groups (ELBv2). Instances behind one load
//! balancer are one workload.

use aws_sdk_elasticloadbalancingv2::types::TargetTypeEnum;
use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, next_page, own, owned,
};

pub(crate) struct LoadBalancers;

impl Collect for LoadBalancers {
    const API: &'static str = "elasticloadbalancing:DescribeLoadBalancers";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_elasticloadbalancingv2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages =
            client.describe_load_balancers().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for lb in page.load_balancers() {
                let Some(arn) = lb.load_balancer_arn() else { continue };
                out.resources.push(ctx.resource(
                    Arn::from(arn),
                    Default::default(),
                    Body::LoadBalancer(body::LoadBalancer {
                        name: own(lb.load_balancer_name()),
                        lb_type: own(lb.r#type().map(|t| t.as_str())),
                        scheme: own(lb.scheme().map(|s| s.as_str())),
                        vpc_id: own(lb.vpc_id()),
                        dns_name: own(lb.dns_name()),
                        security_groups: owned(
                            lb.security_groups().iter().map(String::as_str),
                        ),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// Target groups plus their healthy and unhealthy instance targets, as
/// `target_of` edges from instance to target group.
pub(crate) struct TargetGroups;

impl Collect for TargetGroups {
    const API: &'static str = "elasticloadbalancing:DescribeTargetGroups";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_elasticloadbalancingv2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = client.describe_target_groups().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for tg in page.target_groups() {
                let Some(arn) = tg.target_group_arn() else { continue };
                let mut targets = 0usize;
                if tg.target_type() == Some(&TargetTypeEnum::Instance) {
                    let health = client
                        .describe_target_health()
                        .target_group_arn(arn)
                        .send()
                        .await
                        .map_err(CollectError::new(
                            "elasticloadbalancing:DescribeTargetHealth",
                        ))?;
                    for d in health.target_health_descriptions() {
                        let Some(id) = d.target().and_then(|t| t.id()) else {
                            continue;
                        };
                        targets += 1;
                        out.relate(
                            ctx.arn(ArnType::Instance, id),
                            Edge::TargetOf,
                            Arn::from(arn),
                        );
                    }
                }
                for lb in tg.load_balancer_arns() {
                    out.relate(
                        Arn::from(arn),
                        Edge::AttachedTo,
                        Arn::from(lb.as_str()),
                    );
                }
                out.resources.push(ctx.resource(
                    Arn::from(arn),
                    Default::default(),
                    Body::TargetGroup(body::TargetGroup {
                        name: own(tg.target_group_name()),
                        target_type: own(tg.target_type().map(|t| t.as_str())),
                        protocol: own(tg.protocol().map(|p| p.as_str())),
                        port: tg.port(),
                        vpc_id: own(tg.vpc_id()),
                        load_balancer_arns: owned(
                            tg.load_balancer_arns().iter().map(String::as_str),
                        ),
                        instance_targets: targets,
                    }),
                ));
            }
        }
        Ok(out)
    }
}
