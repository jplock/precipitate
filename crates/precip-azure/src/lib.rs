//! Read-only Azure discovery. precip lists the subscriptions the caller
//! can read, then runs Resource Graph queries over them in batches of up
//! to 1,000. Resource Graph covers every region at once, so a subscription
//! is one scan target.

mod arm;
mod collect;
mod compute;
mod groups;
mod id;
mod managed;
mod network;
mod skus;
mod subscriptions;

use arm::Arm;
pub use arm::ArmError;
use azure_core::base64;
use azure_core::cloud::CloudConfiguration;
use azure_core::credentials::{Secret, TokenCredential};
use azure_core::http::ClientOptions;
use azure_identity::{
    AzureCliCredential, AzureCliCredentialOptions,
    ClientAssertionCredentialOptions, ClientSecretCredential,
    ClientSecretCredentialOptions, DeveloperToolsCredential,
    WorkloadIdentityCredential, WorkloadIdentityCredentialOptions,
};
use precip_collect::{ProgressSender, emit, finish_batch, selected};
use precip_types::scan::TargetStatus;
use precip_types::{AccountId, Provider, Scan, ScanEvent};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use subscriptions::{Parent, Subscription};

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("loading Azure credentials")]
    Credential(#[source] azure_core::Error),
    #[error("getting an Azure token")]
    Token(#[source] azure_core::Error),
    #[error("listing subscriptions")]
    Subscriptions(#[source] ArmError),
    #[error(
        "no readable subscriptions match the selection; the principal needs Reader on them"
    )]
    NoSubscriptions,
}

/// The Azure cloud a scan signs in to and reads. Named as `az cloud list`
/// names them.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize,
)]
pub enum AzureCloud {
    #[default]
    #[serde(rename = "AzureCloud")]
    Public,
    #[serde(rename = "AzureUSGovernment")]
    UsGovernment,
}

impl AzureCloud {
    pub fn name(self) -> &'static str {
        match self {
            Self::Public => "AzureCloud",
            Self::UsGovernment => "AzureUSGovernment",
        }
    }

    /// The cloud's Resource Manager endpoint.
    pub(crate) fn endpoint(self) -> &'static str {
        match self {
            Self::Public => "https://management.azure.com",
            Self::UsGovernment => "https://management.usgovcloudapi.net",
        }
    }

    /// The sign-in settings for service principal and workload identity
    /// credentials. The Azure CLI uses the cloud `az cloud set` chose.
    fn client_options(self) -> ClientOptions {
        ClientOptions {
            cloud: Some(Arc::new(match self {
                Self::Public => CloudConfiguration::AzurePublic,
                Self::UsGovernment => CloudConfiguration::AzureGovernment,
            })),
            ..ClientOptions::default()
        }
    }
}

/// Which subscriptions to scan. Saved with each scan so a later scan with
/// no flags repeats the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub cloud: AzureCloud,
    /// Only subscriptions in this tenant, and the tenant the Azure CLI
    /// signs in to.
    pub tenant: Option<String>,
    /// Only these subscriptions. Empty means every readable one.
    pub subscriptions: Vec<AccountId>,
    pub exclude_subscriptions: Vec<AccountId>,
    /// Only subscriptions under this management group, at any depth.
    pub management_group: Option<String>,
}

/// The credential a scan signs in with.
#[derive(Debug, Clone)]
pub struct Credential(Arc<dyn TokenCredential>);

const AZURE_CLIENT_ID: &str = "AZURE_CLIENT_ID";
const AZURE_CLIENT_SECRET: &str = "AZURE_CLIENT_SECRET";
const AZURE_TENANT_ID: &str = "AZURE_TENANT_ID";
const AZURE_FEDERATED_TOKEN_FILE: &str = "AZURE_FEDERATED_TOKEN_FILE";

/// The credential the environment provides, in this order: a service
/// principal secret (`AZURE_CLIENT_SECRET` with `AZURE_CLIENT_ID` and a
/// tenant), workload identity (`AZURE_FEDERATED_TOKEN_FILE`), else the
/// Azure CLI or Azure Developer CLI sign-in.
pub fn load_credential(
    tenant: Option<&str>,
    cloud: AzureCloud,
) -> Result<Credential, ScanError> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let tenant_id = tenant.map(str::to_string).or_else(|| var(AZURE_TENANT_ID));
    let credential: Arc<dyn TokenCredential> =
        match (var(AZURE_CLIENT_SECRET), var(AZURE_CLIENT_ID), &tenant_id) {
            (Some(secret), Some(client), Some(tenant)) => {
                ClientSecretCredential::new(
                    tenant,
                    client,
                    Secret::new(secret),
                    Some(ClientSecretCredentialOptions {
                        client_options: cloud.client_options(),
                    }),
                )
                .map_err(ScanError::Credential)?
            }
            _ if var(AZURE_FEDERATED_TOKEN_FILE).is_some() => {
                WorkloadIdentityCredential::new(Some(
                    WorkloadIdentityCredentialOptions {
                        tenant_id: tenant_id.clone(),
                        credential_options: ClientAssertionCredentialOptions {
                            client_options: cloud.client_options(),
                        },
                        ..Default::default()
                    },
                ))
                .map_err(ScanError::Credential)?
            }
            _ => match tenant {
                Some(t) => {
                    AzureCliCredential::new(Some(AzureCliCredentialOptions {
                        tenant_id: Some(t.to_string()),
                        ..Default::default()
                    }))
                    .map_err(ScanError::Credential)?
                }
                None => DeveloperToolsCredential::new(None)
                    .map_err(ScanError::Credential)?,
            },
        };
    Ok(Credential(credential))
}

/// Scan every selected subscription.
pub async fn scan_estate(
    credential: &Credential,
    opts: &Options,
    progress: Option<ProgressSender>,
) -> Result<Scan, ScanError> {
    let arm = Arm::new(credential.0.clone(), opts.cloud);
    scan_with(&arm, credential, opts, progress).await
}

async fn scan_with(
    arm: &Arm,
    credential: &Credential,
    opts: &Options,
    progress: Option<ProgressSender>,
) -> Result<Scan, ScanError> {
    let token = credential
        .0
        .get_token(&[arm.scope()], None)
        .await
        .map_err(ScanError::Token)?;
    let mut scan = Scan {
        id: uuid::Uuid::now_v7().to_string(),
        provider: Provider::Azure,
        started: jiff::Timestamp::now(),
        finished: None,
        caller: caller(token.token.secret()),
        targets: Vec::new(),
        resources: Vec::new(),
        relationships: Vec::new(),
    };

    let all =
        subscriptions::list(arm).await.map_err(ScanError::Subscriptions)?;
    let ids: Vec<String> = all.iter().map(|s| s.id().to_string()).collect();
    // Without management group access the chain is empty; the scan goes
    // on with flat subscriptions.
    let parents = match subscriptions::parents(arm, &ids).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %precip_collect::error_chain(&e), "reading management groups");
            HashMap::new()
        }
    };
    let selected = select(all, &parents, opts);
    if selected.is_empty() {
        return Err(ScanError::NoSubscriptions);
    }
    emit(&progress, ScanEvent::AccountsDiscovered { total: selected.len() });
    scan.resources.extend(selected.iter().map(|s| s.to_resource(opts.cloud)));

    for batch in selected.chunks(arm::GRAPH_MAX_SUBSCRIPTIONS) {
        for s in batch {
            emit(
                &progress,
                ScanEvent::AccountStarted {
                    account_id: s.id(),
                    name: Some(s.display_name.clone()),
                },
            );
        }
        let ctx = Arc::new(collect::Batch {
            arm: arm.clone(),
            subscriptions: batch.iter().map(|s| s.id().to_string()).collect(),
        });
        let (got, status) = precip_collect::run(collect::all(), ctx).await;
        let accounts: Vec<AccountId> =
            batch.iter().map(Subscription::id).collect();
        scan.targets.extend(finish_batch(&progress, &accounts, &got, &status));
        scan.resources.extend(got.resources);
        scan.relationships.extend(got.relationships);
    }
    scan.finished = Some(jiff::Timestamp::now());
    let failed = scan
        .targets
        .iter()
        .filter(|t| matches!(t.status, TargetStatus::Failed(_)))
        .count();
    tracing::info!(
        subscriptions = scan.targets.len(),
        failed,
        resources = scan.resources.len(),
        "Azure scan finished"
    );
    Ok(scan)
}

/// The subscriptions `opts` selects, readable ones only, each with its
/// management group.
fn select(
    all: Vec<Subscription>,
    parents: &HashMap<String, Parent>,
    opts: &Options,
) -> Vec<Subscription> {
    let mut out = Vec::new();
    for mut s in all {
        let id = s.id().to_string();
        s.parent = parents.get(&id).cloned();
        let keep = s.is_readable()
            && selected(
                &[id.as_str(), s.display_name.as_str()],
                &opts.subscriptions,
                &opts.exclude_subscriptions,
            )
            && opts
                .tenant
                .as_deref()
                .is_none_or(|t| s.tenant_id.eq_ignore_ascii_case(t))
            && opts.management_group.as_deref().is_none_or(|mg| {
                s.parent.as_ref().is_some_and(|p| {
                    p.ancestors.iter().any(|a| a.eq_ignore_ascii_case(mg))
                })
            });
        if keep {
            out.push(s);
        }
    }
    out.sort_by_key(Subscription::id);
    out
}

/// Who the token was issued to: the user's sign-in name, else the
/// application id, else the object id. The token's signature is not
/// checked; this is a label for the scan, not an authorisation.
fn caller(token: &str) -> String {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Claims {
        upn: Option<String>,
        unique_name: Option<String>,
        appid: Option<String>,
        oid: Option<String>,
    }
    let claims: Claims = token
        .split('.')
        .nth(1)
        .and_then(|p| base64::decode_url_safe(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    claims
        .upn
        .or(claims.unique_name)
        .or(claims.appid)
        .or(claims.oid)
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arm::fake::{Reply, Script, Token};

    fn sub(id: &str, tenant: &str, state: &str) -> Subscription {
        Subscription {
            subscription_id: id.into(),
            display_name: format!("{id}-name"),
            state: state.into(),
            tenant_id: tenant.into(),
            parent: None,
        }
    }

    #[test]
    fn selection_filters_by_list_tenant_state_and_management_group() {
        let all = || {
            vec![
                sub("B", "t1", "Enabled"),
                sub("a", "t1", "Warned"),
                sub("c", "t2", "Enabled"),
                sub("d", "t1", "Disabled"),
            ]
        };
        let parents = HashMap::from([(
            "a".to_string(),
            Parent {
                id: "prod".into(),
                name: "Prod".into(),
                ancestors: vec!["prod".into(), "root".into()],
            },
        )]);
        let ids = |v: Vec<Subscription>| -> Vec<String> {
            v.iter().map(|s| s.id().to_string()).collect()
        };
        assert_eq!(
            ids(select(all(), &parents, &Options::default())),
            ["a", "b", "c"]
        );
        let tenant =
            Options { tenant: Some("T1".into()), ..Default::default() };
        assert_eq!(ids(select(all(), &parents, &tenant)), ["a", "b"]);
        let only = Options {
            subscriptions: vec!["b".into(), "d".into()],
            ..Default::default()
        };
        assert_eq!(
            ids(select(all(), &parents, &only)),
            ["b"],
            "disabled stays out"
        );
        let by_name = Options {
            subscriptions: vec!["c-NAME".into()],
            ..Default::default()
        };
        assert_eq!(ids(select(all(), &parents, &by_name)), ["c"]);
        let except = Options {
            exclude_subscriptions: vec!["A".into()],
            ..Default::default()
        };
        assert_eq!(ids(select(all(), &parents, &except)), ["b", "c"]);
        let mg = Options {
            management_group: Some("ROOT".into()),
            ..Default::default()
        };
        let picked = select(all(), &parents, &mg);
        assert_eq!(ids(picked.clone()), ["a"]);
        assert_eq!(
            picked[0].parent.as_ref().map(|p| p.name.as_str()),
            Some("Prod")
        );
    }

    #[test]
    fn each_cloud_has_its_own_endpoint_scope_and_sign_in() {
        let gov = Arm::new(Arc::new(Token), AzureCloud::UsGovernment);
        assert_eq!(
            gov.scope(),
            "https://management.usgovcloudapi.net/.default"
        );
        let public = Arm::new(Arc::new(Token), AzureCloud::Public);
        assert_eq!(public.scope(), "https://management.azure.com/.default");
        assert_eq!(
            AzureCloud::UsGovernment.client_options().cloud.as_deref(),
            Some(&CloudConfiguration::AzureGovernment)
        );
        assert_eq!(
            AzureCloud::Public.client_options().cloud.as_deref(),
            Some(&CloudConfiguration::AzurePublic)
        );
    }

    #[test]
    fn saved_options_name_their_cloud_and_default_to_public() {
        let gov =
            Options { cloud: AzureCloud::UsGovernment, ..Default::default() };
        let json = serde_json::to_value(&gov).unwrap();
        assert_eq!(json["cloud"], "AzureUSGovernment");
        assert_eq!(serde_json::from_value::<Options>(json).unwrap(), gov);
        let old: Options = serde_json::from_str(r#"{"tenant": "t"}"#).unwrap();
        assert_eq!(old.cloud, AzureCloud::Public);
    }

    #[test]
    fn the_caller_is_read_from_the_token_claims() {
        let jwt = |claims: serde_json::Value| {
            format!("h.{}.s", base64::encode_url_safe(claims.to_string()))
        };
        assert_eq!(
            caller(&jwt(
                serde_json::json!({"upn": "ana@contoso.com", "oid": "1"})
            )),
            "ana@contoso.com"
        );
        assert_eq!(
            caller(&jwt(serde_json::json!({"appid": "app-1", "oid": "1"}))),
            "app-1"
        );
        assert_eq!(caller("not-a-jwt"), "unknown");
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    /// A whole scan against canned answers: one subscription with one VM,
    /// its disk and its resource group.
    #[test]
    fn a_scan_records_subscriptions_resources_and_targets() {
        let p = "/subscriptions/s1/resourceGroups/shop/providers";
        let empty = || Reply::ok(serde_json::json!({"data": []}));
        let script = Script::routes(vec![
            (
                "/subscriptions?api-version",
                Reply::ok(serde_json::json!({"value": [
                    {"subscriptionId": "S1", "displayName": "shop-prod", "state": "Enabled", "tenantId": "t"},
                ]})),
            ),
            (
                "managementGroupAncestorsChain",
                Reply::ok(serde_json::json!({"data": [
                    {"subscriptionId": "s1", "chain": [{"name": "prod", "displayName": "Prod"}]},
                ]})),
            ),
            (
                "microsoft.compute/virtualmachines'",
                Reply::ok(serde_json::json!({"data": [{
                    "id": format!("{p}/Microsoft.Compute/virtualMachines/web-1"),
                    "name": "web-1", "location": "eastus", "subscriptionId": "s1",
                    "vmSize": "Standard_D2s_v5", "osType": "Linux", "nics": [],
                }]})),
            ),
            (
                "microsoft.compute/disks'\\n| project",
                Reply::ok(serde_json::json!({"data": [{
                    "id": format!("{p}/Microsoft.Compute/disks/web-1-os"),
                    "name": "web-1-os", "location": "eastus", "subscriptionId": "s1",
                    "sizeGb": 64, "managedBy": format!("{p}/Microsoft.Compute/virtualMachines/web-1"),
                }]})),
            ),
            (
                "microsoft.resources/subscriptions/resourcegroups",
                Reply::ok(serde_json::json!({"data": [{
                    "id": "/subscriptions/s1/resourceGroups/shop", "name": "shop",
                    "location": "eastus", "subscriptionId": "s1",
                }]})),
            ),
            (
                "Microsoft.Compute/skus",
                Reply::ok(serde_json::json!({"value": [{
                    "resourceType": "virtualMachines", "name": "Standard_D2s_v5",
                    "capabilities": [{"name": "vCPUs", "value": "2"}, {"name": "MemoryGB", "value": "8"}],
                }]})),
            ),
            ("ResourceGraph", empty()),
        ]);
        let arm = Arm::with_transport(
            Arc::new(Token),
            Some(script.transport()),
            "https://arm.test",
        );
        let credential = Credential(Arc::new(Token));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let scan = block_on(scan_with(
            &arm,
            &credential,
            &Options::default(),
            Some(tx),
        ))
        .unwrap();

        assert_eq!(scan.provider, Provider::Azure);
        assert_eq!(scan.targets.len(), 1);
        assert_eq!(scan.targets[0].status, TargetStatus::Scanned);
        let kinds: Vec<&str> = {
            let mut k: Vec<&str> =
                scan.resources.iter().map(|r| r.kind().as_str()).collect();
            k.sort_unstable();
            k
        };
        assert_eq!(
            kinds,
            [
                "account",
                "deployment_group",
                "instance",
                "instance_type",
                "volume"
            ]
        );
        let Some(account) =
            scan.resources.iter().find(|r| r.kind().as_str() == "account")
        else {
            panic!()
        };
        let precip_types::Body::Account(a) = &account.body else { panic!() };
        assert_eq!(a.parent_name.as_deref(), Some("Prod"));
        let edges: Vec<&str> =
            scan.relationships.iter().map(|r| r.edge.as_str()).collect();
        assert!(edges.contains(&"attached_to"));
        assert_eq!(
            edges.iter().filter(|e| **e == "in_deployment_group").count(),
            2
        );

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert!(matches!(
            events.first(),
            Some(ScanEvent::AccountsDiscovered { total: 1 })
        ));
        assert!(matches!(
            events.last(),
            Some(ScanEvent::AccountFinished { resources: 4, .. })
        ));
    }

    #[test]
    fn a_failing_query_marks_the_subscription_partial_not_the_scan_failed() {
        let script = Script::routes(vec![
            (
                "/subscriptions?api-version",
                Reply::ok(serde_json::json!({"value": [
                    {"subscriptionId": "s1", "displayName": "x", "state": "Enabled"},
                ]})),
            ),
            (
                "networksecuritygroups",
                Reply {
                    status: 403,
                    headers: Vec::new(),
                    body: serde_json::json!({"error": {"code": "AuthorizationFailed", "message": "denied"}}),
                },
            ),
            ("ResourceGraph", Reply::ok(serde_json::json!({"data": []}))),
        ]);
        let arm = Arm::with_transport(
            Arc::new(Token),
            Some(script.transport()),
            "https://arm.test",
        );
        let scan = block_on(scan_with(
            &arm,
            &Credential(Arc::new(Token)),
            &Options::default(),
            None,
        ))
        .unwrap();
        let TargetStatus::Partial(why) = &scan.targets[0].status else {
            panic!("{:?}", scan.targets[0].status)
        };
        assert!(
            why.contains("(network)") && why.contains("AuthorizationFailed"),
            "{why}"
        );
    }

    #[test]
    fn no_matching_subscription_is_an_error() {
        let script = Script::routes(vec![
            (
                "/subscriptions?api-version",
                Reply::ok(serde_json::json!({"value": []})),
            ),
            ("ResourceGraph", Reply::ok(serde_json::json!({"data": []}))),
        ]);
        let arm = Arm::with_transport(
            Arc::new(Token),
            Some(script.transport()),
            "https://arm.test",
        );
        let err = block_on(scan_with(
            &arm,
            &Credential(Arc::new(Token)),
            &Options::default(),
            None,
        ))
        .unwrap_err();
        assert!(matches!(err, ScanError::NoSubscriptions));
    }
}
