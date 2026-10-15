//! EC2 instances, the instance types they use, their volumes and images.

use aws_config::SdkConfig;
use aws_sdk_ec2::types::InstanceStateName;
use precip_types::scan::Edge;
use precip_types::{Body, body};

use crate::ScanError;
use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, next_page, own,
    owned, tags,
};

pub(crate) async fn enabled_regions(
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

pub(crate) struct Instances;

impl Collect for Instances {
    type Context = RegionContext;
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
                    let mappings = inst.block_device_mappings();
                    let local_disk_count =
                        mappings.iter().filter(|b| b.ebs().is_none()).count();
                    out.resources.push(
                        ctx.resource(
                            arn,
                            tags(inst.tags()),
                            Body::Instance(body::Instance {
                                instance_id: id.to_string(),
                                instance_type: own(inst
                                    .instance_type()
                                    .map(|t| t.as_str())),
                                architecture: own(inst
                                    .architecture()
                                    .map(|a| a.as_str())),
                                platform: own(inst.platform_details()),
                                state: own(state.map(|n| n.as_str())),
                                image_id: own(inst.image_id()),
                                subnet_id: own(inst.subnet_id()),
                                network_id: own(inst.vpc_id()),
                                // The mode the instance booted in, else the
                                // one its image asks for.
                                boot_mode: own(inst
                                    .current_instance_boot_mode()
                                    .map(|b| b.as_str())
                                    .or(inst.boot_mode().map(|b| b.as_str()))),
                                launch_time: inst
                                    .launch_time()
                                    .map(|t| t.to_string()),
                                volume_count: mappings.len() - local_disk_count,
                                local_disk_count,
                                network_interface_count: inst
                                    .network_interfaces()
                                    .len(),
                                public_ip: own(inst.public_ip_address()),
                            }),
                        ),
                    );
                }
            }
        }
        Ok(out)
    }
}

/// vCPU and memory for every instance type referenced by an instance or
/// an Auto Scaling group in the region. Sizing reads these rows.
pub(crate) struct InstanceTypes;

impl Collect for InstanceTypes {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeInstanceTypes";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let names = prior.distinct(|b| {
            let mut types: Vec<&str> = Vec::new();
            if let Some((Some(ty), _)) = b.launch_source() {
                types.push(ty);
            }
            if let Body::ScaleGroup(g) = b {
                types.extend(g.instance_types.iter().map(String::as_str));
            }
            types
        });
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
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::InstanceType, name),
                        Default::default(),
                        Body::InstanceType(body::InstanceType {
                            name: name.to_string(),
                            vcpus: it
                                .v_cpu_info()
                                .and_then(|v| v.default_v_cpus()),
                            memory_mib: it
                                .memory_info()
                                .and_then(|m| m.size_in_mib()),
                            architectures: it.processor_info().map(|p| {
                                owned(
                                    p.supported_architectures()
                                        .iter()
                                        .map(|a| a.as_str()),
                                )
                            }),
                            bare_metal: it.bare_metal(),
                            gpus: it.gpu_info().map_or(0, |g| g.gpus().len()),
                            accelerators: it
                                .inference_accelerator_info()
                                .map_or(0, |g| g.accelerators().len()),
                            local_storage_gb: it
                                .instance_storage_info()
                                .and_then(|s| s.total_size_in_gb()),
                            network_performance: own(it
                                .network_info()
                                .and_then(|n| n.network_performance())),
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

pub(crate) struct Volumes;

impl Collect for Volumes {
    type Context = RegionContext;
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
                out.resources.push(
                    ctx.resource(
                        ctx.arn(ArnType::Volume, id),
                        tags(vol.tags()),
                        Body::Volume(body::Volume {
                            volume_id: id.to_string(),
                            size_gib: vol.size(),
                            volume_type: own(vol
                                .volume_type()
                                .map(|t| t.as_str())),
                            iops: vol.iops(),
                            throughput: vol.throughput(),
                            encrypted: vol.encrypted(),
                            encryption_key_id: own(vol.kms_key_id()),
                            state: own(vol.state().map(|s| s.as_str())),
                            attached_to: owned(
                                vol.attachments()
                                    .iter()
                                    .filter_map(|a| a.instance_id()),
                            ),
                        }),
                    ),
                );
            }
        }
        Ok(out)
    }
}

/// Images referenced by instances in the region, whoever owns them.
/// Platform, boot mode, Marketplace and encryption decide the export path.
pub(crate) struct Images;

impl Collect for Images {
    type Context = RegionContext;
    const API: &'static str = "ec2:DescribeImages";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let ids =
            prior.distinct(|b| b.launch_source().and_then(|(_, image)| image));
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
                out.resources.push(ctx.resource(
                    ctx.arn(ArnType::Image, id),
                    tags(img.tags()),
                    Body::Image(body::Image {
                        image_id: id.to_string(),
                        name: own(img.name()),
                        owner_id: own(img.owner_id()),
                        owner_alias: own(img.image_owner_alias()),
                        platform: own(img.platform_details()),
                        architecture: own(
                            img.architecture().map(|a| a.as_str()),
                        ),
                        boot_mode: own(img.boot_mode().map(|b| b.as_str())),
                        marketplace_product_codes: img.product_codes().len(),
                        encrypted_snapshot:
                            img.block_device_mappings().iter().any(|b| {
                                b.ebs().and_then(|e| e.encrypted())
                                    == Some(true)
                            }),
                        public: img.public(),
                        state: own(img.state().map(|s| s.as_str())),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

/// Elastic IPs. Each maps to an Oxide floating IP.
pub(crate) struct Addresses;

impl Collect for Addresses {
    type Context = RegionContext;
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
                tags(a.tags()),
                Body::PublicIp(body::PublicIp {
                    allocation_id: id.to_string(),
                    public_ip: own(a.public_ip()),
                    instance_id: own(a.instance_id()),
                    network_interface_id: own(a.network_interface_id()),
                    domain: own(a.domain().map(|d| d.as_str())),
                }),
            ));
        }
        Ok(out)
    }
}
