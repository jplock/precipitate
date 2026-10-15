//! The REST client the scanners without an SDK share: each request gets
//! headers from the scanner's auth (a bearer token, a request signature),
//! throttled and failed attempts are retried with backoff that honours
//! `Retry-After`, and a scan keeps a bounded number of calls in flight.
//! Paging differs by cloud, so each scanner follows its own next-page
//! marker.

use crate::BoxFuture;
use http::header::{CONTENT_TYPE, RETRY_AFTER};
use http::{HeaderMap, HeaderValue};
pub use reqwest::Url;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// The longest wait between attempts, unless the service asks for longer.
const MAX_DELAY: Duration = Duration::from_secs(32);
/// How long a request keeps retrying. A large estate can stay throttled
/// for minutes.
const RETRY_FOR: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Why the scanner's auth could not produce headers.
pub type AuthError = Box<dyn std::error::Error + Send + Sync>;

/// A failed call.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The service answered with an error status, and the code and
    /// message its error body gave.
    #[error("HTTP {status} {code}: {message}")]
    Status { status: u16, code: String, message: String },
    #[error("sending the request")]
    Transport(#[source] reqwest::Error),
    #[error("authenticating the request")]
    Auth(#[source] AuthError),
    #[error("parsing the response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bad URL {0}")]
    Url(String),
}

impl HttpError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Status { status, .. } => {
                matches!(status, 429 | 500 | 502 | 503 | 504)
            }
            Self::Transport(e) => {
                e.is_timeout() || e.is_connect() || e.is_request()
            }
            Self::Auth(_) | Self::Json(_) | Self::Url(_) => false,
        }
    }
}

/// One request. Every call a scan makes is a read: a GET, or a POST
/// whose body asks a question (a search, a token lookup).
#[derive(Debug, Clone)]
pub struct Request {
    pub url: Url,
    /// The body and its content type; a request with one is a POST.
    pub body: Option<(Vec<u8>, &'static str)>,
}

impl Request {
    pub fn get(url: Url) -> Self {
        Self { url, body: None }
    }

    pub fn post(url: Url, body: Vec<u8>, content_type: &'static str) -> Self {
        Self { url, body: Some((body, content_type)) }
    }

    pub fn method(&self) -> &'static str {
        if self.body.is_some() { "POST" } else { "GET" }
    }
}

/// What came back.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    /// A response header as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, HttpError> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

/// Sends requests: reqwest, or a script in tests.
pub trait Transport: Send + Sync + std::fmt::Debug {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        headers: HeaderMap,
    ) -> BoxFuture<'a, Result<Reply, reqwest::Error>>;
}

/// The headers that authenticate one request.
pub trait Auth: Send + Sync + std::fmt::Debug {
    fn headers<'a>(
        &'a self,
        request: &'a Request,
    ) -> BoxFuture<'a, Result<HeaderMap, AuthError>>;
}

/// A service's error body as its code and message.
pub type ErrorBody = fn(&[u8]) -> (String, String);

#[derive(Debug)]
struct Reqwest(reqwest::Client);

impl Transport for Reqwest {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        headers: HeaderMap,
    ) -> BoxFuture<'a, Result<Reply, reqwest::Error>> {
        Box::pin(async move {
            let builder = match &request.body {
                Some((body, _)) => {
                    self.0.post(request.url.clone()).body(body.clone())
                }
                None => self.0.get(request.url.clone()),
            };
            let response = builder.headers(headers).send().await?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let body = response.bytes().await?.to_vec();
            Ok(Reply { status, headers, body })
        })
    }
}

/// One client for every call in a scan.
#[derive(Debug, Clone)]
pub struct Client {
    transport: Arc<dyn Transport>,
    auth: Arc<dyn Auth>,
    error_body: ErrorBody,
    first_delay: Duration,
    in_flight: Arc<Semaphore>,
}

/// The wait before attempt `attempt + 1`: what the service asked for,
/// else doubling from `first` up to [`MAX_DELAY`].
fn delay(first: Duration, attempt: u32, asked: Option<Duration>) -> Duration {
    asked.unwrap_or_else(|| {
        first.saturating_mul(1 << attempt.min(16)).min(MAX_DELAY)
    })
}

impl Client {
    /// A client over reqwest that keeps `in_flight` calls going at once
    /// and reads error bodies with `error_body`.
    pub fn new(
        auth: Arc<dyn Auth>,
        error_body: ErrorBody,
        in_flight: usize,
    ) -> Result<Self, HttpError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("precip/", env!("CARGO_PKG_VERSION")))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(HttpError::Transport)?;
        Ok(Self::with_transport(
            Arc::new(Reqwest(client)),
            auth,
            error_body,
            in_flight,
            Duration::from_secs(1),
        ))
    }

    /// A client over another transport; tests pass a script and no wait
    /// between attempts.
    pub fn with_transport(
        transport: Arc<dyn Transport>,
        auth: Arc<dyn Auth>,
        error_body: ErrorBody,
        in_flight: usize,
        first_delay: Duration,
    ) -> Self {
        Self {
            transport,
            auth,
            error_body,
            first_delay,
            in_flight: Arc::new(Semaphore::new(in_flight)),
        }
    }

    /// The auth the client signs requests with.
    pub fn auth(&self) -> &dyn Auth {
        self.auth.as_ref()
    }

    /// Send `request`, retrying throttled and failed attempts, and return
    /// the first success. Each attempt is authenticated afresh, so a
    /// signature's date stays current.
    pub async fn send(&self, request: &Request) -> Result<Reply, HttpError> {
        let _permit = self.in_flight.acquire().await;
        let started = tokio::time::Instant::now();
        let mut attempt = 0;
        loop {
            let (err, asked) = match self.attempt(request).await {
                Ok(reply) if (200..300).contains(&reply.status) => {
                    return Ok(reply);
                }
                Ok(reply) => {
                    let (code, message) = (self.error_body)(&reply.body);
                    let asked = reply
                        .header(RETRY_AFTER.as_str())
                        .and_then(|v| v.trim().parse().ok())
                        .map(Duration::from_secs);
                    (
                        HttpError::Status {
                            status: reply.status,
                            code,
                            message,
                        },
                        asked,
                    )
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

    async fn attempt(&self, request: &Request) -> Result<Reply, HttpError> {
        let mut headers =
            self.auth.headers(request).await.map_err(HttpError::Auth)?;
        if let Some((_, content_type)) = &request.body {
            headers
                .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        }
        self.transport
            .send(request, headers)
            .await
            .map_err(HttpError::Transport)
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<T, HttpError> {
        self.send(&Request::get(url)).await?.json()
    }
}

/// A URL from text, or the error naming it.
pub fn url(s: &str) -> Result<Url, HttpError> {
    Url::parse(s).map_err(|_| HttpError::Url(s.to_string()))
}

/// An error body's text, cut short, for services whose body is not JSON.
pub fn body_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).chars().take(200).collect()
}

#[cfg(any(test, feature = "fake"))]
pub mod fake {
    //! A transport that answers from a script, and auth that adds a
    //! bearer token, for tests.

    use super::{Auth, AuthError, BoxFuture, Reply, Request, Transport};
    use http::header::AUTHORIZATION;
    use http::{HeaderMap, HeaderValue};
    use std::sync::Mutex;

    #[derive(Debug)]
    pub struct Bearer;

    impl Auth for Bearer {
        fn headers<'a>(
            &'a self,
            _request: &'a Request,
        ) -> BoxFuture<'a, Result<HeaderMap, AuthError>> {
            let mut h = HeaderMap::new();
            h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer token"));
            Box::pin(async move { Ok(h) })
        }
    }

    /// One scripted reply.
    #[derive(Debug, Clone)]
    pub struct Canned {
        pub status: u16,
        pub headers: Vec<(&'static str, String)>,
        pub body: serde_json::Value,
    }

    impl Canned {
        /// The same reply with a response header.
        pub fn with(mut self, name: &'static str, value: &str) -> Self {
            self.headers.push((name, value.to_string()));
            self
        }
    }

    pub fn ok(body: serde_json::Value) -> Canned {
        Canned { status: 200, headers: Vec::new(), body }
    }

    pub fn status(status: u16, body: serde_json::Value) -> Canned {
        Canned { status, headers: Vec::new(), body }
    }

    /// What one request carried.
    #[derive(Debug, Clone)]
    pub struct Sent {
        pub url: String,
        pub body: Option<String>,
        pub headers: HeaderMap,
    }

    /// Answers each request with the first route whose text appears in the
    /// URL, else the next queued reply, else 404, and records each
    /// request. 404 is not retried, so an unscripted call fails at once.
    #[derive(Debug, Default)]
    pub struct Script {
        queue: Mutex<Vec<Canned>>,
        routes: Vec<(String, Canned)>,
        sent: Mutex<Vec<Sent>>,
    }

    impl Script {
        pub fn queue(replies: Vec<Canned>) -> Self {
            let mut q = replies;
            q.reverse();
            Self { queue: Mutex::new(q), ..Default::default() }
        }

        pub fn routes(routes: Vec<(&str, Canned)>) -> Self {
            Self {
                routes: routes
                    .into_iter()
                    .map(|(k, c)| (k.to_string(), c))
                    .collect(),
                ..Default::default()
            }
        }

        pub fn sent(&self) -> Vec<Sent> {
            self.sent.lock().map(|s| s.clone()).unwrap_or_default()
        }

        pub fn urls(&self) -> Vec<String> {
            self.sent().into_iter().map(|s| s.url).collect()
        }
    }

    impl Transport for Script {
        fn send<'a>(
            &'a self,
            request: &'a Request,
            headers: HeaderMap,
        ) -> BoxFuture<'a, Result<Reply, reqwest::Error>> {
            let url = request.url.to_string();
            let routed = self
                .routes
                .iter()
                .find(|(k, _)| url.contains(k.as_str()))
                .map(|(_, c)| c.clone());
            if let Ok(mut sent) = self.sent.lock() {
                sent.push(Sent {
                    url,
                    body: request
                        .body
                        .as_ref()
                        .map(|(b, _)| String::from_utf8_lossy(b).into_owned()),
                    headers,
                });
            }
            let canned = routed
                .or_else(|| self.queue.lock().ok().and_then(|mut q| q.pop()))
                .unwrap_or_else(|| status(404, serde_json::json!({})));
            let mut h = HeaderMap::new();
            for (k, v) in canned.headers {
                if let Ok(v) = HeaderValue::from_str(&v) {
                    h.insert(k, v);
                }
            }
            Box::pin(async move {
                Ok(Reply {
                    status: canned.status,
                    headers: h,
                    body: canned.body.to_string().into_bytes(),
                })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Bearer, Script, ok, status};
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    fn parse(body: &[u8]) -> (String, String) {
        let v: serde_json::Value =
            serde_json::from_slice(body).unwrap_or_default();
        (
            v["code"].as_str().unwrap_or_default().to_string(),
            v["message"]
                .as_str()
                .map_or_else(|| body_text(body), str::to_string),
        )
    }

    fn client(script: &Arc<Script>) -> Client {
        Client::with_transport(
            script.clone(),
            Arc::new(Bearer),
            parse,
            4,
            Duration::ZERO,
        )
    }

    #[test]
    fn throttled_and_failed_calls_are_retried_with_auth_each_time() {
        let script = Arc::new(Script::queue(vec![
            status(429, serde_json::json!({})).with("retry-after", "0"),
            status(503, serde_json::json!({})),
            ok(serde_json::json!({"n": 1})),
        ]));
        let got: serde_json::Value =
            block_on(client(&script).get(url("https://x.test/a").unwrap()))
                .unwrap();
        assert_eq!(got["n"], 1);
        let sent = script.sent();
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|s| s.headers.contains_key("authorization")));
    }

    #[test]
    fn an_access_error_is_returned_with_the_services_code_not_retried() {
        let script = Arc::new(Script::queue(vec![status(
            403,
            serde_json::json!({"code": "Denied", "message": "no"}),
        )]));
        let err = block_on(
            client(&script)
                .get::<serde_json::Value>(url("https://x.test/a").unwrap()),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "HTTP 403 Denied: no");
        assert_eq!(script.sent().len(), 1);
    }

    #[test]
    fn a_post_carries_its_body_and_content_type() {
        let script = Arc::new(Script::queue(vec![ok(serde_json::json!({}))]));
        let req = Request::post(
            url("https://x.test/q").unwrap(),
            b"a=1".to_vec(),
            "application/x-www-form-urlencoded",
        );
        assert_eq!(req.method(), "POST");
        block_on(client(&script).send(&req)).unwrap();
        let sent = &script.sent()[0];
        assert_eq!(sent.body.as_deref(), Some("a=1"));
        assert_eq!(
            sent.headers[CONTENT_TYPE],
            "application/x-www-form-urlencoded"
        );
    }

    #[test]
    fn a_malformed_body_and_a_bad_url_are_errors() {
        let script = Arc::new(Script::queue(vec![ok(serde_json::json!([1]))]));
        let err = block_on(client(&script).get::<std::collections::BTreeMap<
            String,
            u8,
        >>(
            url("https://x.test/a").unwrap()
        ))
        .unwrap_err();
        assert!(matches!(err, HttpError::Json(_)), "{err:?}");
        assert!(matches!(url("not a url"), Err(HttpError::Url(_))));
        assert_eq!(
            body_text(b"<html>bad gateway</html>"),
            "<html>bad gateway</html>"
        );
    }

    #[test]
    fn delays_double_up_to_a_ceiling_unless_the_service_says_otherwise() {
        let s = Duration::from_secs;
        assert_eq!(delay(s(1), 0, None), s(1));
        assert_eq!(delay(s(1), 3, None), s(8));
        assert_eq!(delay(s(1), 30, None), MAX_DELAY);
        assert_eq!(delay(s(1), 0, Some(s(60))), s(60));
    }
}
