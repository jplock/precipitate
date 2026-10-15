//! Azure Resource Manager over the `azure_core` pipeline: bearer token,
//! retries that honour `Retry-After`, and paging for both ARM lists
//! (`nextLink`) and Resource Graph queries (`$skipToken`).

use crate::AzureCloud;
use azure_core::credentials::TokenCredential;
use azure_core::error::ErrorKind;
use azure_core::http::policies::Policy;
use azure_core::http::policies::auth::BearerTokenAuthorizationPolicy;
use azure_core::http::{
    ClientOptions, Context, ExponentialRetryOptions, Method, Pipeline, Request,
    RetryOptions, Transport, Url,
};
use azure_core::time::Duration;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tokio::sync::Semaphore;

const GRAPH_API_VERSION: &str = "2022-10-01";
/// Resource Graph accepts at most this many subscriptions per query.
pub(crate) const GRAPH_MAX_SUBSCRIPTIONS: usize = 1000;
/// Rows per Resource Graph page; the service maximum.
const GRAPH_PAGE: u32 = 1000;
/// Resource Graph allows about 15 queries per 5 seconds per user, so a few
/// in flight keeps a scan under the limit without serialising it.
const IN_FLIGHT: usize = 4;

/// A failed ARM call.
#[derive(Debug, thiserror::Error)]
pub enum ArmError {
    /// The service answered with an error status.
    #[error("HTTP {status} {code}: {message}")]
    Http { status: u16, code: String, message: String },
    #[error(transparent)]
    Azure(azure_core::Error),
    #[error("parsing the response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bad URL {0}")]
    Url(String),
}

/// One client for every ARM call in a scan.
#[derive(Debug, Clone)]
pub(crate) struct Arm {
    pipeline: Pipeline,
    endpoint: String,
    scope: String,
    in_flight: Arc<Semaphore>,
}

#[derive(Deserialize)]
struct ListPage<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
    #[serde(rename = "nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct GraphPage<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    #[serde(rename = "$skipToken")]
    skip_token: Option<String>,
}

impl From<azure_core::Error> for ArmError {
    fn from(e: azure_core::Error) -> Self {
        match e.kind() {
            ErrorKind::HttpResponse { status, error_code, .. } => Self::Http {
                status: u16::from(*status),
                code: error_code.clone().unwrap_or_default(),
                message: e.to_string(),
            },
            ErrorKind::Connection
            | ErrorKind::Io
            | ErrorKind::DataConversion
            | ErrorKind::Credential
            | ErrorKind::Other => Self::Azure(e),
        }
    }
}

impl Arm {
    pub(crate) fn new(
        credential: Arc<dyn TokenCredential>,
        cloud: AzureCloud,
    ) -> Self {
        Self::with_transport(credential, None, cloud.endpoint())
    }

    /// A client whose requests go to `transport`, for tests.
    pub(crate) fn with_transport(
        credential: Arc<dyn TokenCredential>,
        transport: Option<Transport>,
        endpoint: &str,
    ) -> Self {
        let retry = ExponentialRetryOptions {
            // A large tenant can stay throttled for minutes.
            max_total_elapsed: Duration::seconds(300),
            ..ExponentialRetryOptions::default()
        };
        let options = ClientOptions {
            retry: RetryOptions::exponential(retry),
            transport,
            ..ClientOptions::default()
        };
        let scope = format!("{endpoint}/.default");
        let auth: Arc<dyn Policy> = Arc::new(
            BearerTokenAuthorizationPolicy::new(credential, [scope.clone()]),
        );
        Self {
            pipeline: Pipeline::new(
                option_env!("CARGO_PKG_NAME"),
                option_env!("CARGO_PKG_VERSION"),
                options,
                Vec::new(),
                vec![auth],
                None,
            ),
            endpoint: endpoint.to_string(),
            scope,
            in_flight: Arc::new(Semaphore::new(IN_FLIGHT)),
        }
    }

    /// The token scope for this cloud's Resource Manager.
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }

    fn url(&self, path_or_url: &str) -> Result<Url, ArmError> {
        let full = if path_or_url.starts_with("https://")
            || path_or_url.starts_with("http://")
        {
            path_or_url.to_string()
        } else {
            format!("{}{path_or_url}", self.endpoint)
        };
        Url::parse(&full).map_err(|_| ArmError::Url(full))
    }

    async fn send<T: DeserializeOwned>(
        &self,
        mut request: Request,
    ) -> Result<T, ArmError> {
        let _permit = self.in_flight.acquire().await;
        let response = self
            .pipeline
            .send(&Context::new(), &mut request, Some(Default::default()))
            .await
            .map_err(ArmError::from)?;
        Ok(serde_json::from_slice(response.body().as_ref())?)
    }

    /// Every item of an ARM list call, following `nextLink`.
    pub(crate) async fn list<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Vec<T>, ArmError> {
        let mut out = Vec::new();
        let mut next = Some(self.url(path)?);
        while let Some(url) = next {
            let page: ListPage<T> =
                self.send(Request::new(url, Method::Get)).await?;
            out.extend(page.value);
            next =
                page.next_link.as_deref().map(|l| self.url(l)).transpose()?;
        }
        Ok(out)
    }

    /// Every row a Resource Graph query returns over `subscriptions`,
    /// following `$skipToken`.
    pub(crate) async fn graph<T: DeserializeOwned>(
        &self,
        subscriptions: &[String],
        query: &str,
    ) -> Result<Vec<T>, ArmError> {
        let url = self.url(&format!(
            "/providers/Microsoft.ResourceGraph/resources?api-version={GRAPH_API_VERSION}"
        ))?;
        let mut out = Vec::new();
        let mut skip_token: Option<String> = None;
        loop {
            let mut options = serde_json::json!({
                "$top": GRAPH_PAGE,
                "resultFormat": "objectArray",
            });
            if let Some(token) = &skip_token {
                options["$skipToken"] = token.clone().into();
            }
            let mut request = Request::new(url.clone(), Method::Post);
            request.insert_header("content-type", "application/json");
            request.set_json(&serde_json::json!({
                "subscriptions": subscriptions,
                "query": query,
                "options": options,
            }))?;
            let page: GraphPage<T> = self.send(request).await?;
            out.extend(page.data);
            match page.skip_token {
                Some(t) if !t.is_empty() => skip_token = Some(t),
                Some(_) | None => return Ok(out),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A transport that answers from a script, and a credential that
    //! always has a token.

    use azure_core::credentials::{
        AccessToken, TokenCredential, TokenRequestOptions,
    };
    use azure_core::http::headers::Headers;
    use azure_core::http::{
        AsyncRawResponse, Body, HttpClient, Request, StatusCode, Transport,
    };
    use azure_core::time::{Duration, OffsetDateTime};
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    pub(crate) struct Token;

    #[async_trait::async_trait]
    impl TokenCredential for Token {
        async fn get_token(
            &self,
            _scopes: &[&str],
            _options: Option<TokenRequestOptions<'_>>,
        ) -> azure_core::Result<AccessToken> {
            Ok(AccessToken::new(
                "token",
                OffsetDateTime::now_utc() + Duration::hours(1),
            ))
        }
    }

    /// One scripted reply: status, headers and body.
    pub(crate) struct Reply {
        pub status: u16,
        pub headers: Vec<(&'static str, &'static str)>,
        pub body: serde_json::Value,
    }

    impl Reply {
        pub(crate) fn ok(body: serde_json::Value) -> Self {
            Self { status: 200, headers: Vec::new(), body }
        }
    }

    type Canned = (u16, Vec<(&'static str, &'static str)>, Vec<u8>);

    fn canned(r: Reply) -> Canned {
        (r.status, r.headers, r.body.to_string().into_bytes())
    }

    /// Answers each request with the next reply, or with the first route
    /// whose text appears in the URL or body, and records what was sent:
    /// method, URL and body.
    #[derive(Debug, Default)]
    pub(crate) struct Script {
        replies: Mutex<Vec<Canned>>,
        routes: Vec<(&'static str, Canned)>,
        pub sent: Mutex<Vec<(String, String, String)>>,
    }

    impl Script {
        pub(crate) fn new(replies: Vec<Reply>) -> Arc<Self> {
            let mut queue: Vec<_> = replies.into_iter().map(canned).collect();
            queue.reverse();
            Arc::new(Self { replies: Mutex::new(queue), ..Default::default() })
        }

        pub(crate) fn routes(routes: Vec<(&'static str, Reply)>) -> Arc<Self> {
            Arc::new(Self {
                routes: routes
                    .into_iter()
                    .map(|(k, r)| (k, canned(r)))
                    .collect(),
                ..Default::default()
            })
        }

        pub(crate) fn transport(self: &Arc<Self>) -> Transport {
            Transport::new(self.clone())
        }

        pub(crate) fn sent(&self) -> Vec<(String, String, String)> {
            self.sent.lock().map(|s| s.clone()).unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl HttpClient for Script {
        async fn execute_request(
            &self,
            request: &Request,
        ) -> azure_core::Result<AsyncRawResponse> {
            let body = match request.body() {
                Body::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                Body::SeekableStream(_) => String::new(),
            };
            let url = request.url().to_string();
            let routed = self
                .routes
                .iter()
                .find(|(k, _)| url.contains(k) || body.contains(k))
                .map(|(_, c)| c.clone());
            if let Ok(mut sent) = self.sent.lock() {
                sent.push((request.method().to_string(), url, body));
            }
            let (status, headers, body) = routed
                .or_else(|| self.replies.lock().ok().and_then(|mut q| q.pop()))
                .unwrap_or((500, Vec::new(), b"{}".to_vec()));
            let mut h = Headers::new();
            for (k, v) in headers {
                h.insert(k, v);
            }
            Ok(AsyncRawResponse::from_bytes(StatusCode::from(status), h, body))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Reply, Script, Token};
    use super::*;

    fn arm(script: &Arc<Script>) -> Arm {
        Arm::with_transport(
            Arc::new(Token),
            Some(script.transport()),
            "https://arm.test",
        )
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    #[test]
    fn lists_follow_next_link_until_it_stops() {
        let script = Script::new(vec![
            Reply::ok(serde_json::json!({
                "value": [{"n": 1}],
                "nextLink": "https://arm.test/subscriptions?page=2",
            })),
            Reply::ok(serde_json::json!({"value": [{"n": 2}]})),
        ]);
        let got: Vec<serde_json::Value> =
            block_on(arm(&script).list("/subscriptions?api-version=1"))
                .unwrap();
        assert_eq!(
            got,
            [serde_json::json!({"n": 1}), serde_json::json!({"n": 2})]
        );
        let urls: Vec<String> =
            script.sent().into_iter().map(|s| s.1).collect();
        assert_eq!(
            urls,
            [
                "https://arm.test/subscriptions?api-version=1",
                "https://arm.test/subscriptions?page=2"
            ]
        );
    }

    #[test]
    fn graph_queries_page_with_the_skip_token_and_name_their_subscriptions() {
        let script = Script::new(vec![
            Reply::ok(
                serde_json::json!({"data": [{"id": "a"}], "$skipToken": "t1"}),
            ),
            Reply::ok(
                serde_json::json!({"data": [{"id": "b"}], "$skipToken": ""}),
            ),
        ]);
        let got: Vec<serde_json::Value> = block_on(
            arm(&script).graph(&["s1".to_string()], "resources | project id"),
        )
        .unwrap();
        assert_eq!(got.len(), 2);
        let sent = script.sent();
        assert_eq!(sent[0].0, "POST");
        let first: serde_json::Value =
            serde_json::from_str(&sent[0].2).unwrap();
        assert_eq!(first["subscriptions"], serde_json::json!(["s1"]));
        assert_eq!(first["options"]["resultFormat"], "objectArray");
        assert!(first["options"].get("$skipToken").is_none());
        let second: serde_json::Value =
            serde_json::from_str(&sent[1].2).unwrap();
        assert_eq!(second["options"]["$skipToken"], "t1");
    }

    #[test]
    fn throttled_calls_are_retried_after_the_delay_the_service_asks_for() {
        let script = Script::new(vec![
            Reply {
                status: 429,
                headers: vec![("retry-after", "0")],
                body: serde_json::json!({}),
            },
            Reply::ok(serde_json::json!({"value": [{"n": 1}]})),
        ]);
        let got: Vec<serde_json::Value> =
            block_on(arm(&script).list("/subscriptions")).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(script.sent().len(), 2, "one retry");
    }

    #[test]
    fn an_access_error_is_returned_not_retried() {
        let script = Script::new(vec![Reply {
            status: 403,
            headers: Vec::new(),
            body: serde_json::json!({"error": {"code": "AuthorizationFailed", "message": "no"}}),
        }]);
        let err =
            block_on(arm(&script).list::<serde_json::Value>("/subscriptions"))
                .unwrap_err();
        assert!(err.to_string().contains("AuthorizationFailed"), "{err}");
        assert_eq!(script.sent().len(), 1);
    }

    #[test]
    fn a_malformed_body_is_a_json_error() {
        let script =
            Script::new(vec![Reply::ok(serde_json::json!({"value": "nope"}))]);
        let err =
            block_on(arm(&script).list::<serde_json::Value>("/subscriptions"))
                .unwrap_err();
        assert!(matches!(err, ArmError::Json(_)), "{err:?}");
    }
}
