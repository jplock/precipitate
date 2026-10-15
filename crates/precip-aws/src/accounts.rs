//! Caller identity, AWS Organization discovery and member-account roles.

use aws_config::SdkConfig;
use aws_config::sts::AssumeRoleProvider;
use aws_credential_types::provider::SharedCredentialsProvider;
use precip_types::{AccountId, Arn, Body, Region, Resource, body};

use crate::{ScanError, arn};

#[derive(Debug, Clone)]
pub(crate) struct CallerIdentity {
    pub account_id: AccountId,
    pub arn: Arn,
    /// The ARN partition the credentials belong to: `aws`, `aws-us-gov`,
    /// `aws-cn`, or one of the isolated partitions. Every ARN the scanner
    /// builds uses this, never a literal `aws`.
    pub partition: String,
}

pub(crate) async fn caller_identity(
    config: &SdkConfig,
) -> Result<CallerIdentity, ScanError> {
    let sts = aws_sdk_sts::Client::new(config);
    let out = sts
        .get_caller_identity()
        .send()
        .await
        .map_err(|e| ScanError::CallerIdentity(Box::new(e)))?;
    let arn = Arn::from(out.arn().ok_or(ScanError::IdentityField("Arn"))?);
    let account_id = AccountId::from(
        out.account().ok_or(ScanError::IdentityField("Account"))?,
    );
    let partition = arn::partition_of(arn.as_str())
        .ok_or_else(|| ScanError::Partition(arn.to_string()))?
        .to_string();
    Ok(CallerIdentity { account_id, arn, partition })
}

/// Whether the caller is the management account of an Organization.
/// `AWSOrganizationsNotInUseException` and `AccessDenied` both mean
/// single-account mode, so any error is treated as "no".
pub(crate) async fn is_management_account(
    config: &SdkConfig,
    account_id: &AccountId,
) -> bool {
    let orgs = aws_sdk_organizations::Client::new(config);
    match orgs.describe_organization().send().await {
        Ok(out) => out
            .organization()
            .and_then(|o| o.master_account_id())
            .is_some_and(|id| account_id == id),
        Err(e) => {
            tracing::debug!(
                error = %crate::error_chain(&e),
                "organizations:DescribeOrganization failed; scanning as a single account"
            );
            false
        }
    }
}

/// One member of the Organization.
#[derive(Debug, Clone)]
pub(crate) struct MemberAccount {
    pub id: AccountId,
    pub arn: Arn,
    pub name: String,
    pub status: String,
    /// Immediate parent: an OU id or the root id.
    pub parent_id: Option<String>,
    pub parent_name: Option<String>,
}

impl MemberAccount {
    pub(crate) fn to_resource(&self, partition: &str) -> Resource {
        Resource {
            arn: self.arn.clone(),
            account_id: self.id.clone(),
            region: Region::global(),
            tags: Default::default(),
            body: Body::Account(body::Account {
                name: self.name.clone(),
                status: self.status.clone(),
                parent_id: self.parent_id.clone(),
                parent_name: self.parent_name.clone(),
                partition: partition.to_string(),
            }),
        }
    }
}

/// Every account in the Organization, with its parent OU name. Suspended
/// and pending accounts are included so the report can say they were
/// skipped; the caller filters on `status`.
pub(crate) async fn list_accounts(
    config: &SdkConfig,
) -> Result<Vec<MemberAccount>, ScanError> {
    let orgs = aws_sdk_organizations::Client::new(config);
    let mut accounts = Vec::new();
    let mut pages = orgs.list_accounts().into_paginator().send();
    while let Some(page) = pages.next().await {
        let page = page.map_err(|e| ScanError::Organization(Box::new(e)))?;
        for a in page.accounts() {
            let (Some(id), Some(arn)) = (a.id(), a.arn()) else { continue };
            accounts.push(MemberAccount {
                id: AccountId::from(id),
                arn: Arn::from(arn),
                name: a.name().unwrap_or_default().to_string(),
                status: a
                    .status()
                    .map(|s| s.as_str())
                    .unwrap_or("UNKNOWN")
                    .to_string(),
                parent_id: None,
                parent_name: None,
            });
        }
    }

    // Parent lookups are one call per account plus one per distinct OU.
    // Organizations throttles hard, so keep this modest.
    let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut tasks = tokio::task::JoinSet::new();
    for (i, a) in accounts.iter().enumerate() {
        let orgs = orgs.clone();
        let id = a.id.clone();
        let limit = limit.clone();
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await;
            let parent = match orgs
                .list_parents()
                .child_id(id.as_str())
                .send()
                .await
            {
                Ok(p) => p
                    .parents()
                    .first()
                    .and_then(|x| x.id().map(str::to_string)),
                Err(e) => {
                    tracing::warn!(
                        account = %id,
                        error = %crate::error_chain(&e),
                        "organizations:ListParents failed; parent left blank"
                    );
                    None
                }
            };
            let parent_name = match &parent {
                Some(pid) if pid.starts_with("ou-") => {
                    match orgs
                        .describe_organizational_unit()
                        .organizational_unit_id(pid)
                        .send()
                        .await
                    {
                        Ok(o) => o
                            .organizational_unit()
                            .and_then(|u| u.name().map(str::to_string)),
                        Err(e) => {
                            tracing::warn!(
                                ou = %pid,
                                error = %crate::error_chain(&e),
                                "organizations:DescribeOrganizationalUnit failed; OU name left blank"
                            );
                            None
                        }
                    }
                }
                Some(_) => Some("root".to_string()),
                None => None,
            };
            (i, parent, parent_name)
        });
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((i, parent, parent_name)) => {
                accounts[i].parent_id = parent;
                accounts[i].parent_name = parent_name;
            }
            Err(e) => {
                tracing::warn!(error = %e, "account parent lookup panicked");
            }
        }
    }
    accounts.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(accounts)
}

/// SDK config whose credentials come from assuming `role_name` in
/// `account_id`, chained from the management account's credentials.
pub(crate) async fn assume_member_role(
    base: &SdkConfig,
    partition: &str,
    account_id: &AccountId,
    role_name: &str,
) -> SdkConfig {
    let role_arn =
        format!("arn:{partition}:iam::{account_id}:role/{role_name}");
    let provider = AssumeRoleProvider::builder(role_arn)
        .session_name("precip-scan")
        .configure(base)
        .build()
        .await;
    base.to_builder()
        .credentials_provider(SharedCredentialsProvider::new(provider))
        .build()
}
