//! Auto Scaling groups. The group is the resource; its instances are
//! observed counts, not tracked by id.

use precip_types::Arn;
use precip_types::scan::{Edge, Relationship, Resource, ResourceKind};

use crate::collect::{
    BoxFuture, CollectError, Collected, Collector, RegionContext, tags,
};

pub struct Groups;

impl Collector for Groups {
    fn name(&self) -> &'static str {
        "autoscaling:DescribeAutoScalingGroups"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_autoscaling::Client::new(&ctx.config);
            let mut out = Collected::default();

            let mut pages =
                client.describe_auto_scaling_groups().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for group in page.auto_scaling_groups() {
                    let Some(arn) = group.auto_scaling_group_arn() else {
                        continue;
                    };
                    let mut instance_types: Vec<String> = group
                        .mixed_instances_policy()
                        .and_then(|p| p.launch_template())
                        .map(|lt| {
                            lt.overrides()
                                .iter()
                                .filter_map(|o| {
                                    o.instance_type().map(str::to_string)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    instance_types.sort();
                    instance_types.dedup();

                    for inst in group.instances() {
                        if let Some(id) = inst.instance_id() {
                            out.relationships.push(Relationship {
                                from_arn: ctx
                                    .arn("ec2", &format!("instance/{id}")),
                                edge: Edge::MemberOfAsg,
                                to_arn: Arn::from(arn),
                            });
                        }
                    }

                    out.resources.push(Resource {
                        arn: Arn::from(arn),
                        kind: ResourceKind::AutoScalingGroup,
                        account_id: ctx.identity.account_id.clone(),
                        region: ctx.region.clone(),
                        tags: tags(group.tags(), |t| t.key(), |t| t.value()),
                        body: serde_json::json!({
                            "name": group.auto_scaling_group_name(),
                            "min_size": group.min_size(),
                            "desired_capacity": group.desired_capacity(),
                            "max_size": group.max_size(),
                            "observed_instances": group.instances().len(),
                            "launch_template_id": group
                                .launch_template()
                                .and_then(|lt| lt.launch_template_id())
                                .or_else(|| group
                                    .mixed_instances_policy()
                                    .and_then(|p| p.launch_template())
                                    .and_then(|lt| lt.launch_template_specification())
                                    .and_then(|s| s.launch_template_id())),
                            "launch_configuration_name": group.launch_configuration_name(),
                            "launch_template_version": group
                                .launch_template()
                                .and_then(|lt| lt.version())
                                .or_else(|| group
                                    .mixed_instances_policy()
                                    .and_then(|p| p.launch_template())
                                    .and_then(|lt| lt.launch_template_specification())
                                    .and_then(|s| s.version())),
                            "instance_types": instance_types,
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

/// The launch template version each ASG points at, for its instance type.
/// Runs after the groups so it only describes templates in use.
pub struct LaunchTemplates;

impl Collector for LaunchTemplates {
    fn needs_prior(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "ec2:DescribeLaunchTemplateVersions"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let mut out = Collected::default();
            // (template id, version) pairs the groups reference.
            let mut wanted: Vec<(String, String)> = prior
                .of_kind(ResourceKind::AutoScalingGroup)
                .filter_map(|g| {
                    let id = g.body.get("launch_template_id")?.as_str()?;
                    let version = g
                        .body
                        .get("launch_template_version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("$Default");
                    Some((id.to_string(), version.to_string()))
                })
                .collect();
            wanted.sort();
            wanted.dedup();
            if wanted.is_empty() {
                return Ok(out);
            }
            let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
            for (id, version) in wanted {
                let page = ec2
                    .describe_launch_template_versions()
                    .launch_template_id(&id)
                    .versions(&version)
                    .send()
                    .await
                    .map_err(CollectError::new(self.name()))?;
                for v in page.launch_template_versions() {
                    let Some(data) = v.launch_template_data() else { continue };
                    let number = v.version_number().unwrap_or_default();
                    let arn = ctx
                        .arn("ec2", &format!("launch-template/{id}:{number}"));
                    out.resources.push(ctx.resource(
                        arn,
                        ResourceKind::LaunchTemplate,
                        Default::default(),
                        serde_json::json!({
                            "launch_template_id": id,
                            "name": v.launch_template_name(),
                            "version": number,
                            "requested_version": version,
                            "instance_type": data.instance_type().map(|t| t.as_str()),
                            "image_id": data.image_id(),
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

/// Launch configurations the groups reference. Older than launch
/// templates and no longer creatable in new accounts, but existing groups
/// keep using them.
pub struct LaunchConfigurations;

impl Collector for LaunchConfigurations {
    fn needs_prior(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "autoscaling:DescribeLaunchConfigurations"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let mut out = Collected::default();
            let names = prior.distinct_field(
                ResourceKind::AutoScalingGroup,
                "launch_configuration_name",
            );
            if names.is_empty() {
                return Ok(out);
            }
            let client = aws_sdk_autoscaling::Client::new(&ctx.config);
            // The API accepts up to 50 names per call.
            for chunk in names.chunks(50) {
                let mut pages = client
                    .describe_launch_configurations()
                    .set_launch_configuration_names(Some(chunk.to_vec()))
                    .into_paginator()
                    .send();
                while let Some(page) = pages.next().await {
                    let page = page.map_err(CollectError::new(self.name()))?;
                    for lc in page.launch_configurations() {
                        let Some(arn) = lc.launch_configuration_arn() else {
                            continue;
                        };
                        out.resources.push(ctx.resource(
                            Arn::from(arn),
                            ResourceKind::LaunchConfiguration,
                            Default::default(),
                            serde_json::json!({
                                "name": lc.launch_configuration_name(),
                                "instance_type": lc.instance_type(),
                                "image_id": lc.image_id(),
                                "created": lc.created_time().map(|t| t.to_string()),
                            }),
                        ));
                    }
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
