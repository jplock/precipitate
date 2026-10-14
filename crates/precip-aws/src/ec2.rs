//! EC2 instances, the instance types they use, their volumes and images.

use aws_config::SdkConfig;
use aws_sdk_ec2::types::InstanceStateName;
use precip_types::scan::{Edge, ResourceKind};

use crate::ScanError;
use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, next_page, tags,
};

pub async fn enabled_regions(
    config: &SdkConfig,
) -> Result<Vec<precip_types::Region>, ScanError> {
    let ec2 = aws_sdk_ec2::Client::new(config);
    let out = ec2
        .describe_regions()
        .all_regions(false)
        .send()
        .await
        .map_err(|e| ScanError::Regions(Box::new(e)))?;
    Ok(out
        .regions()
        .iter()
        .filter_map(|r| r.region_name().map(precip_types::Region::from))
        .collect())
}

pub struct Instances;

impl Collect for Instances {
    const API: &'static str = "ec2:DescribeInstances";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = ec2.describe_instances().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for reservation in page.reservations() {
                for inst in reservation.instances() {
                    let Some(id) = inst.instance_id() else { continue };
                    let state = inst.state().and_then(|s| s.name());
                    // Terminated instances stay visible for about an hour
                    // and are not part of the estate.
                    if state == Some(&InstanceStateName::Terminated) {
                        continue;
                    }
                    let arn = ctx.arn(ArnType::Instance, id);
                    for bdm in inst.block_device_mappings() {
                        if let Some(vol) = bdm.ebs().and_then(|e| e.volume_id())
                        {
                            out.relate(
                                ctx.arn(ArnType::Volume, vol),
                                Edge::AttachedTo,
                                arn.clone(),
                            );
                        }
                    }
                    for sg in inst.security_groups() {
                        if let Some(gid) = sg.group_id() {
                            out.relate(
                                arn.clone(),
                                Edge::HasSecurityGroup,
                                ctx.arn(ArnType::SecurityGroup, gid),
                            );
                        }
                    }
                    if let Some(subnet) = inst.subnet_id() {
                        out.relate(
                            arn.clone(),
                            Edge::InSubnet,
                            ctx.arn(ArnType::Subnet, subnet),
                        );
                    }
                    let instance_store_volumes = inst
                        .block_device_mappings()
                        .iter()
                        .filter(|b| b.ebs().is_none())
                        .count();
                    out.resources.push(ctx.resource(
                        arn,
                        ResourceKind::Ec2Instance,
                        tags(inst.tags()),
                        serde_json::json!({
                            "instance_id": id,
                            "instance_type": inst.instance_type().map(|t| t.as_str()),
                            "architecture": inst.architecture().map(|a| a.as_str()),
                            "platform": inst.platform_details(),
                            "state": state.map(|n| n.as_str()),
                            "image_id": inst.image_id(),
                            "subnet_id": inst.subnet_id(),
                            "vpc_id": inst.vpc_id(),
                            "boot_mode": inst.boot_mode().map(|b| b.as_str()),
                            "current_boot_mode": inst.current_instance_boot_mode().map(|b| b.as_str()),
                            "launch_time": inst.launch_time().map(|t| t.to_string()),
                            "ebs_volume_count": inst.block_device_mappings().len() - instance_store_volumes,
                            "instance_store_volume_count": instance_store_volumes,
                            "network_interface_count": inst.network_interfaces().len(),
                            "public_ip": inst.public_ip_address(),
                        }),
                    ));
                }
            }
        }
        Ok(out)
    }
}

/// vCPU and memory for every instance type referenced by an instance or
/// an Auto Scaling group in the region. Sizing reads these rows.
pub struct InstanceTypes;

impl Collect for InstanceTypes {
    const API: &'static str = "ec2:DescribeInstanceTypes";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut names = Vec::new();
        for kind in [
            ResourceKind::Ec2Instance,
            ResourceKind::LaunchTemplate,
            ResourceKind::LaunchConfiguration,
        ] {
            names.extend(prior.distinct_field(kind, "instance_type"));
        }
        for g in prior.of_kind(ResourceKind::AutoScalingGroup) {
            if let Some(types) =
                g.body.get("instance_types").and_then(|v| v.as_array())
            {
                names.extend(
                    types.iter().filter_map(|t| t.as_str().map(str::to_string)),
                );
            }
        }
        names.sort();
        names.dedup();
        let mut out = Collected::default();
        if names.is_empty() {
            return Ok(out);
        }
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        // The API accepts at most 100 types per call.
        for chunk in names.chunks(100) {
            let types: Vec<aws_sdk_ec2::types::InstanceType> = chunk
                .iter()
                .map(|n| aws_sdk_ec2::types::InstanceType::from(n.as_str()))
                .collect();
            let page = ec2
                .describe_instance_types()
                .set_instance_types(Some(types))
                .send()
                .await
                .map_err(CollectError::new(Self::API))?;
            for it in page.instance_types() {
                let Some(name) = it.instance_type().map(|t| t.as_str()) else {
                    continue;
                };
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::InstanceType, name),
                    ResourceKind::InstanceType,
                    Default::default(),
                    serde_json::json!({
                        "name": name,
                        "vcpus": it.v_cpu_info().and_then(|v| v.default_v_cpus()),
                        "memory_mib": it.memory_info().and_then(|m| m.size_in_mib()),
                        "architectures": it.processor_info().map(|p| {
                            p.supported_architectures().iter().map(|a| a.as_str()).collect::<Vec<_>>()
                        }),
                        "bare_metal": it.bare_metal(),
                        "gpus": it.gpu_info().map(|g| g.gpus().len()).unwrap_or(0),
                        "accelerators": it.inference_accelerator_info().map(|g| g.accelerators().len()).unwrap_or(0),
                        "instance_storage_gb": it.instance_storage_info().and_then(|s| s.total_size_in_gb()),
                        "network_performance": it.network_info().and_then(|n| n.network_performance()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct Volumes;

impl Collect for Volumes {
    const API: &'static str = "ec2:DescribeVolumes";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = ec2.describe_volumes().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for vol in page.volumes() {
                let Some(id) = vol.volume_id() else { continue };
                let attached_to: Vec<&str> = vol
                    .attachments()
                    .iter()
                    .filter_map(|a| a.instance_id())
                    .collect();
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::Volume, id),
                    ResourceKind::EbsVolume,
                    tags(vol.tags()),
                    serde_json::json!({
                        "volume_id": id,
                        "size_gib": vol.size(),
                        "volume_type": vol.volume_type().map(|t| t.as_str()),
                        "iops": vol.iops(),
                        "throughput": vol.throughput(),
                        "encrypted": vol.encrypted(),
                        "kms_key_id": vol.kms_key_id(),
                        "state": vol.state().map(|s| s.as_str()),
                        "attached_to": attached_to,
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// Images referenced by instances in the region, whoever owns them.
/// Platform, boot mode, Marketplace and encryption decide the export path.
pub struct Images;

impl Collect for Images {
    const API: &'static str = "ec2:DescribeImages";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let mut ids = Vec::new();
        for kind in [
            ResourceKind::Ec2Instance,
            ResourceKind::LaunchTemplate,
            ResourceKind::LaunchConfiguration,
        ] {
            ids.extend(prior.distinct_field(kind, "image_id"));
        }
        ids.sort();
        ids.dedup();
        let mut out = Collected::default();
        if ids.is_empty() {
            return Ok(out);
        }
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        for chunk in ids.chunks(100) {
            let page = ec2
                .describe_images()
                .set_image_ids(Some(chunk.to_vec()))
                .include_deprecated(true)
                .send()
                .await
                .map_err(CollectError::new(Self::API))?;
            for img in page.images() {
                let Some(id) = img.image_id() else { continue };
                let encrypted_snapshot = img
                    .block_device_mappings()
                    .iter()
                    .any(|b| b.ebs().and_then(|e| e.encrypted()) == Some(true));
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::Image, id),
                    ResourceKind::Ami,
                    tags(img.tags()),
                    serde_json::json!({
                        "image_id": id,
                        "name": img.name(),
                        "owner_id": img.owner_id(),
                        "owner_alias": img.image_owner_alias(),
                        "platform": img.platform_details(),
                        "architecture": img.architecture().map(|a| a.as_str()),
                        "boot_mode": img.boot_mode().map(|b| b.as_str()),
                        "marketplace_product_codes": img.product_codes().len(),
                        "encrypted_snapshot": encrypted_snapshot,
                        "public": img.public(),
                        "state": img.state().map(|s| s.as_str()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// Elastic IPs. Each maps to an Oxide floating IP.
pub struct Addresses;

impl Collect for Addresses {
    const API: &'static str = "ec2:DescribeAddresses";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ec2 = aws_sdk_ec2::Client::new(&ctx.config);
        let mut out = Collected::default();
        let page = ec2
            .describe_addresses()
            .send()
            .await
            .map_err(CollectError::new(Self::API))?;
        for a in page.addresses() {
            let Some(id) = a.allocation_id() else { continue };
            let arn = ctx.arn(ArnType::ElasticIp, id);
            if let Some(instance) = a.instance_id() {
                out.relate(
                    arn.clone(),
                    Edge::AttachedTo,
                    ctx.arn(ArnType::Instance, instance),
                );
            }
            out.resources.push(ctx.resource(
                arn,
                ResourceKind::ElasticIp,
                tags(a.tags()),
                serde_json::json!({
                    "allocation_id": id,
                    "public_ip": a.public_ip(),
                    "instance_id": a.instance_id(),
                    "network_interface_id": a.network_interface_id(),
                    "domain": a.domain().map(|d| d.as_str()),
                }),
            ));
        }
        Ok(out)
    }
}
