//! OCI over the shared REST client: every attempt signed afresh, OCI's
//! error body, regional service endpoints, and paging by the
//! `opc-next-page` header.

use crate::sign::{self, Signer};
use http::HeaderMap;
use precip_collect::BoxFuture;
use precip_collect::http::{
    Auth, AuthError, Client, HttpError, Request, Url, body_text, url,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

#[cfg(test)]
use crate::sign::{parse_key, test_key};
#[cfg(test)]
use precip_collect::http::fake::Script;

/// Calls in flight at once. OCI rate-limits each API per tenancy, so a
/// scan keeps several going and backs off when throttled.
const IN_FLIGHT: usize = 8;
/// Rows per page; the maximum most list calls accept.
pub(crate) const PAGE: &str = "1000";

/// Requests signed with the profile's key.
#[derive(Debug)]
struct Signing(Signer);

impl Auth for Signing {
    fn headers<'a>(
        &'a self,
        request: &'a Request,
    ) -> BoxFuture<'a, Result<HeaderMap, AuthError>> {
        let signed = self.0.headers(
            request.method(),
            &request.url,
            request.body.as_ref().map(|(b, _)| b.as_slice()),
            jiff::Timestamp::now(),
        );
        Box::pin(async move { signed.map_err(Into::into) })
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ErrorBody {
    code: String,
    message: String,
}

/// OCI's error body: `{"code": ..., "message": ...}`.
fn error_body(body: &[u8]) -> (String, String) {
    let parsed: ErrorBody = serde_json::from_slice(body).unwrap_or_default();
    let message = if parsed.message.is_empty() {
        body_text(body)
    } else {
        parsed.message
    };
    (parsed.code, message)
}

/// Where a list call's rows are: the body is a JSON array, or an object
/// whose `items` is one.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Rows {
    Array,
    Items,
}

#[derive(Deserialize)]
struct Items<T> {
    items: Vec<T>,
}

/// An OCI service, by its endpoint's first label.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Service {
    /// Compute, Block Volume and Networking.
    Core,
    Identity,
    Search,
}

impl Service {
    fn host(self, region: &str, domain: &str) -> String {
        match self {
            Self::Core => format!("iaas.{region}.{domain}"),
            Self::Identity => format!("identity.{region}.oci.{domain}"),
            Self::Search => format!("query.{region}.oci.{domain}"),
        }
    }
}

/// One client for every call in a scan.
#[derive(Debug, Clone)]
pub(crate) struct Oci {
    http: Client,
    /// The realm's domain, such as `oraclecloud.com`.
    domain: String,
    /// The scheme and host requests go to instead, in tests.
    test_host: Option<String>,
}

impl Oci {
    pub(crate) fn new(
        signer: Signer,
        domain: String,
    ) -> Result<Self, HttpError> {
        Ok(Self {
            http: Client::new(
                Arc::new(Signing(signer)),
                error_body,
                IN_FLIGHT,
            )?,
            domain,
            test_host: None,
        })
    }

    /// A client whose requests go to `script` at a test host, with no
    /// wait between attempts.
    #[cfg(test)]
    pub(crate) fn fake(script: Arc<Script>) -> Self {
        let key = parse_key(&test_key::pem()).expect("key");
        Self {
            http: Client::with_transport(
                script,
                Arc::new(Signing(Signer::new("t/u/f".into(), key))),
                error_body,
                IN_FLIGHT,
                std::time::Duration::ZERO,
            ),
            domain: "oraclecloud.com".into(),
            test_host: Some("https://oci.test".into()),
        }
    }

    /// The URL of `path` with `query` on `service` in `region`. The query
    /// is encoded as the signature covers it. In tests the host becomes
    /// the first path segment.
    pub(crate) fn url(
        &self,
        service: Service,
        region: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Url, HttpError> {
        let host = service.host(region, &self.domain);
        let base = match &self.test_host {
            Some(t) => format!("{t}/{host}{path}"),
            None => format!("https://{host}{path}"),
        };
        if query.is_empty() {
            url(&base)
        } else {
            url(&format!("{base}?{}", sign::query(query)))
        }
    }

    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<T, HttpError> {
        self.http.get(url).await
    }

    /// Every row of a list call, following `opc-next-page`. A POST
    /// resends its JSON body with each page.
    pub(crate) async fn list<T: DeserializeOwned>(
        &self,
        url: Url,
        body: Option<Vec<u8>>,
        rows: Rows,
    ) -> Result<Vec<T>, HttpError> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut page_url = url.clone();
            if let Some(p) = &page {
                let q = match page_url.query() {
                    Some(q) => format!("{q}&page={}", sign::encode(p)),
                    None => format!("page={}", sign::encode(p)),
                };
                page_url.set_query(Some(&q));
            }
            let request = match &body {
                Some(b) => {
                    Request::post(page_url, b.clone(), "application/json")
                }
                None => Request::get(page_url),
            };
            let reply = self.http.send(&request).await?;
            let got: Vec<T> = match rows {
                Rows::Array => reply.json()?,
                Rows::Items => reply.json::<Items<T>>()?.items,
            };
            out.extend(got);
            match reply.header("opc-next-page").filter(|p| !p.is_empty()) {
                Some(p) => page = Some(p.to_string()),
                None => return Ok(out),
            }
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
            .expect("rt")
            .block_on(f)
    }

    #[test]
    fn lists_follow_the_next_page_header_and_every_request_is_signed() {
        let script = Arc::new(Script::queue(vec![
            ok(serde_json::json!([1])).with("opc-next-page", "p 2"),
            ok(serde_json::json!([2])),
        ]));
        let oci = Oci::fake(script.clone());
        let url = oci
            .url(
                Service::Core,
                "us-ashburn-1",
                "/20160918/instances",
                &[("compartmentId", "c")],
            )
            .unwrap();
        let got: Vec<u8> = block_on(oci.list(url, None, Rows::Array)).unwrap();
        assert_eq!(got, [1, 2]);
        let sent = script.sent();
        assert_eq!(
            sent[0].url,
            "https://oci.test/iaas.us-ashburn-1.oraclecloud.com/20160918/instances?compartmentId=c"
        );
        assert!(sent[1].url.ends_with("compartmentId=c&page=p%202"));
        assert!(sent.iter().all(|s| s.headers.contains_key("authorization")));
    }

    #[test]
    fn posts_are_paged_with_their_body_and_read_items_from_a_field() {
        let script = Arc::new(Script::queue(vec![
            ok(serde_json::json!({"items": [1]})).with("opc-next-page", "n"),
            ok(serde_json::json!({"items": [2]})),
        ]));
        let oci = Oci::fake(script.clone());
        let url = oci
            .url(Service::Search, "eu-frankfurt-1", "/20180409/resources", &[])
            .unwrap();
        let got: Vec<u8> =
            block_on(oci.list(url, Some(b"{\"q\":1}".to_vec()), Rows::Items))
                .unwrap();
        assert_eq!(got, [1, 2]);
        let sent = script.sent();
        assert!(
            sent[0].url.contains("query.eu-frankfurt-1.oci.oraclecloud.com")
        );
        assert!(sent.iter().all(|s| s.body.as_deref() == Some("{\"q\":1}")));
        assert!(sent[0].headers.contains_key("x-content-sha256"));
        assert!(sent[1].url.ends_with("?page=n"));
    }

    #[test]
    fn ocis_error_body_names_the_code() {
        let script = Arc::new(Script::queue(vec![status(
            404,
            serde_json::json!({"code": "NotAuthorizedOrNotFound", "message": "no"}),
        )]));
        let oci = Oci::fake(script);
        let err = block_on(oci.get::<serde_json::Value>(
            oci.url(Service::Identity, "r", "/x", &[]).unwrap(),
        ))
        .unwrap_err();
        assert_eq!(err.to_string(), "HTTP 404 NotAuthorizedOrNotFound: no");
        assert_eq!(error_body(b"bad gateway").1, "bad gateway");
    }
}
