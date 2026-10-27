//! Forwarding a request to the broker behind an endpoint (T-0005, EP-01, GW20).
//!
//! The upstream is chosen by the gateway, never by the client: the broker of the space the
//! gateway pinned as tenant, or of the view it named, else the one fixed at start-up
//! (ADR-N-047). The forwarded path is built from the route the router matched, so nothing a
//! client sends can point the gateway at a different host or a different resource tree.
//!
//! Hop-by-hop headers are dropped in both directions: they describe one TCP connection,
//! and this is two.
//!
//! The same client carries the notification egress (R46), where the target is a subscriber's
//! own webhook rather than the broker, so it speaks TLS when the URL asks for it. The broker
//! hop stays plain `http://` in the cluster, where Linkerd carries the mTLS.

use axum::body::Body;
use axum::http::header::{
    HeaderName, ACCEPT_ENCODING, CONNECTION, CONTENT_LENGTH, HOST, TRANSFER_ENCODING,
};
use axum::http::{HeaderMap, Method, Response, StatusCode, Uri};
use axum::response::IntoResponse;
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
    /// The address of every broker but `main`: `{broker}` replaced by its name
    /// (`JC_GATEWAY_BROKER_URL_TEMPLATE`, ADR-N-047).
    template: Option<String>,
    /// Which broker each space and view lives on, swapped whole on a reload.
    routes: std::sync::Arc<arc_swap::ArcSwap<Routes>>,
}

/// The broker address of each space (by tenant) and each view (by name) whose broker is not
/// `main`; `None` is a broker with no address, refused rather than sent to `main`.
#[derive(Debug, Default)]
pub struct Routes {
    by_tenant: std::collections::HashMap<String, Option<String>>,
    by_view: std::collections::HashMap<String, Option<String>>,
    /// The broker each view lives on, by name, for the hub's label (ADR-N-047 §2.3).
    named: std::collections::HashMap<String, String>,
    /// Tenants and views a move job holds read-only (PF-115).
    moving: std::collections::HashSet<String>,
    /// The tenants alone of `moving`: the series the move job reads, never a view's slug.
    held: std::collections::HashSet<String>,
}

/// How long a writer is told to wait while its space moves (ADR-N-047 §2.6).
const MOVING_RETRY_SECONDS: u32 = 30;

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
    /// The request names a JSON-LD context other than the core one (T-3287).
    #[error("{0}")]
    Context(&'static str),
    /// The forwarded path has a `.` or `..` segment, which a hop resolving dot segments would
    /// turn into another resource than the one the gateway decided on (T-3588).
    #[error("a path segment of `.` or `..` names no resource")]
    DotSegment,
}

impl From<ProxyError> for ProblemDetails {
    fn from(error: ProxyError) -> Self {
        if let ProxyError::Context(why) = error {
            return ProblemDetails::bad_request().with_detail(why);
        }
        if let ProxyError::DotSegment = error {
            return ProblemDetails::bad_request().with_detail(error.to_string());
        }
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

    /// Addresses every broker but `main` by `template`, `{broker}` replaced by its name.
    pub fn with_template(mut self, template: Option<String>) -> Self {
        self.template = template;
        self
    }

    /// Routes each space and view to the broker it lives on (ADR-N-047), from the next request
    /// on. A broker without an address (no template, or a space naming no broker) is kept as
    /// `None`, so its requests are refused instead of reaching `main`.
    pub fn route(&self, brokers: &crate::store::SpaceBrokers) {
        let address = |broker: &String| -> Option<String> {
            match broker.as_str() {
                jc_core::kinds::IMPLICIT_BROKER => Some(self.base.clone()),
                "" => None,
                name => self.template.as_ref().map(|t| t.replace("{broker}", name)),
            }
        };
        let off_main = |table: &std::collections::BTreeMap<String, String>| {
            table
                .iter()
                .filter(|(_, broker)| broker.as_str() != jc_core::kinds::IMPLICIT_BROKER)
                .map(|(key, broker)| (key.clone(), address(broker)))
                .collect()
        };
        let routes = Routes {
            by_tenant: off_main(&brokers.by_space),
            by_view: off_main(&brokers.by_view),
            named: brokers.by_view.clone().into_iter().collect(),
            moving: brokers.moving.iter().cloned().collect(),
            held: brokers
                .moving
                .iter()
                .filter(|key| brokers.by_space.contains_key(*key))
                .cloned()
                .collect(),
        };
        crate::telemetry::held(&self.routes.load().held, &routes.held);
        crate::activity::name_spaces(&brokers.names);
        self.routes.store(std::sync::Arc::new(routes));
    }

    /// The name of the broker a view (an Endpoint's slug or `cs-{space}`) lives on.
    pub fn broker_of_view(&self, view: &str) -> String {
        self.routes
            .load()
            .named
            .get(view)
            .cloned()
            .unwrap_or_else(|| jc_core::kinds::IMPLICIT_BROKER.to_owned())
    }

    /// Whether this request reaches a space a move holds read-only and could write it: every
    /// method but `GET` and `HEAD`, except a query by `POST` (PF-115). An MCP call may write,
    /// so it waits too.
    fn held(&self, method: &Method, path: &str, headers: &HeaderMap) -> bool {
        if matches!(*method, Method::GET | Method::HEAD) {
            return false;
        }
        let route = path.split('?').next().unwrap_or_default();
        if *method == Method::POST && route.ends_with("/entityOperations/query") {
            return false;
        }
        let routes = self.routes.load();
        if routes.moving.is_empty() {
            return false;
        }
        let tenant = headers
            .get(crate::middleware::tenancy::TENANT)
            .and_then(|tenant| tenant.to_str().ok());
        let view = route
            .strip_prefix("/x/views/")
            .and_then(|rest| rest.split('/').next());
        tenant.is_some_and(|t| routes.moving.contains(t))
            || view.is_some_and(|v| routes.moving.contains(v))
    }

    /// The broker a request goes to: the pinned tenant's, else the named view's, else `main`.
    /// Only the gateway sets the tenant header; a client's is stripped before routing (GW20).
    pub fn base_for(&self, path: &str, headers: &HeaderMap) -> Result<String, ProxyError> {
        let routes = self.routes.load();
        let routed = headers
            .get(crate::middleware::tenancy::TENANT)
            .and_then(|tenant| tenant.to_str().ok())
            .and_then(|tenant| routes.by_tenant.get(tenant))
            .or_else(|| {
                let view = path.strip_prefix("/x/views/")?.split(['/', '?']).next()?;
                routes.by_view.get(view)
            });
        match routed {
            None => Ok(self.base.clone()),
            Some(Some(base)) => Ok(base.clone()),
            Some(None) => Err(ProxyError::Unreachable(
                "the space's broker has no address".to_owned(),
            )),
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
            template: None,
            routes: Default::default(),
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
    ///
    /// Absolute is checked, not assumed (T-2337, GW20). `self.base` ends at the authority with no
    /// trailing slash, so a path that does not begin with `/` is glued onto that authority and the
    /// URI parser reads what follows as more of it: `@evil.example/x` makes the broker's own
    /// address the userinfo of `evil.example`, and the hop leaves the upstream the deployment
    /// configured. Every caller passes a leading slash today; the check is what keeps that true
    /// when one of them is rewritten.
    pub async fn send(
        &self,
        method: Method,
        path_and_query: &str,
        headers: HeaderMap,
        body: Body,
    ) -> Result<Response<Body>, ProxyError> {
        // An empty path is the broker's root and has no authority to confuse; every other
        // relative one does.
        if !path_and_query.is_empty() && !path_and_query.starts_with('/') {
            return Err(ProxyError::Uri(
                "the forwarded path is not absolute".to_owned(),
            ));
        }
        // Every route that forwards a client's path passes here, whatever surface it serves.
        let path = path_and_query.split('?').next().unwrap_or_default();
        if path.split('/').any(crate::operations::is_dot_segment) {
            return Err(ProxyError::DotSegment);
        }
        if self.held(&method, path_and_query, &headers) {
            let mut answer = ProblemDetails::new(503, "space-moving", "Space Moving")
                .with_detail(
                    "the space is moving to another broker and takes no writes until the move ends",
                )
                .into_response();
            answer.headers_mut().insert(
                "retry-after",
                axum::http::HeaderValue::from(MOVING_RETRY_SECONDS),
            );
            return Ok(answer);
        }
        let base = self.base_for(path_and_query, &headers)?;
        let uri: Uri = format!("{base}{path_and_query}")
            .parse()
            .map_err(|e: axum::http::uri::InvalidUri| ProxyError::Uri(e.to_string()))?;

        // Whatever route forwards it: a context of the caller's own never reaches the broker,
        // which would expand the names the gateway decided on to other IRIs (T-3287).
        crate::context_guard::check_link(&headers).map_err(ProxyError::Context)?;

        let mut upstream = axum::http::Request::builder().method(method).uri(&uri);
        let out = upstream.headers_mut().expect("a fresh builder has headers");
        copy_relayable(&headers, out);
        // The upstream authority is ours, not the client's.
        out.remove(HOST);
        // The broker answers plain bytes: several routes parse or rewrite its body before it
        // leaves, and the gateway's own compression layer packs the answer for the client
        // (T-5847).
        out.remove(ACCEPT_ENCODING);
        // The length is the body's that goes up, which a route may have rewritten: the MCP hop
        // sends its message re-serialised, and a client's spaced JSON then promised the broker
        // bytes that never came (T-4337). hyper writes it from the body itself.
        out.remove(CONTENT_LENGTH);

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
/// `gcr.io/distroless/cc-debian13`, which ships `ca-certificates`.
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
pub fn with_body(
    mut parts: axum::http::response::Parts,
    bytes: impl Into<axum::body::Bytes>,
) -> Response<Body> {
    let bytes = bytes.into();
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

    #[tokio::test]
    async fn with_body_keeps_the_bytes_and_sets_content_length() {
        let held = axum::body::Bytes::from_static(b"{\"id\":\"urn:ngsi-ld:T:1\"}");
        let (mut parts, _) = Response::new(Body::empty()).into_parts();
        parts
            .headers
            .insert(CONTENT_LENGTH, HeaderValue::from_static("999"));
        parts
            .headers
            .insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));

        let answer = with_body(parts, held.clone());
        assert_eq!(
            answer
                .headers()
                .get(CONTENT_LENGTH)
                .expect("length")
                .to_str()
                .expect("ascii"),
            held.len().to_string()
        );
        assert!(answer.headers().get(TRANSFER_ENCODING).is_none());
        let sent = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(sent, held);

        let (mut parts, _) = Response::new(Body::empty()).into_parts();
        parts.status = StatusCode::NO_CONTENT;
        let empty = with_body(parts, Vec::new());
        assert_eq!(empty.status(), StatusCode::NO_CONTENT);
        assert_eq!(empty.headers().get(CONTENT_LENGTH).expect("length"), "0");
        let sent = axum::body::to_bytes(empty.into_body(), usize::MAX)
            .await
            .expect("body");
        assert!(sent.is_empty());
    }
}
