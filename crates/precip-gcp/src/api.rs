//! Google Cloud over the shared REST client: the credentials' headers on
//! every request, Google's error body, and paging by `nextPageToken`.

use google_cloud_auth::credentials::{CacheableResource, Credentials};
use http::header::AUTHORIZATION;
use http::{Extensions, HeaderMap};
use precip_collect::BoxFuture;
use precip_collect::http::{
    Auth, AuthError, Client, HttpError, Request, Url, body_text, url,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

#[cfg(test)]
use precip_collect::http::fake::{Bearer, Script};

/// Calls in flight at once. Cloud Asset Inventory allows 100 `ListAssets`
/// calls a minute per quota project, so a scan keeps a few going and
/// backs off when throttled.
const IN_FLIGHT: usize = 4;

/// Application Default Credentials as request headers.
#[derive(Debug)]
struct Adc(Credentials);

#[derive(Debug, thiserror::Error)]
#[error("the credentials returned no headers")]
struct NoHeaders;

impl Auth for Adc {
    fn headers<'a>(
        &'a self,
        _request: &'a Request,
    ) -> BoxFuture<'a, Result<HeaderMap, AuthError>> {
        Box::pin(async move {
            match self.0.headers(Extensions::new()).await? {
                CacheableResource::New { data, .. } => Ok(data),
                CacheableResource::NotModified => Err(NoHeaders.into()),
            }
        })
    }
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

/// Google's error body: `{"error": {"status": ..., "message": ...}}`.
fn error_body(body: &[u8]) -> (String, String) {
    let parsed: ErrorBody = serde_json::from_slice(body).unwrap_or_default();
    let message = if parsed.error.message.is_empty() {
        body_text(body)
    } else {
        parsed.error.message
    };
    (parsed.error.status, message)
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
    http: Client,
    hosts: Hosts,
}

impl Gcp {
    pub(crate) fn new(credentials: Credentials) -> Result<Self, HttpError> {
        Ok(Self {
            http: Client::new(
                Arc::new(Adc(credentials)),
                error_body,
                IN_FLIGHT,
            )?,
            hosts: Hosts::Google,
        })
    }

    /// A client whose requests go to `script`, for tests.
    #[cfg(test)]
    pub(crate) fn fake(script: Arc<Script>) -> Self {
        Self {
            http: Client::with_transport(
                script,
                Arc::new(Bearer),
                error_body,
                IN_FLIGHT,
                std::time::Duration::ZERO,
            ),
            hosts: Hosts::Test,
        }
    }

    /// The URL of `path` on `service`, such as `cloudasset` and
    /// `/v1/projects/p/assets`.
    pub(crate) fn url(
        &self,
        service: &str,
        path: &str,
    ) -> Result<Url, HttpError> {
        match self.hosts {
            Hosts::Google => {
                url(&format!("https://{service}.googleapis.com{path}"))
            }
            #[cfg(test)]
            Hosts::Test => url(&format!("https://gcp.test/{service}{path}")),
        }
    }

    /// The bearer token the credentials send, for reading who it was
    /// issued to.
    pub(crate) async fn token(&self) -> Result<String, HttpError> {
        let probe = Request::get(self.url("oauth2", "/tokeninfo")?);
        let headers =
            self.http.auth().headers(&probe).await.map_err(HttpError::Auth)?;
        Ok(headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_string())
    }

    pub(crate) async fn send(
        &self,
        request: &Request,
    ) -> Result<Vec<u8>, HttpError> {
        Ok(self.http.send(request).await?.body)
    }

    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<T, HttpError> {
        self.http.get(url).await
    }

    /// Every page of a list call, following `nextPageToken`.
    pub(crate) async fn pages<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<Vec<T>, HttpError> {
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
            let reply = self.http.send(&Request::get(page_url)).await?;
            let next: Next = reply.json()?;
            out.push(reply.json()?);
            if next.next_page_token.is_empty() {
                return Ok(out);
            }
            token = next.next_page_token;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use precip_collect::http::fake::{ok, status};

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    #[test]
    fn pages_follow_the_next_page_token_until_it_is_empty() {
        let script = Arc::new(Script::queue(vec![
            ok(serde_json::json!({"items": [1], "nextPageToken": "t2"})),
            ok(serde_json::json!({"items": [2], "nextPageToken": ""})),
        ]));
        let gcp = Gcp::fake(script.clone());
        let url = gcp
            .url("cloudasset", "/v1/projects/p/assets?pageSize=1000")
            .unwrap();
        let pages: Vec<serde_json::Value> = block_on(gcp.pages(url)).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(
            script.urls(),
            [
                "https://gcp.test/cloudasset/v1/projects/p/assets?pageSize=1000",
                "https://gcp.test/cloudasset/v1/projects/p/assets?pageSize=1000&pageToken=t2",
            ]
        );
    }

    #[test]
    fn googles_error_body_names_the_status() {
        let script = Arc::new(Script::queue(vec![status(
            403,
            serde_json::json!({"error": {"code": 403, "message": "denied", "status": "PERMISSION_DENIED"}}),
        )]));
        let gcp = Gcp::fake(script);
        let err = block_on(
            gcp.get::<serde_json::Value>(gcp.url("compute", "/x").unwrap()),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "HTTP 403 PERMISSION_DENIED: denied");
        assert_eq!(
            error_body(b"<html>bad gateway</html>").1,
            "<html>bad gateway</html>"
        );
    }

    #[test]
    fn the_token_is_read_from_the_authorization_header() {
        let gcp = Gcp::fake(Arc::new(Script::default()));
        assert_eq!(block_on(gcp.token()).unwrap(), "token");
    }
}
