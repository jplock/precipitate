//! Managed services with no Oxide equivalent. One list call each. Every
//! resource is stored by ARN with `service` in its body so dependencies
//! can be counted per workload. Nothing here is sized.

use precip_types::scan::ResourceKind;
use precip_types::{Arn, Resource};
use std::collections::BTreeMap;

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, next_page, tags,
};

impl RegionContext {
    /// A managed-service resource: `service` and `name` in its body, plus
    /// the service's own fields from `extra`.
    fn managed(
        &self,
        service: &str,
        arn: Arn,
        name: Option<&str>,
        tags: BTreeMap<String, String>,
        extra: serde_json::Value,
    ) -> Resource {
        let mut body = serde_json::json!({ "service": service, "name": name });
        if let (Some(b), serde_json::Value::Object(e)) =
            (body.as_object_mut(), extra)
        {
            b.extend(e);
        }
        self.resource(arn, ResourceKind::ManagedService, tags, body)
    }
}

pub struct Rds;

impl Collect for Rds {
    const API: &'static str = "rds:DescribeDBInstances";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_rds::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = client.describe_db_instances().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for db in page.db_instances() {
                let Some(arn) = db.db_instance_arn() else { continue };
                out.resources.push(ctx.managed(
                    "rds",
                    Arn::from(arn),
                    db.db_instance_identifier(),
                    tags(db.tag_list()),
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
        Ok(out)
    }
}

pub struct Eks;

impl Collect for Eks {
    const API: &'static str = "eks:ListClusters";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_eks::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = client.list_clusters().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for name in page.clusters() {
                out.resources.push(ctx.managed(
                    "eks",
                    ctx.arn(ArnType::EksCluster, name),
                    Some(name),
                    BTreeMap::new(),
                    serde_json::json!({}),
                ));
            }
        }
        Ok(out)
    }
}

pub struct Ecs;

impl Collect for Ecs {
    const API: &'static str = "ecs:ListClusters";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_ecs::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = client.list_clusters().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for arn in page.cluster_arns() {
                out.resources.push(ctx.managed(
                    "ecs",
                    Arn::from(arn.as_str()),
                    arn.rsplit('/').next(),
                    BTreeMap::new(),
                    serde_json::json!({}),
                ));
            }
        }
        Ok(out)
    }
}

pub struct Lambda;

impl Collect for Lambda {
    const API: &'static str = "lambda:ListFunctions";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_lambda::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = client.list_functions().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for f in page.functions() {
                let Some(arn) = f.function_arn() else { continue };
                out.resources.push(ctx.managed(
                    "lambda",
                    Arn::from(arn),
                    f.function_name(),
                    BTreeMap::new(),
                    serde_json::json!({
                        "runtime": f.runtime().map(|r| r.as_str()),
                        "memory_mib": f.memory_size(),
                        "vpc_id": f.vpc_config().and_then(|v| v.vpc_id()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct ElastiCache;

impl Collect for ElastiCache {
    const API: &'static str = "elasticache:DescribeCacheClusters";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_elasticache::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages =
            client.describe_cache_clusters().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for c in page.cache_clusters() {
                let Some(arn) = c.arn() else { continue };
                out.resources.push(ctx.managed(
                    "elasticache",
                    Arn::from(arn),
                    c.cache_cluster_id(),
                    BTreeMap::new(),
                    serde_json::json!({
                        "engine": c.engine(),
                        "node_type": c.cache_node_type(),
                        "nodes": c.num_cache_nodes(),
                    }),
                ));
            }
        }
        Ok(out)
    }
}

pub struct DynamoDb;

impl Collect for DynamoDb {
    const API: &'static str = "dynamodb:ListTables";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_dynamodb::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = client.list_tables().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for name in page.table_names() {
                out.resources.push(ctx.managed(
                    "dynamodb",
                    ctx.arn(ArnType::DynamoDbTable, name),
                    Some(name),
                    BTreeMap::new(),
                    serde_json::json!({}),
                ));
            }
        }
        Ok(out)
    }
}

pub struct Efs;

impl Collect for Efs {
    const API: &'static str = "elasticfilesystem:DescribeFileSystems";

    async fn collect(
        &self,
        ctx: &RegionContext,
        _prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let client = aws_sdk_efs::Client::new(&ctx.config);
        let mut out = Collected::default();
        let mut pages = client.describe_file_systems().into_paginator().send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for fs in page.file_systems() {
                let Some(arn) = fs.file_system_arn() else { continue };
                out.resources.push(ctx.managed(
                    "efs",
                    Arn::from(arn),
                    fs.name(),
                    tags(fs.tags()),
                    serde_json::json!({
                        "size_bytes": fs.size_in_bytes().map(|s| s.value()),
                    }),
                ));
            }
        }
        Ok(out)
    }
}
