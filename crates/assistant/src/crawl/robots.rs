//! robots.txt fetching and rule enforcement (T-3052, RFC 9309).

use reqwest::Client;
use url::Url;

use super::fetch::{fetch_url, is_url_allowed, FetchOutcome};
use super::CrawlPolicy;

/// The token identifying this platform in robots.txt matching.
pub const USER_AGENT_TOKEN: &str = "jc-assistant";

/// Evaluated robots rules for a host.
pub enum Robots {
    /// Host explicitly allowed or answered with HTTP 4xx.
    AllowAll,
    /// Host answered with HTTP 5xx, timed out, or was refused (RFC 9309 §2.3.1.4).
    DisallowAll,
    /// Parsed robot rules from an HTTP 2xx response.
    Rules(texting_robots::Robot),
}

impl Robots {
    /// Returns whether the specified URL or path is allowed.
    pub fn is_allowed(&self, url_str: &str) -> bool {
        match self {
            Robots::AllowAll => true,
            Robots::DisallowAll => false,
            Robots::Rules(robot) => {
                let path = Url::parse(url_str).ok().map(|u| {
                    let mut p = u.path().to_owned();
                    if let Some(q) = u.query() {
                        p.push('?');
                        p.push_str(q);
                    }
                    p
                });
                if !robot.allowed(url_str) {
                    return false;
                }
                if let Some(ref p) = path {
                    if !robot.allowed(p) {
                        return false;
                    }
                }
                true
            }
        }
    }
}

/// Extracts `Sitemap:` lines from robots.txt content.
pub fn extract_sitemaps(content: &str) -> Vec<String> {
    let mut sitemaps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        // `get`, not indexing: robots.txt is the site's text, and a multi-byte character at
        // byte 8 would make a slice panic.
        if trimmed
            .get(..8)
            .is_some_and(|head| head.eq_ignore_ascii_case("sitemap:"))
        {
            let target = trimmed.get(8..).unwrap_or_default().trim();
            if !target.is_empty() {
                sitemaps.push(target.to_string());
            }
        }
    }
    sitemaps
}

/// Fetches and parses robots.txt for a host.
pub async fn fetch_robots(
    client: &Client,
    policy: &CrawlPolicy,
    site_host: &str,
    robots_url_str: &str,
) -> (Robots, Vec<String>) {
    let robots_url = match Url::parse(robots_url_str) {
        Ok(u) => u,
        Err(_) => return (Robots::DisallowAll, Vec::new()),
    };

    if !is_url_allowed(&robots_url, policy.allow_plain_http) {
        return (Robots::DisallowAll, Vec::new());
    }

    let outcome = fetch_url(
        client,
        policy,
        site_host,
        &robots_url,
        policy.max_page_bytes,
        None,
        None,
    )
    .await;

    match outcome {
        FetchOutcome::Success { status, body, .. } => {
            if status.is_success() {
                let text = String::from_utf8_lossy(&body);
                let sitemaps = extract_sitemaps(&text);
                // A robots.txt that cannot be read at all is treated as a refusal: the site
                // said something, and guessing it meant "everything" is not ours to do.
                match texting_robots::Robot::new(USER_AGENT_TOKEN, &body) {
                    Ok(robot) => (Robots::Rules(robot), sitemaps),
                    Err(_) => (Robots::DisallowAll, Vec::new()),
                }
            } else if status.is_client_error() {
                (Robots::AllowAll, Vec::new())
            } else {
                (Robots::DisallowAll, Vec::new())
            }
        }
        // RFC 9309 §2.3.1.3: a 4xx means there are no rules for this host.
        FetchOutcome::Status(status) if status.is_client_error() => (Robots::AllowAll, Vec::new()),
        _ => (Robots::DisallowAll, Vec::new()),
    }
}
