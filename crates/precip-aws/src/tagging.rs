//! Resource Groups Tagging API. One paginated call per region returns the
//! tags of every taggable resource, including the `aws:cloudformation:*`
//! tags that the Lambda and DynamoDB list calls leave out. Runs in the
//! second stage and merges tags into resources collected earlier.

use std::collections::HashSet;

use crate::collect::{
    BoxFuture, CollectError, Collected, Collector, RegionContext, tags,
};

pub struct Tags;

impl Collector for Tags {
    fn needs_prior(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "tag:GetResources"
    }

    fn collect<'a>(
        &'a self,
        ctx: &'a RegionContext,
        prior: &'a Collected,
    ) -> BoxFuture<'a, Result<Collected, CollectError>> {
        Box::pin(async move {
            let known: HashSet<&str> =
                prior.resources.iter().map(|r| r.arn.as_str()).collect();
            let mut out = Collected::default();
            if known.is_empty() {
                return Ok(out);
            }
            let client =
                aws_sdk_resourcegroupstagging::Client::new(&ctx.config);
            let mut pages = client
                .get_resources()
                .resources_per_page(100)
                .into_paginator()
                .send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(CollectError::new(self.name()))?;
                for m in page.resource_tag_mapping_list() {
                    let Some(arn) = m.resource_arn() else { continue };
                    if !known.contains(arn) {
                        continue;
                    }
                    let t =
                        tags(m.tags(), |t| Some(t.key()), |t| Some(t.value()));
                    if !t.is_empty() {
                        out.tag_updates.push((precip_types::Arn::from(arn), t));
                    }
                }
            }
            tracing::info!(
                api = self.name(),
                count = out.tag_updates.len(),
                "collected"
            );
            Ok(out)
        })
    }
}
