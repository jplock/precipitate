//! OCI request signatures (version 1 of Oracle's profile of
//! draft-cavage-http-signatures-08): every request is signed with the API
//! key's RSA private key over its date, method, path, query and host, and
//! for a request with a body, over the body's length, type and SHA-256.

use aws_lc_rs::digest::{SHA256, digest};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, DATE};
use http::{HeaderMap, HeaderValue};
use precip_collect::http::Url;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("the key file is not a PEM private key")]
    NotPem,
    #[error(
        "the key is encrypted; precip reads only unencrypted keys (remove the pass phrase with `openssl rsa`)"
    )]
    Encrypted,
    #[error("the key is not a 2048- to 8192-bit RSA key: {0}")]
    Rejected(String),
    #[error("signing failed")]
    Sign,
}

/// Signs requests with one key.
pub(crate) struct Signer {
    /// `<tenancy>/<user>/<fingerprint>`, or `ST$<token>` for a session.
    key_id: String,
    key: RsaKeyPair,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// A PEM private key: PKCS#1 (`RSA PRIVATE KEY`) or PKCS#8 (`PRIVATE
/// KEY`). Text outside the markers, such as the `OCI_API_KEY` line the
/// console adds, is ignored.
pub(crate) fn parse_key(pem: &str) -> Result<RsaKeyPair, KeyError> {
    if pem.contains("ENCRYPTED") {
        return Err(KeyError::Encrypted);
    }
    for (label, pkcs8) in [("RSA PRIVATE KEY", false), ("PRIVATE KEY", true)] {
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        let Some((_, rest)) = pem.split_once(&begin) else { continue };
        let Some((body, _)) = rest.split_once(&end) else { continue };
        let b64: String = body.split_whitespace().collect();
        let der = STANDARD.decode(b64).map_err(|_| KeyError::NotPem)?;
        let key = if pkcs8 {
            RsaKeyPair::from_pkcs8(&der)
        } else {
            RsaKeyPair::from_der(&der)
        };
        return key.map_err(|e| KeyError::Rejected(e.to_string()));
    }
    Err(KeyError::NotPem)
}

/// Percent-encode a query value per RFC 3986: everything but unreserved
/// characters. The signature covers the query as sent, so the encoding
/// must be the one the URL keeps.
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A query string from pairs, encoded per [`encode`].
pub(crate) fn query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The date header's value: RFC 7231's IMF-fixdate.
pub(crate) fn http_date(t: jiff::Timestamp) -> String {
    t.strftime("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// The lines a signature covers, in the order `headers` names them.
fn signing_string(
    method: &str,
    url: &Url,
    date: &str,
    body: Option<(&str, usize, &str)>,
) -> (String, &'static str) {
    let mut target = url.path().to_string();
    if let Some(q) = url.query() {
        target.push('?');
        target.push_str(q);
    }
    let host = url.host_str().unwrap_or_default();
    let mut lines = vec![
        format!("date: {date}"),
        format!("(request-target): {} {target}", method.to_lowercase()),
        format!("host: {host}"),
    ];
    let headers = match body {
        Some((sha, len, content_type)) => {
            lines.push(format!("x-content-sha256: {sha}"));
            lines.push(format!("content-type: {content_type}"));
            lines.push(format!("content-length: {len}"));
            "date (request-target) host x-content-sha256 content-type content-length"
        }
        None => "date (request-target) host",
    };
    (lines.join("\n"), headers)
}

impl Signer {
    pub(crate) fn new(key_id: String, key: RsaKeyPair) -> Self {
        Self { key_id, key }
    }

    /// The headers that sign a request made at `now`. A request with a
    /// body is sent as JSON.
    pub(crate) fn headers(
        &self,
        method: &str,
        url: &Url,
        body: Option<&[u8]>,
        now: jiff::Timestamp,
    ) -> Result<HeaderMap, KeyError> {
        let date = http_date(now);
        let sha = body.map(|b| STANDARD.encode(digest(&SHA256, b)));
        let content_type = "application/json";
        let (text, names) = signing_string(
            method,
            url,
            &date,
            sha.as_deref().zip(body).map(|(s, b)| (s, b.len(), content_type)),
        );
        let mut sig = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                text.as_bytes(),
                &mut sig,
            )
            .map_err(|_| KeyError::Sign)?;
        let authorization = format!(
            "Signature version=\"1\",keyId=\"{}\",algorithm=\"rsa-sha256\",headers=\"{names}\",signature=\"{}\"",
            self.key_id,
            STANDARD.encode(&sig)
        );
        let value =
            |s: &str| HeaderValue::from_str(s).map_err(|_| KeyError::Sign);
        let mut h = HeaderMap::new();
        h.insert(DATE, value(&date)?);
        h.insert(AUTHORIZATION, value(&authorization)?);
        if let (Some(sha), Some(b)) = (sha, body) {
            h.insert("x-content-sha256", value(&sha)?);
            h.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
            h.insert(CONTENT_LENGTH, HeaderValue::from(b.len()));
        }
        Ok(h)
    }
}

#[cfg(test)]
pub(crate) mod test_key {
    //! A fresh 2048-bit key in PEM, for tests.

    use super::*;
    use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
    use aws_lc_rs::rsa::KeySize;

    pub(crate) fn pem() -> String {
        let key = RsaKeyPair::generate(KeySize::Rsa2048).expect("key");
        let der = AsDer::<Pkcs8V1Der>::as_der(&key).expect("der");
        let b64 = STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect();
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\nOCI_API_KEY\n",
            lines.join("\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{
        KeyPair, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey,
    };

    /// Oracle's documented GET example.
    #[test]
    fn the_signing_string_matches_oracles_example() {
        let url = Url::parse(&format!(
            "https://iaas.us-phoenix-1.oraclecloud.com/20160918/instances?{}",
            query(&[
                ("availabilityDomain", "Pjwf: PHX-AD-1"),
                ("compartmentId", "ocid1.compartment.oc1..aaaaaaaam3we6vgnherjq5q2idnccdflvjsnog7mlr6rtdb25gilchfeyjxa"),
                ("displayName", "TeamXInstances"),
                ("volumeId", "ocid1.volume.oc1.phx.abyhqljrgvttnlx73nmrwfaux7kcvzfs3s66izvxf2h4lgvyndsdsnoiwr5q"),
            ])
        ))
        .unwrap();
        let (text, headers) =
            signing_string("GET", &url, "Thu, 05 Jan 2014 21:31:40 GMT", None);
        assert_eq!(
            text,
            "date: Thu, 05 Jan 2014 21:31:40 GMT\n\
             (request-target): get /20160918/instances?availabilityDomain=Pjwf%3A%20PHX-AD-1&compartmentId=ocid1.compartment.oc1..aaaaaaaam3we6vgnherjq5q2idnccdflvjsnog7mlr6rtdb25gilchfeyjxa&displayName=TeamXInstances&volumeId=ocid1.volume.oc1.phx.abyhqljrgvttnlx73nmrwfaux7kcvzfs3s66izvxf2h4lgvyndsdsnoiwr5q\n\
             host: iaas.us-phoenix-1.oraclecloud.com"
        );
        assert_eq!(headers, "date (request-target) host");
    }

    #[test]
    fn requests_are_signed_with_the_key_and_bodies_are_hashed() {
        let key = parse_key(&test_key::pem()).unwrap();
        let public = key.public_key().as_ref().to_vec();
        let signer = Signer::new("t/u/f".into(), key);
        let url = Url::parse(
            "https://query.us-ashburn-1.oci.oraclecloud.com/20180409/resources",
        )
        .unwrap();
        let now: jiff::Timestamp = "2026-10-15T12:00:00Z".parse().unwrap();
        let h = signer.headers("POST", &url, Some(b"{}"), now).unwrap();
        assert_eq!(h[DATE], "Thu, 15 Oct 2026 12:00:00 GMT");
        assert_eq!(
            h["x-content-sha256"],
            "RBNvo1WzZ4oRRq0W9+hknpT7T8If536DEMBg9hyq/4o="
        );
        assert_eq!(h[CONTENT_LENGTH], "2");
        let auth = h[AUTHORIZATION].to_str().unwrap();
        assert!(auth.starts_with(
            "Signature version=\"1\",keyId=\"t/u/f\",algorithm=\"rsa-sha256\","
        ));
        assert!(auth.contains("headers=\"date (request-target) host x-content-sha256 content-type content-length\""));
        let sig =
            auth.rsplit_once("signature=\"").unwrap().1.trim_end_matches('"');
        let (text, _) = signing_string(
            "POST",
            &url,
            "Thu, 15 Oct 2026 12:00:00 GMT",
            Some((
                "RBNvo1WzZ4oRRq0W9+hknpT7T8If536DEMBg9hyq/4o=",
                2,
                "application/json",
            )),
        );
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
            .verify(text.as_bytes(), &STANDARD.decode(sig).unwrap())
            .expect("the signature verifies");
    }

    #[test]
    fn keys_that_cannot_be_used_are_refused() {
        assert!(matches!(parse_key("nothing"), Err(KeyError::NotPem)));
        assert!(matches!(
            parse_key(
                "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAA\n-----END ENCRYPTED PRIVATE KEY-----"
            ),
            Err(KeyError::Encrypted)
        ));
        assert!(matches!(
            parse_key(
                "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----"
            ),
            Err(KeyError::Rejected(_))
        ));
        assert!(matches!(
            parse_key(
                "-----BEGIN RSA PRIVATE KEY-----\n!!\n-----END RSA PRIVATE KEY-----"
            ),
            Err(KeyError::NotPem)
        ));
    }

    #[test]
    fn query_values_are_percent_encoded_per_rfc_3986() {
        assert_eq!(encode("a b:c/d~e"), "a%20b%3Ac%2Fd~e");
        assert_eq!(query(&[("x", "1"), ("y", "a&b")]), "x=1&y=a%26b");
    }
}
