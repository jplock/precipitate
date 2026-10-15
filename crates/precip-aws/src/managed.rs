//! Managed services with no Oxide equivalent. One list call each. Every
//! resource is stored by ARN with `service` in its body so dependencies
//! can be counted per workload. Nothing here is sized.

use precip_types::scan::ResourceKind;

use crate::collect::{
    BoxFuture, CollectError, Collected, Collector, RegionContext, tags,
};

fn managed(
    ctx: &RegionContext,
    service: &str,
    arn: precip_types::Arn,
    name: Option<&str>,
    tags: std::collections::BTreeMap<String, String>,
    extra: serde_json::Value,
) -> precip_types::Resource {
    let mut body = serde_json::json!({ "service": service, "name": name });
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    ctx.resource(arn, ResourceKind::ManagedService, tags, body)
}

macro_rules! log_count {
    ($ctx:expr, $name:expr, $out:expr) => {
        tracing::info!(api = $name, count = $out.resources.len(), "collected");
    };
}

pub struct Rds;

impl Collector for Rds {
    fn name(&self) -> &'static str {
        "rds:DescribeDBInstances"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_rds::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages =
                client.describe_db_instances().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for db in page.db_instances() {
                    let Some(arn) = db.db_instance_arn() else { continue };
                    out.resources.push(managed(
                        ctx,
                        "rds",
                        precip_types::Arn::from(arn),
                        db.db_instance_identifier(),
                        tags(db.tag_list(), |t| t.key(), |t| t.value()),
                        serde_json::json!({
                            "engine": db.engine(),
                            "engine_version": db.engine_version(),
                            "instance_class": db.db_instance_class(),
                            "allocated_storage_gib": db.allocated_storage(),
                            "multi_az": db.multi_az(),
                            "vpc_id": db.db_subnet_group().and_then(|g| g.vpc_id()),
                        }),
                    ));
                }
            }
            log_count!(ctx, self.name(), out);
            Ok(out)
        })
    }
}

pub struct Eks;

impl Collector for Eks {
    fn name(&self) -> &'static str {
        "eks:ListClusters"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_eks::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages = client.list_clusters().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for name in page.clusters() {
                    out.resources.push(managed(
                        ctx,
                        "eks",
                        ctx.arn("eks", &format!("cluster/{name}")),
                        Some(name),
                        Default::default(),
                        serde_json::json!({}),
                    ));
                }
            }
            log_count!(ctx, self.name(), out);
            Ok(out)
        })
    }
}

pub struct Ecs;

impl Collector for Ecs {
    fn name(&self) -> &'static str {
        "ecs:ListClusters"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_ecs::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages = client.list_clusters().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for arn in page.cluster_arns() {
                    let name = arn.rsplit('/').next();
                    out.resources.push(managed(
                        ctx,
                        "ecs",
                        precip_types::Arn::from(arn.clone()),
                        name,
                        Default::default(),
                        serde_json::json!({}),
                    ));
                }
            }
            log_count!(ctx, self.name(), out);
            Ok(out)
        })
    }
}

pub struct Lambda;

impl Collector for Lambda {
    fn name(&self) -> &'static str {
        "lambda:ListFunctions"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_lambda::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages = client.list_functions().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for f in page.functions() {
                    let Some(arn) = f.function_arn() else { continue };
                    out.resources.push(managed(
                        ctx,
                        "lambda",
                        precip_types::Arn::from(arn),
                        f.function_name(),
                        Default::default(),
                        serde_json::json!({
                            "runtime": f.runtime().map(|r| r.as_str()),
                            "memory_mib": f.memory_size(),
                            "vpc_id": f.vpc_config().and_then(|v| v.vpc_id()),
                        }),
                    ));
                }
            }
            log_count!(ctx, self.name(), out);
            Ok(out)
        })
    }
}

pub struct ElastiCache;

impl Collector for ElastiCache {
    fn name(&self) -> &'static str {
        "elasticache:DescribeCacheClusters"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_elasticache::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages =
                client.describe_cache_clusters().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for c in page.cache_clusters() {
                    let Some(arn) = c.arn() else { continue };
                    out.resources.push(managed(
                        ctx,
                        "elasticache",
                        precip_types::Arn::from(arn),
                        c.cache_cluster_id(),
                        Default::default(),
                        serde_json::json!({
                            "engine": c.engine(),
                            "node_type": c.cache_node_type(),
                            "nodes": c.num_cache_nodes(),
                        }),
                    ));
                }
            }
            log_count!(ctx, self.name(), out);
            Ok(out)
        })
    }
}

pub struct DynamoDb;

impl Collector for DynamoDb {
    fn name(&self) -> &'static str {
        "dynamodb:ListTables"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_dynamodb::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages = client.list_tables().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for name in page.table_names() {
                    out.resources.push(managed(
                        ctx,
                        "dynamodb",
                        ctx.arn("dynamodb", &format!("table/{name}")),
                        Some(name),
                        Default::default(),
                        serde_json::json!({}),
                    ));
                }
            }
            log_count!(ctx, self.name(), out);
            Ok(out)
        })
    }
}

pub struct Efs;

impl Collector for Efs {
    fn name(&self) -> &'static str {
        "elasticfilesystem:DescribeFileSystems"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        _prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let client = aws_sdk_efs::Client::new(&ctx.config);
            let mut out = Collected::default();
            let mut pages =
                client.describe_file_systems().into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for fs in page.file_systems() {
                    let Some(arn) = fs.file_system_arn() else { continue };
                    out.resources.push(managed(
                        ctx,
                        "efs",
                        precip_types::Arn::from(arn),
                        fs.name(),
                        tags(fs.tags(), |t| Some(t.key()), |t| Some(t.value())),
                        serde_json::json!({
                            "size_bytes": fs.size_in_bytes().map(|s| s.value()),
                        }),
                    ));
                }
            }
            log_count!(ctx, self.name(), out);
            Ok(out)
        })
    }
}
