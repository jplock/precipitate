//! Resource Groups Tagging API. One paginated call per region returns the
//! tags of every taggable resource, including the `aws:cloudformation:*`
//! tags that the Lambda and DynamoDB list calls leave out. Runs in the
//! second stage and merges tags into resources collected earlier.

use precip_types::Arn;
use std::collections::HashSet;

use crate::collect::{
    Collect, CollectError, Collected, RegionContext, Stage, next_page, tags,
};

pub struct Tags;

impl Collect for Tags {
    const API: &'static str = "tag:GetResources";
    const STAGE: Stage = Stage::Second;

    async fn collect(
        &self,
        ctx: &RegionContext,
        prior: &Collected,
    ) -> Result<Collected, CollectError> {
        let known: HashSet<&str> =
            prior.resources.iter().map(|r| r.arn.as_str()).collect();
        let mut out = Collected::default();
        if known.is_empty() {
            return Ok(out);
        }
        let client = aws_sdk_resourcegroupstagging::Client::new(&ctx.config);
        let mut pages = client
            .get_resources()
            .resources_per_page(100)
            .into_paginator()
            .send();
        while let Some(page) = next_page(Self::API, &mut pages).await? {
            for m in page.resource_tag_mapping_list() {
                let Some(arn) = m.resource_arn() else { continue };
                if !known.contains(arn) {
                    continue;
                }
                let t = tags(m.tags());
                if !t.is_empty() {
                    out.tag_updates.push((Arn::from(arn), t));
                }
            }
        }
        Ok(out)
    }
}
