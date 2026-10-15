//! Google Cloud REST calls: the credentials' headers on every request,
//! retries with backoff on throttling and server errors, and paging by
//! `nextPageToken`.

use google_cloud_auth::credentials::{CacheableResource, Credentials};
use google_cloud_auth::errors::CredentialsError;
use http::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use http::{Extensions, HeaderMap};
use precip_collect::BoxFuture;
use reqwest::Url;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Calls in flight at once. Cloud Asset Inventory allows 100 `ListAssets`
/// calls a minute per quota project, so a scan keeps a few going and
/// backs off when throttled.
const IN_FLIGHT: usize = 4;
/// The longest a request waits between attempts, unless the service asks
/// for longer.
const MAX_DELAY: Duration = Duration::from_secs(32);
/// How long a request keeps retrying. A large organization can stay
/// throttled for minutes.
const RETRY_FOR: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// A failed Google Cloud call.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The service answered with an error status.
    #[error("HTTP {status} {code}: {message}")]
    Http { status: u16, code: String, message: String },
    #[error("sending the request")]
    Transport(#[source] reqwest::Error),
    #[error("getting a Google Cloud token")]
    Credentials(#[source] CredentialsError),
    #[error("the credentials returned no headers")]
    NoHeaders,
    #[error("parsing the response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bad URL {0}")]
    Url(String),
}

impl ApiError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Http { status, .. } => {
                matches!(status, 429 | 500 | 502 | 503 | 504)
            }
            Self::Transport(e) => {
                e.is_timeout() || e.is_connect() || e.is_request()
            }
            Self::Credentials(e) => e.is_transient(),
            Self::NoHeaders | Self::Json(_) | Self::Url(_) => false,
        }
    }
}

/// One request. Every call precip makes is a read: a GET, or a form POST
/// to the token information endpoint.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub url: Url,
    pub form: Option<String>,
}

/// What came back.
#[derive(Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

/// Sends requests: reqwest, or a script in tests.
pub(crate) trait Http: Send + Sync + std::fmt::Debug {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<Reply, ApiError>>;
}

#[derive(Debug)]
struct Reqwest(reqwest::Client);

impl Http for Reqwest {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<Reply, ApiError>> {
        Box::pin(async move {
            let builder = match &request.form {
                Some(form) => self
                    .0
                    .post(request.url.clone())
                    .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(form.clone()),
                None => self.0.get(request.url.clone()),
            };
            let response = builder
                .headers(headers.clone())
                .send()
                .await
                .map_err(ApiError::Transport)?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok()?.trim().parse().ok())
                .map(Duration::from_secs);
            let body =
                response.bytes().await.map_err(ApiError::Transport)?.to_vec();
            Ok(Reply { status, retry_after, body })
        })
    }
}

/// Where each service is reached: `https://<service>.googleapis.com`, or
/// one test host with the service as the first path segment.
#[derive(Debug, Clone)]
enum Hosts {
    Google,
    #[cfg(test)]
    Test,
}

/// One client for every call in a scan.
#[derive(Debug, Clone)]
pub(crate) struct Gcp {
    credentials: Credentials,
    http: Arc<dyn Http>,
    hosts: Hosts,
    first_delay: Duration,
    in_flight: Arc<Semaphore>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ErrorDetail {
    message: String,
    status: String,
}

fn http_error(status: u16, body: &[u8]) -> ApiError {
    let parsed: ErrorBody = serde_json::from_slice(body).unwrap_or_default();
    let message = if parsed.error.message.is_empty() {
        String::from_utf8_lossy(body).chars().take(200).collect()
    } else {
        parsed.error.message
    };
    ApiError::Http { status, code: parsed.error.status, message }
}

/// The wait before attempt `attempt + 1`: what the service asked for,
/// else doubling from `first` up to [`MAX_DELAY`].
fn delay(first: Duration, attempt: u32, asked: Option<Duration>) -> Duration {
    asked.unwrap_or_else(|| {
        first.saturating_mul(1 << attempt.min(16)).min(MAX_DELAY)
    })
}

impl Gcp {
    pub(crate) fn new(credentials: Credentials) -> Result<Self, ApiError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("precip/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::Transport)?;
        Ok(Self::with_http(
            credentials,
            Arc::new(Reqwest(client)),
            Hosts::Google,
            Duration::from_secs(1),
        ))
    }

    fn with_http(
        credentials: Credentials,
        http: Arc<dyn Http>,
        hosts: Hosts,
        first_delay: Duration,
    ) -> Self {
        Self {
            credentials,
            http,
            hosts,
            first_delay,
            in_flight: Arc::new(Semaphore::new(IN_FLIGHT)),
        }
    }

    /// A client whose requests go to `http`, with no wait between
    /// attempts, for tests.
    #[cfg(test)]
    pub(crate) fn fake(http: Arc<dyn Http>) -> Self {
        Self::with_http(fake::credentials(), http, Hosts::Test, Duration::ZERO)
    }

    /// The URL of `path` on `service`, such as `cloudasset` and
    /// `/v1/projects/p/assets`.
    pub(crate) fn url(
        &self,
        service: &str,
        path: &str,
    ) -> Result<Url, ApiError> {
        let full = match self.hosts {
            Hosts::Google => format!("https://{service}.googleapis.com{path}"),
            #[cfg(test)]
            Hosts::Test => format!("https://gcp.test/{service}{path}"),
        };
        Url::parse(&full).map_err(|_| ApiError::Url(full))
    }

    async fn headers(&self) -> Result<HeaderMap, ApiError> {
        match self
            .credentials
            .headers(Extensions::new())
            .await
            .map_err(ApiError::Credentials)?
        {
            CacheableResource::New { data, .. } => Ok(data),
            CacheableResource::NotModified => Err(ApiError::NoHeaders),
        }
    }

    /// The bearer token the credentials send, for reading who it was
    /// issued to.
    pub(crate) async fn token(&self) -> Result<String, ApiError> {
        let headers = self.headers().await?;
        Ok(headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_string())
    }

    /// Send `request`, retrying throttled and failed attempts, and return
    /// the body of the first success.
    pub(crate) async fn send(
        &self,
        request: &Request,
    ) -> Result<Vec<u8>, ApiError> {
        let _permit = self.in_flight.acquire().await;
        let started = tokio::time::Instant::now();
        let mut attempt = 0;
        loop {
            let (err, asked) = match self.attempt(request).await {
                Ok(reply) if (200..300).contains(&reply.status) => {
                    return Ok(reply.body);
                }
                Ok(reply) => {
                    (http_error(reply.status, &reply.body), reply.retry_after)
                }
                Err(e) => (e, None),
            };
            let wait = delay(self.first_delay, attempt, asked);
            if !err.is_retryable() || started.elapsed() + wait > RETRY_FOR {
                return Err(err);
            }
            tracing::debug!(url = %request.url, error = %err, ?wait, "retrying");
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    async fn attempt(&self, request: &Request) -> Result<Reply, ApiError> {
        let headers = self.headers().await?;
        self.http.send(request, &headers).await
    }

    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<T, ApiError> {
        let body = self.send(&Request { url, form: None }).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// Every page of a list call, following `nextPageToken`.
    pub(crate) async fn pages<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<Vec<T>, ApiError> {
        #[derive(Deserialize)]
        struct Next {
            #[serde(rename = "nextPageToken", default)]
            next_page_token: String,
        }
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let mut page_url = url.clone();
            if !token.is_empty() {
                page_url.query_pairs_mut().append_pair("pageToken", &token);
            }
            let body =
                self.send(&Request { url: page_url, form: None }).await?;
            let next: Next = serde_json::from_slice(&body)?;
            out.push(serde_json::from_slice(&body)?);
            if next.next_page_token.is_empty() {
                return Ok(out);
            }
            token = next.next_page_token;
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! An HTTP client that answers from a script, and credentials that
    //! always have a token.

    use super::{ApiError, BoxFuture, Http, Reply, Request};
    use google_cloud_auth::credentials::{
        CacheableResource, Credentials, CredentialsProvider, EntityTag,
    };
    use google_cloud_auth::errors::CredentialsError;
    use http::header::AUTHORIZATION;
    use http::{Extensions, HeaderMap, HeaderValue};
    use std::sync::Mutex;

    #[derive(Debug)]
    struct Token;

    impl CredentialsProvider for Token {
        async fn headers(
            &self,
            _extensions: Extensions,
        ) -> Result<CacheableResource<HeaderMap>, CredentialsError> {
            let mut h = HeaderMap::new();
            h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer token"));
            Ok(CacheableResource::New { entity_tag: EntityTag::new(), data: h })
        }

        async fn universe_domain(&self) -> Option<String> {
            None
        }
    }

    pub(crate) fn credentials() -> Credentials {
        Credentials::from(Token)
    }

    /// One scripted reply.
    #[derive(Debug, Clone)]
    pub(crate) struct Canned {
        pub status: u16,
        pub retry_after: Option<u64>,
        pub body: serde_json::Value,
    }

    pub(crate) fn ok(body: serde_json::Value) -> Canned {
        Canned { status: 200, retry_after: None, body }
    }

    pub(crate) fn status(status: u16, body: serde_json::Value) -> Canned {
        Canned { status, retry_after: None, body }
    }

    /// Answers each request with the first route whose text appears in the
    /// URL, else the next queued reply, else 404, and records each URL.
    #[derive(Debug, Default)]
    pub(crate) struct Script {
        queue: Mutex<Vec<Canned>>,
        routes: Vec<(String, Canned)>,
        sent: Mutex<Vec<String>>,
    }

    impl Script {
        pub(crate) fn queue(replies: Vec<Canned>) -> Self {
            let mut q = replies;
            q.reverse();
            Self { queue: Mutex::new(q), ..Default::default() }
        }

        pub(crate) fn routes(routes: Vec<(&str, Canned)>) -> Self {
            Self {
                routes: routes
                    .into_iter()
                    .map(|(k, c)| (k.to_string(), c))
                    .collect(),
                ..Default::default()
            }
        }

        pub(crate) fn sent(&self) -> Vec<String> {
            self.sent.lock().map(|s| s.clone()).unwrap_or_default()
        }
    }

    impl Http for Script {
        fn send<'a>(
            &'a self,
            request: &'a Request,
            _headers: &'a HeaderMap,
        ) -> BoxFuture<'a, Result<Reply, ApiError>> {
            let url = request.url.to_string();
            let routed = self
                .routes
                .iter()
                .find(|(k, _)| url.contains(k.as_str()))
                .map(|(_, c)| c.clone());
            if let Ok(mut sent) = self.sent.lock() {
                sent.push(url);
            }
            let canned = routed
                .or_else(|| self.queue.lock().ok().and_then(|mut q| q.pop()))
                .unwrap_or_else(|| status(404, serde_json::json!({})));
            Box::pin(async move {
                Ok(Reply {
                    status: canned.status,
                    retry_after: canned
                        .retry_after
                        .map(std::time::Duration::from_secs),
                    body: canned.body.to_string().into_bytes(),
                })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Canned, Script, ok, status};
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    fn client(script: &Arc<Script>) -> Gcp {
        Gcp::fake(script.clone())
    }

    #[test]
    fn pages_follow_the_next_page_token_until_it_is_empty() {
        let script = Arc::new(Script::queue(vec![
            ok(serde_json::json!({"items": [1], "nextPageToken": "t2"})),
            ok(serde_json::json!({"items": [2], "nextPageToken": ""})),
        ]));
        let gcp = client(&script);
        let url = gcp
            .url("cloudasset", "/v1/projects/p/assets?pageSize=1000")
            .unwrap();
        let pages: Vec<serde_json::Value> = block_on(gcp.pages(url)).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(
            script.sent(),
            [
                "https://gcp.test/cloudasset/v1/projects/p/assets?pageSize=1000",
                "https://gcp.test/cloudasset/v1/projects/p/assets?pageSize=1000&pageToken=t2",
            ]
        );
    }

    #[test]
    fn throttled_and_failed_calls_are_retried() {
        let script = Arc::new(Script::queue(vec![
            Canned {
                status: 429,
                retry_after: Some(0),
                body: serde_json::json!({}),
            },
            status(503, serde_json::json!({})),
            ok(serde_json::json!({"n": 1})),
        ]));
        let gcp = client(&script);
        let got: serde_json::Value =
            block_on(gcp.get(gcp.url("compute", "/x").unwrap())).unwrap();
        assert_eq!(got["n"], 1);
        assert_eq!(script.sent().len(), 3);
    }

    #[test]
    fn an_access_error_is_returned_with_its_status_not_retried() {
        let script = Arc::new(Script::queue(vec![status(
            403,
            serde_json::json!({"error": {"code": 403, "message": "denied", "status": "PERMISSION_DENIED"}}),
        )]));
        let gcp = client(&script);
        let err = block_on(
            gcp.get::<serde_json::Value>(gcp.url("compute", "/x").unwrap()),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "HTTP 403 PERMISSION_DENIED: denied");
        assert_eq!(script.sent().len(), 1);
    }

    #[test]
    fn a_non_json_error_body_becomes_the_message() {
        let err = http_error(502, b"<html>bad gateway</html>");
        assert!(err.to_string().contains("bad gateway"), "{err}");
    }

    #[test]
    fn a_malformed_body_is_a_json_error() {
        let script = Arc::new(Script::queue(vec![ok(serde_json::json!([1]))]));
        let gcp = client(&script);
        let err = block_on(gcp.get::<std::collections::BTreeMap<String, u8>>(
            gcp.url("compute", "/x").unwrap(),
        ))
        .unwrap_err();
        assert!(matches!(err, ApiError::Json(_)), "{err:?}");
    }

    #[test]
    fn delays_double_up_to_a_ceiling_unless_the_service_says_otherwise() {
        let s = Duration::from_secs;
        assert_eq!(delay(s(1), 0, None), s(1));
        assert_eq!(delay(s(1), 3, None), s(8));
        assert_eq!(delay(s(1), 30, None), MAX_DELAY);
        assert_eq!(delay(s(1), 0, Some(s(60))), s(60));
    }

    #[test]
    fn the_token_is_read_from_the_authorization_header() {
        let gcp = Gcp::fake(Arc::new(Script::default()));
        assert_eq!(block_on(gcp.token()).unwrap(), "token");
    }
}
