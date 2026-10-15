//! Managed services with no Oxide equivalent. One list call each. Every
//! resource is stored by ARN with `service` in its body so dependencies
//! can be counted per workload. Nothing here is sized.

use precip_types::{Arn, Body, Resource, body};
use std::collections::BTreeMap;

use crate::arn::ArnType;
use crate::collect::{
    Collect, CollectError, Collected, RegionContext, next_page, own, tags,
};

impl RegionContext {
    /// A managed-service resource.
    fn managed(
        &self,
        arn: Arn,
        name: Option<&str>,
        tags: BTreeMap<String, String>,
        service: body::Service,
    ) -> Resource {
        self.resource(
            arn,
            tags,
            Body::ManagedService(body::Managed { name: own(name), service }),
        )
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
                    Arn::from(arn),
                    db.db_instance_identifier(),
                    tags(db.tag_list()),
                    body::Service::Rds {
                        engine: own(db.engine()),
                        engine_version: own(db.engine_version()),
                        instance_class: own(db.db_instance_class()),
                        allocated_storage_gib: db.allocated_storage(),
                        multi_az: db.multi_az(),
                        vpc_id: own(
                            db.db_subnet_group().and_then(|g| g.vpc_id()),
                        ),
                    },
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
                    ctx.arn(ArnType::EksCluster, name),
                    Some(name),
                    BTreeMap::new(),
                    body::Service::Eks,
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
                    Arn::from(arn.as_str()),
                    arn.rsplit('/').next(),
                    BTreeMap::new(),
                    body::Service::Ecs,
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
                    Arn::from(arn),
                    f.function_name(),
                    BTreeMap::new(),
                    body::Service::Lambda {
                        runtime: own(f.runtime().map(|r| r.as_str())),
                        memory_mib: f.memory_size(),
                        vpc_id: own(f.vpc_config().and_then(|v| v.vpc_id())),
                    },
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
                    Arn::from(arn),
                    c.cache_cluster_id(),
                    BTreeMap::new(),
                    body::Service::ElastiCache {
                        engine: own(c.engine()),
                        node_type: own(c.cache_node_type()),
                        nodes: c.num_cache_nodes(),
                    },
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
                    ctx.arn(ArnType::DynamoDbTable, name),
                    Some(name),
                    BTreeMap::new(),
                    body::Service::DynamoDb,
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
                    Arn::from(arn),
                    fs.name(),
                    tags(fs.tags()),
                    body::Service::Efs {
                        size_bytes: fs.size_in_bytes().map(|s| s.value()),
                    },
                ));
            }
        }
        Ok(out)
    }
}
