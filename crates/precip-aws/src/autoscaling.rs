//! Auto Scaling groups. The group is the resource; its instances are
//! observed counts, not tracked by id.

use precip_types::scan::Edge;
use precip_types::{Arn, Body, body};

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, next_page, own,
    owned, tags,
};

pub(crate) struct Groups;

impl Collect for Groups {
    type Context = RegionContext;
    const API: &'static str = "autoscaling:DescribeAutoScalingGroups";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_autoscaling::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages =
            client.describe_auto_scaling_groups().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for group in page.auto_scaling_groups() {
                let Some(arn) = group.auto_scaling_group_arn() else {
                    continue;
                };
                let mixed = group
                    .mixed_instances_policy()
                    .and_then(|p| p.launch_template());
                let mut instance_types: Vec<&str> = mixed
                    .map(|lt| {
                        lt.overrides()
                            .iter()
                            .filter_map(|o| o.instance_type())
                            .collect()
                    })
                    .unwrap_or_default();
                instance_types.sort_unstable();
                instance_types.dedup();
                let mixed_spec =
                    mixed.and_then(|lt| lt.launch_template_specification());

                for inst in group.instances() {
                    if let Some(id) = inst.instance_id() {
                        out.relate(
                            ctx.arn(ArnType::Instance, id),
                            Edge::MemberOfScaleGroup,
                            Arn::from(arn),
                        );
                    }
                }

                out.resources.push(
                    ctx.resource(
                        Arn::from(arn),
                        tags(group.tags()),
                        Body::ScaleGroup(body::ScaleGroup {
                            name: own(group.auto_scaling_group_name()),
                            min_size: group.min_size(),
                            desired_capacity: group.desired_capacity(),
                            max_size: group.max_size(),
                            observed_instances: group.instances().len(),
                            launch_template_id: own(group
                                .launch_template()
                                .and_then(|lt| lt.launch_template_id())
                                .or_else(|| {
                                    mixed_spec
                                        .and_then(|s| s.launch_template_id())
                                })),
                            launch_configuration_name: own(
                                group.launch_configuration_name()
                            ),
                            launch_template_version: own(group
                                .launch_template()
                                .and_then(|lt| lt.version())
                                .or_else(|| {
                                    mixed_spec.and_then(|s| s.version())
                                })),
                            instance_types: owned(instance_types),
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

/// The launch template version each ASG points at, for its instance type.
/// Runs after the groups so it only describes templates in use.
pub(crate) struct LaunchTemplates;

impl Collect for LaunchTemplates {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeLaunchTemplateVersions";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut out = Collected::default();
        // (template id, version) pairs the groups reference.
        let mut wanted: Vec<(&str, &str)> = prior
            .resources
            .iter()
            .filter_map(|r| {
                let Body::ScaleGroup(g) = &r.body else { return None };
                let id = g.launch_template_id.as_deref()?;
                let version =
                    g.launch_template_version.as_deref().unwrap_or("$Default");
                Some((id, version))
            })
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(out);
        }
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        for (id, version) in wanted {
            let page = ec2
                .describe_launch_template_versions()
                .launch_template_id(id)
                .versions(version)
                .send()
                .await
                .map_err(CollectError::new(Self::API))?;
            for v in page.launch_template_versions() {
                let Some(data) = v.launch_template_data() else { continue };
                let number = v.version_number().unwrap_or_default();
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::LaunchTemplate, &format!("{id}:{number}")),
                    Default::default(),
                    Body::LaunchTemplate(body::LaunchTemplate {
                        launch_template_id: id.to_string(),
                        name: own(v.launch_template_name()),
                        version: number,
                        requested_version: version.to_string(),
                        instance_type: own(
                            data.instance_type().map(|t| t.as_str()),
                        ),
                        image_id: own(data.image_id()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// Launch configurations the groups reference. Older than launch
/// templates and no longer creatable in new accounts, but existing groups
/// keep using them.
pub(crate) struct LaunchConfigurations;

impl Collect for LaunchConfigurations {
    type Context = RegionContext;
    const API: &'static str = "autoscaling:DescribeLaunchConfigurations";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut out = Collected::default();
        let names = prior.distinct(|b| {
            if let Body::ScaleGroup(g) = b {
                g.launch_configuration_name.as_deref()
            } else {
                None
            }
        });
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
            while let Some(page) = next_page(Self::API, &mut pages).await? {
                for lc in page.launch_configurations() {
                    let Some(arn) = lc.launch_configuration_arn() else {
                        continue;
                    };
                    out.resources.push(ctx.resource(
                        Arn::from(arn),
                        Default::default(),
                        Body::LaunchConfiguration(body::LaunchConfiguration {
                            name: own(lc.launch_configuration_name()),
                            instance_type: own(lc.instance_type()),
                            image_id: own(lc.image_id()),
                            created: lc.created_time().map(|t| t.to_string()),
                        }),
                    ));
                }
            }
        }
        Ok(out)
    }
}
