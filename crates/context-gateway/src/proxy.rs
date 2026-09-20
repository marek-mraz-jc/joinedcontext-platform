//! Forwarding a request to the broker behind an endpoint (T-0005, EP-01, GW20).
//!
//! The upstream is fixed at start-up and the forwarded path is built from the route the
//! router matched, so nothing a client sends can point the gateway at a different host or
//! a different resource tree.
//!
//! Hop-by-hop headers are dropped in both directions: they describe one TCP connection,
//! and this is two.
//!
//! The same client carries the notification egress (R46), where the target is a subscriber's
//! own webhook rather than the broker, so it speaks TLS when the URL asks for it. The broker
//! hop stays plain `http://` in the cluster, where Linkerd carries the mTLS.

use axum::body::Body;
use axum::http::header::{HeaderName, CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING};
use axum::http::{HeaderMap, Method, Response, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use jc_core::ProblemDetails;
use rustls::RootCertStore;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::CertificateDer;

/// Headers that belong to one connection and must not be relayed to the next (RFC 9110
/// section 7.6.1).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The broker the gateway forwards to.
#[derive(Debug, Clone)]
pub struct Broker {
    client: Client<HttpsConnector<HttpConnector>, Body>,
    base: String,
}

/// Why a forwarded request produced no answer.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// The gateway could not build a legal upstream URI from the matched route.
    #[error("cannot address the broker: {0}")]
    Uri(String),
    /// Trust anchors the deployment depends on cannot be used, whether the image's own or
    /// the bundle it named. Never an answer to a request: the gateway refuses to start rather
    /// than deliver to an unverified peer.
    #[error("no usable trust anchor: {0}")]
    Trust(String),
    /// The broker did not answer.
    #[error("the broker did not answer: {0}")]
    Unreachable(String),
}

impl From<ProxyError> for ProblemDetails {
    fn from(error: ProxyError) -> Self {
        // The caller learns that the platform, not their request, is at fault, and nothing
        // about the topology behind the endpoint.
        tracing::error!(%error, "forwarding failed");
        ProblemDetails::new(502, "upstream-unavailable", "Broker Unavailable")
    }
}

impl Broker {
    /// A client for the broker at `base`, which is scheme and authority only, with no trust
    /// anchors.
    ///
    /// The broker hop is in-cluster and plain `http://`, where Linkerd carries the mTLS, so
    /// this client needs none. A delivery over TLS goes through [`Broker::verified`] or
    /// [`Broker::trusting`]; an `https://` request made through this one fails to verify,
    /// which is the right way for a missing anchor to end (R46).
    pub fn new(base: impl Into<String>) -> Self {
        Self::with(base, RootCertStore::empty())
    }

    /// The same, trusting the public roots the image carries.
    ///
    /// This is what the binary builds: the anchors are read from the image rather than
    /// compiled in, so a root that is withdrawn or added arrives with the next base image.
    pub fn verified(base: impl Into<String>) -> Result<Self, ProxyError> {
        Ok(Self::with(base, system_roots()?))
    }

    /// The same, trusting the certificates in `bundle` (PEM) on top of the public roots.
    ///
    /// This is `JC_GATEWAY_EGRESS_CA_BUNDLE`: an installation whose subscribers sit behind its
    /// own CA. A bundle that parses to nothing is an error rather than a silent fallback to
    /// the public roots, because a delivery that lost a trust anchor is a delivery to
    /// somebody else (R46).
    pub fn trusting(base: impl Into<String>, bundle: &[u8]) -> Result<Self, ProxyError> {
        let mut roots = system_roots()?;
        match add_pem(&mut roots, bundle)? {
            0 => Err(ProxyError::Trust(
                "no CERTIFICATE block in the bundle".to_owned(),
            )),
            _ => Ok(Self::with(base, roots)),
        }
    }

    /// The same client aimed at another origin (the egress dispatcher's target, R46).
    ///
    /// The connection pool and the trust anchors are shared: a delivery reuses the
    /// connection to a subscriber it already spoke to, and cannot trust more than the
    /// gateway was configured to.
    pub fn aimed_at(&self, origin: impl Into<String>) -> Self {
        Self {
            client: self.client.clone(),
            base: origin.into(),
        }
    }

    fn with(base: impl Into<String>, roots: RootCertStore) -> Self {
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            // Plain HTTP stays available: the broker hop is in-cluster and never TLS.
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            base: base.into(),
        }
    }

    /// The upstream this gateway forwards to.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Forwards one request and returns the broker's answer.
    ///
    /// `path_and_query` is absolute and already percent-encoded, as it came off the wire:
    /// re-encoding it would corrupt the URN in an entity path.
    pub async fn send(
        &self,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: Body,
    ) -> Result<Response<Body>, ProxyError> {
        let uri: Uri = format!("{}{path_and_query}", self.base)
            .parse()
            .map_err(|e: axum::http::uri::InvalidUri| ProxyError::Uri(e.to_string()))?;

        let mut upstream = axum::http::Request::builder().method(method).uri(&uri);
        let out = upstream.headers_mut().expect("a fresh builder has headers");
        copy_relayable(&headers, out);
        // The upstream authority is ours, not the client's.
        out.remove(HOST);

        let request = upstream
            .body(body)
            .map_err(|e| ProxyError::Uri(e.to_string()))?;

        let started = std::time::Instant::now();
        let response = self.client.request(request).await;
        crate::telemetry::broker_round_trip(started.elapsed().as_secs_f64(), response.is_ok());
        let response = response.map_err(|e| ProxyError::Unreachable(e.to_string()))?;

        let (parts, body) = response.into_parts();
        let mut answer = Response::builder().status(parts.status);
        let out = answer.headers_mut().expect("a fresh builder has headers");
        copy_relayable(&parts.headers, out);
        answer
            .body(Body::new(body))
            .map_err(|e| ProxyError::Uri(e.to_string()))
    }
}

/// Where a Debian-derived image keeps its trust anchors. The gateway's own image is
/// `gcr.io/distroless/cc-debian12`, which ships `ca-certificates`.
const SYSTEM_ROOTS: &str = "/etc/ssl/certs/ca-certificates.crt";

/// The public trust anchors the image carries, read at start-up rather than compiled in (R46).
///
/// `SSL_CERT_FILE` is the conventional override and is what a host keeping its roots
/// elsewhere sets. A file that cannot be read, or that holds no certificate, is an error and
/// never an empty store: a dispatcher that trusts nothing fails every delivery, and one that
/// silently trusts less than it was configured to delivers to somebody else.
fn system_roots() -> Result<RootCertStore, ProxyError> {
    roots_from_file(&std::env::var("SSL_CERT_FILE").unwrap_or_else(|_| SYSTEM_ROOTS.to_owned()))
}

/// The anchors in one PEM file, or why there are none.
fn roots_from_file(path: &str) -> Result<RootCertStore, ProxyError> {
    let bundle = std::fs::read(path).map_err(|e| ProxyError::Trust(format!("{path}: {e}")))?;
    let mut roots = RootCertStore::empty();
    match add_pem(&mut roots, &bundle)? {
        0 => Err(ProxyError::Trust(format!(
            "{path}: no CERTIFICATE block in the trust store"
        ))),
        _ => Ok(roots),
    }
}

/// Adds every CERTIFICATE block of a PEM bundle to `roots` and says how many there were.
fn add_pem(roots: &mut RootCertStore, bundle: &[u8]) -> Result<usize, ProxyError> {
    let mut added = 0usize;
    for certificate in CertificateDer::pem_slice_iter(bundle) {
        let certificate = certificate.map_err(|e| ProxyError::Trust(e.to_string()))?;
        roots
            .add(certificate)
            .map_err(|e| ProxyError::Trust(e.to_string()))?;
        added += 1;
    }
    Ok(added)
}

/// Copies every header that describes the message rather than the connection.
fn copy_relayable(from: &HeaderMap, to: &mut HeaderMap) {
    // A `Connection: x` header names further headers that are hop-by-hop for this hop.
    let named: Vec<HeaderName> = from
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::try_from(name.trim()).ok())
        .collect();

    for (name, value) in from {
        if HOP_BY_HOP.contains(&name.as_str()) || named.contains(name) {
            continue;
        }
        to.append(name.clone(), value.clone());
    }
}

/// Replaces a buffered body, so the length the broker or the client reads is the length of
/// what it is actually given.
pub fn with_body(mut parts: axum::http::response::Parts, bytes: Vec<u8>) -> Response<Body> {
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(TRANSFER_ENCODING);
    parts.headers.insert(
        CONTENT_LENGTH,
        axum::http::HeaderValue::from(bytes.len() as u64),
    );
    if parts.status == StatusCode::NO_CONTENT {
        return Response::from_parts(parts, Body::empty());
    }
    Response::from_parts(parts, Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn hop_by_hop_headers_do_not_cross_the_gateway() {
        let mut from = HeaderMap::new();
        from.insert(
            CONNECTION,
            HeaderValue::from_static("keep-alive, x-secret-hop"),
        );
        from.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        from.insert("x-secret-hop", HeaderValue::from_static("leaked"));
        from.insert(
            "content-type",
            HeaderValue::from_static("application/ld+json"),
        );

        let mut to = HeaderMap::new();
        copy_relayable(&from, &mut to);

        assert_eq!(
            to.get("content-type").expect("relayed"),
            "application/ld+json"
        );
        for dropped in ["connection", "keep-alive", "x-secret-hop"] {
            assert!(!to.contains_key(dropped), "{dropped} crossed the gateway");
        }
    }

    /// A PEM file holding one self-signed CA, in a path of this test's own.
    fn bundle(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("jc-{name}-{}.pem", std::process::id()));
        std::fs::write(&path, contents).expect("the fixture is writable");
        path
    }

    fn authority() -> String {
        let key = rcgen::KeyPair::generate().expect("a CA key");
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.self_signed(&key).expect("a CA").pem()
    }

    #[test]
    fn the_anchors_of_a_pem_file_are_read() {
        let path = bundle("roots", &authority());
        let roots = roots_from_file(path.to_str().expect("a utf-8 path")).expect("one anchor");
        assert_eq!(roots.len(), 1);
        std::fs::remove_file(path).ok();
    }

    /// R46: the gateway stops instead of dispatching with a trust store it could not read.
    #[test]
    fn a_trust_store_that_is_not_there_is_an_error() {
        let missing = std::env::temp_dir().join("jc-no-such-trust-store.pem");
        let error = roots_from_file(missing.to_str().expect("a utf-8 path"))
            .expect_err("a missing trust store cannot be trusted");
        assert!(matches!(error, ProxyError::Trust(_)), "{error}");
    }

    /// The shape that would otherwise pass silently: a file that reads fine and holds nothing.
    #[test]
    fn a_trust_store_with_no_certificate_is_an_error() {
        let path = bundle("empty-roots", "# the operator emptied this file\n");
        let error = roots_from_file(path.to_str().expect("a utf-8 path"))
            .expect_err("an empty trust store cannot be trusted");
        assert!(matches!(error, ProxyError::Trust(_)), "{error}");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_bundle_adds_its_anchors_to_the_ones_already_there() {
        let mut roots = RootCertStore::empty();
        assert_eq!(
            add_pem(&mut roots, authority().as_bytes()).expect("one block"),
            1
        );
        assert_eq!(
            add_pem(&mut roots, authority().as_bytes()).expect("one block"),
            1
        );
        assert_eq!(roots.len(), 2);
    }
}
