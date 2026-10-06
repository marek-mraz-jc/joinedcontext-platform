//! Guarded HTTP fetching with SSRF protection, manual redirects, and size caps (T-3052).

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, LOCATION};
use reqwest::{Client, StatusCode};
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

/// Builds the crawl client: every name through `resolver` (production: `PublicOnly`), no
/// redirect of its own (each hop passes [`is_url_allowed`] in [`fetch_url`]), no cookies, and
/// the platform user agent. A client that cannot be built is an error, never a default client,
/// which would carry no resolver and so no guard.
pub fn client<R: reqwest::dns::Resolve + 'static>(
    resolver: Arc<R>,
) -> Result<Client, reqwest::Error> {
    Client::builder()
        .dns_resolver(resolver)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent("jc-assistant/0.1 (+https://joinedcontext.com/assistant)")
        .build()
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
                return FetchOutcome::Failed(err.to_string());
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
