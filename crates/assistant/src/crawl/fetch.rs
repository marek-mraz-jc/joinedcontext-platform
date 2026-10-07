//! Guarded HTTP fetching with SSRF protection, manual redirects, and size caps (T-3052).

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, LOCATION};
use reqwest::{Client, StatusCode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use url::Url;

use super::CrawlPolicy;

/// Maximum number of manual redirect hops followed.
pub const MAX_REDIRECT_HOPS: usize = 5;

/// Outcome of a guarded GET request.
#[derive(Debug)]
pub enum FetchOutcome {
    /// Request succeeded and returned a complete response body.
    Success {
        /// The final URL reached after redirect hops.
        final_url: Url,
        /// Response HTTP status code.
        status: StatusCode,
        /// Response headers.
        headers: HeaderMap,
        /// Body bytes.
        body: Vec<u8>,
    },
    /// The resource has not been modified (HTTP 304).
    NotModified {
        /// The final URL reached.
        final_url: Url,
    },
    /// A redirect hop left the site's host.
    RedirectExternal {
        /// Target URL of the external redirect.
        target_url: Url,
    },
    /// The URL was refused by SSRF validation or egress DNS check.
    Refused,
    /// The response exceeded the maximum permitted byte cap.
    TooLarge,
    /// The server answered with a status that is neither success, redirect nor 304.
    Status(StatusCode),
    /// A network error occurred.
    Failed(String),
}

/// Public intermediates some sites do not send with their certificate (T-3298): a browser fetches
/// a missing one from the leaf's AIA address, rustls does not. Each is offered to the chain
/// building beside what the server sent; trust still ends at a native root, and an expired or
/// unrelated intermediate builds no chain. Downloaded from cacerts.digicert.com and pinned by
/// SHA-256 in `the_shipped_intermediates_are_the_pinned_ones`; both expire 2027-11-02.
pub const INTERMEDIATES: &[(&str, &[u8])] = &[
    // www.bbsk.sk
    (
        "GeoTrust TLS RSA CA G1",
        include_bytes!("intermediates/GeoTrustTLSRSACAG1.crt"),
    ),
    // www.praha.eu
    (
        "Thawte TLS RSA CA G1",
        include_bytes!("intermediates/ThawteTLSRSACAG1.crt"),
    ),
];

/// A webpki check that also offers `extra` intermediates when it builds the chain.
#[derive(Debug)]
struct WithIntermediates {
    inner: Arc<WebPkiServerVerifier>,
    extra: Vec<CertificateDer<'static>>,
}

impl ServerCertVerifier for WithIntermediates {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut offered = intermediates.to_vec();
        offered.extend(self.extra.iter().cloned());
        self.inner
            .verify_server_cert(end_entity, &offered, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The crawler's TLS: verified to `roots`, with `intermediates` offered beside the server's own.
pub fn tls_config(
    roots: RootCertStore,
    intermediates: Vec<CertificateDer<'static>>,
) -> Result<ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|err| format!("no TLS verifier: {err}"))?;
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| format!("no TLS versions: {err}"))?
        .dangerous() // a custom verifier; it only adds intermediates to webpki's own check
        .with_custom_certificate_verifier(Arc::new(WithIntermediates {
            inner,
            extra: intermediates,
        }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// The production TLS: the native roots and [`INTERMEDIATES`].
fn public_tls() -> Result<ClientConfig, String> {
    let found = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(found.certs);
    if added == 0 {
        return Err(format!(
            "no native root certificate could be read: {:?}",
            found.errors
        ));
    }
    let intermediates = INTERMEDIATES
        .iter()
        .map(|(_, der)| CertificateDer::from(der.to_vec()))
        .collect();
    tls_config(roots, intermediates)
}

/// Builds the crawl client: every name through `resolver` (production: `PublicOnly`), no
/// redirect of its own (each hop passes [`is_url_allowed`] in [`fetch_url`]), no cookies, the
/// platform user agent, and TLS verified to the native roots with [`INTERMEDIATES`] offered. A
/// client that cannot be built is an error, never a default client, which would carry no
/// resolver and so no guard.
pub fn client<R: reqwest::dns::Resolve + 'static>(resolver: Arc<R>) -> Result<Client, String> {
    client_with_tls(resolver, public_tls()?)
}

/// [`client`] with the TLS of `config`, for a test's own roots.
pub fn client_with_tls<R: reqwest::dns::Resolve + 'static>(
    resolver: Arc<R>,
    config: ClientConfig,
) -> Result<Client, String> {
    Client::builder()
        .dns_resolver(resolver)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent("jc-assistant/0.1 (+https://joinedcontext.com/assistant)")
        .use_preconfigured_tls(config)
        .build()
        .map_err(|err| format!("the HTTP client could not be built: {err}"))
}

/// Checks whether a URL satisfies the crawler's SSRF guard rules.
///
/// Refuses user credentials, non-https schemes (unless `allow_plain_http` is true),
/// and IP literals that do not satisfy [`agent_proxy::public_dns::is_public`].
pub fn is_url_allowed(url: &Url, allow_plain_http: bool) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => {}
        "http" if allow_plain_http => {}
        _ => return false,
    }
    match url.host() {
        Some(url::Host::Ipv4(v4)) => {
            if !agent_proxy::public_dns::is_public(std::net::IpAddr::V4(v4)) {
                return false;
            }
        }
        Some(url::Host::Ipv6(v6)) => {
            if !agent_proxy::public_dns::is_public(std::net::IpAddr::V6(v6)) {
                return false;
            }
        }
        Some(url::Host::Domain(_)) => {}
        None => return false,
    }
    true
}

/// Performs a guarded GET request following at most 5 hops.
pub async fn fetch_url(
    client: &Client,
    policy: &CrawlPolicy,
    site_host: &str,
    initial_url: &Url,
    max_bytes: u64,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> FetchOutcome {
    let mut current_url = initial_url.clone();
    let mut hops = 0;

    loop {
        if !is_url_allowed(&current_url, policy.allow_plain_http) {
            return FetchOutcome::Refused;
        }

        let mut req = client.get(current_url.clone());
        if hops == 0 {
            if let Some(et) = etag {
                if let Ok(v) = HeaderValue::from_str(et) {
                    req = req.header(IF_NONE_MATCH, v);
                }
            }
            if let Some(lm) = last_modified {
                if let Ok(v) = HeaderValue::from_str(lm) {
                    req = req.header(IF_MODIFIED_SINCE, v);
                }
            }
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(err) => {
                if is_refused_error(&err) {
                    return FetchOutcome::Refused;
                }
                // The whole chain, so a TLS refusal names the certificate (T-3298).
                let mut why = err.to_string();
                let mut source = std::error::Error::source(&err);
                while let Some(cause) = source {
                    why.push_str(": ");
                    why.push_str(&cause.to_string());
                    source = cause.source();
                }
                tracing::warn!(url = %current_url, %why, "read failed");
                return FetchOutcome::Failed(why);
            }
        };

        if resp.status() == StatusCode::NOT_MODIFIED {
            return FetchOutcome::NotModified {
                final_url: current_url,
            };
        }

        if resp.status().is_redirection() {
            if hops >= MAX_REDIRECT_HOPS {
                return FetchOutcome::Failed("too many redirects (> 5 hops)".to_string());
            }
            let loc_header = match resp.headers().get(LOCATION) {
                Some(h) => h,
                None => {
                    return FetchOutcome::Failed("redirect missing location header".to_string())
                }
            };
            let loc_str = match loc_header.to_str() {
                Ok(s) => s,
                Err(_) => {
                    return FetchOutcome::Failed("invalid location header encoding".to_string())
                }
            };
            let target_url = match current_url.join(loc_str) {
                Ok(u) => u,
                Err(_) => return FetchOutcome::Failed(format!("invalid redirect URL: {loc_str}")),
            };

            if target_url.host_str() != Some(site_host) {
                return FetchOutcome::RedirectExternal { target_url };
            }

            current_url = target_url;
            hops += 1;
            continue;
        }

        if !resp.status().is_success() {
            return FetchOutcome::Status(resp.status());
        }

        if let Some(cl) = resp.content_length() {
            if cl > max_bytes {
                return FetchOutcome::TooLarge;
            }
        }

        let headers = resp.headers().clone();
        let status = resp.status();
        let mut stream = resp.bytes_stream();
        let mut body = Vec::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(err) => return FetchOutcome::Failed(err.to_string()),
            };
            if (body.len() + chunk.len()) as u64 > max_bytes {
                return FetchOutcome::TooLarge;
            }
            body.extend_from_slice(&chunk);
        }

        return FetchOutcome::Success {
            final_url: current_url,
            status,
            headers,
            body,
        };
    }
}

fn is_refused_error(err: &reqwest::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(current) = cause {
        if agent_proxy::public_dns::refused(current).is_some() {
            return true;
        }
        cause = current.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(url: &str) -> bool {
        is_url_allowed(&Url::parse(url).expect("a URL"), false)
    }

    /// T-3227: every address a crawled link or a start URL could aim inside the cluster or at the
    /// node is refused before a request exists; a public https page is not.
    #[test]
    fn no_link_reaches_inside_the_cluster_the_node_or_another_scheme() {
        assert!(allowed("https://www.banskabystrica.sk/odpad"));
        for refused in [
            // Cloud metadata, the node, the cluster's own ranges.
            "https://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1/",
            "https://10.43.0.10/",
            "https://172.16.0.1/",
            "https://192.168.1.1/",
            "https://100.64.0.1/",
            // The same addresses in the forms a parser normalises: decimal, octal, IPv4-mapped.
            "https://2130706433/",
            "https://0177.0.0.1/",
            "https://[::ffff:127.0.0.1]/",
            "https://[::1]/",
            "https://[fd00::1]/",
            // Other schemes, plain http without the policy, and credentials in the URL.
            "file:///etc/passwd",
            "ftp://www.banskabystrica.sk/",
            "gopher://www.banskabystrica.sk/",
            "http://www.banskabystrica.sk/",
            "https://user:secret@www.banskabystrica.sk/",
        ] {
            assert!(!allowed(refused), "{refused} must be refused");
        }
    }
}
