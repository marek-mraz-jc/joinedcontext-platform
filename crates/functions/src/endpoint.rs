//! The one host capability of a function: a request to its application's endpoint through the
//! Context Gateway, with the caller's token (SDK-22, GW10).
//!
//! The function's code decides the method and path, so both are checked here, not in the SDK:
//! the path must stay under `/api/endpoint/{slug}/` of the invocation, and every segment must
//! decode to the characters of an entity id, because the gateway decodes before it normalises and
//! `..%2F` would otherwise walk out of the endpoint with the caller's token.

use serde_json::{json, Value};

/// The most a gateway answer may be before the function sees it; the function's own memory limit
/// is lower, this only keeps the runtime from buffering an unbounded body for it.
pub const ANSWER_LIMIT: usize = 8 * 1024 * 1024;

/// Where a function's requests go and whose grants they carry.
#[derive(Clone)]
pub struct Endpoint {
    pub http: reqwest::Client,
    /// Scheme and authority of the gateway, from the runtime's own configuration.
    pub gateway: String,
    pub slug: String,
    /// The caller's access token; `None` calls the endpoint anonymously.
    pub token: Option<String>,
}

/// An endpoint slug as the Portal mints them (EP-02): base32, 26 to 32 characters.
pub fn is_slug(slug: &str) -> bool {
    (26..=32).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

fn id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b":._~-".contains(&b)
}

/// A segment that is one segment to every reader: percent-decoded once, only entity-id characters
/// (so no `/`, `\`, `%` of a double encoding) and not made of dots alone.
fn plain_segment(segment: &str) -> bool {
    let raw = segment.as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            let Some(byte) = raw
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            else {
                return false;
            };
            decoded.push(byte);
            i += 3;
        } else if id_byte(raw[i]) {
            decoded.push(raw[i]);
            i += 1;
        } else {
            return false;
        }
    }
    !decoded.is_empty()
        && decoded.iter().all(|&b| id_byte(b))
        && !decoded.iter().all(|&b| b == b'.')
}

/// The path is under the endpoint and reads the same to the gateway as it does here.
pub fn allowed(slug: &str, path: &str) -> bool {
    let (pathname, query) = path.split_once('?').unwrap_or((path, ""));
    let Some(rest) = pathname.strip_prefix(&format!("/api/endpoint/{slug}/")) else {
        return false;
    };
    is_slug(slug)
        && rest.split('/').all(plain_segment)
        && query
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"#\"<>\\^`{|}".contains(&b))
}

fn problem(status: u16, title: &str) -> Value {
    json!({ "status": status, "body": { "title": title, "status": status } })
}

/// Performs one request of the function and answers `{status, body}`: the endpoint's answer, a
/// 403 for a request this runtime does not send, or status 0 when the gateway did not answer.
pub async fn request(endpoint: &Endpoint, method: &str, path: &str, body: Option<String>) -> Value {
    let method = match method {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PATCH" => reqwest::Method::PATCH,
        "DELETE" => reqwest::Method::DELETE,
        _ => return problem(403, "a function may send GET, POST, PATCH or DELETE only"),
    };
    if !allowed(&endpoint.slug, path) {
        return problem(403, "a function may call its own endpoint only");
    }
    let mut builder = endpoint
        .http
        .request(method.clone(), format!("{}{path}", endpoint.gateway))
        .header(reqwest::header::ACCEPT, "application/json");
    if let Some(token) = &endpoint.token {
        builder = builder.bearer_auth(token);
    }
    if let (Some(body), true) = (
        body,
        method == reqwest::Method::POST || method == reqwest::Method::PATCH,
    ) {
        builder = builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let mut response = match builder.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "the gateway did not answer a function");
            return json!({ "status": 0, "body": { "title": "the gateway did not answer" } });
        }
    };
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= ANSWER_LIMIT => {
                bytes.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => return problem(502, "the endpoint answered more than 8 MiB"),
            Ok(None) => break,
            Err(_) => {
                return json!({ "status": 0, "body": { "title": "the gateway answer was cut off" } })
            }
        }
    }
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    json!({ "status": status, "body": body })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLUG: &str = "k7m2qz4tv6xh3n5jb2ryd3wcfa";

    #[test]
    fn the_sdk_paths_are_allowed() {
        let id = "urn%3Angsi-ld%3AA%3Ahel.fi%3Ah%3A1";
        for path in [
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=A&limit=100&offset=0"),
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{id}/attrs"),
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/temporal/entities?type=A&timerel=after&timeAt=2026-09-01T00%3A00%3A00Z"),
            format!("/api/endpoint/{SLUG}/schema/v2/json-schema"),
            format!("/api/endpoint/{SLUG}/access"),
        ] {
            assert!(allowed(SLUG, &path), "{path}");
        }
    }

    #[test]
    fn nothing_leaves_the_endpoint() {
        for path in [
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/../../../../v1/projects"),
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/%2e%2e/%2E%2E"),
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/..%2F..%2F..%2F..%2F..%2Fv1/attrs"),
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/a%252Fb"),
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/a%5Cb"),
            format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/a%2"),
            format!("/api/endpoint/{SLUG}//access"),
            format!("/api/endpoint/{SLUG}/access#x"),
            format!("/api/endpoint/{SLUG}/access?a=<script>"),
            format!("/api/endpoint/{SLUG}"),
            "/api/endpoint/other26charslugother26cha/access".to_owned(),
            "/api/v1/projects/helsinki".to_owned(),
            "http://evil.example/api/endpoint/x/access".to_owned(),
        ] {
            assert!(!allowed(SLUG, &path), "{path}");
        }
        assert!(!allowed("../x", "/api/endpoint/../x/access"));
        assert!(!is_slug("bikes"));
    }

    /// T-1193: `is_slug` is not `EndpointSlug::new` with a shorter name.
    ///
    /// It was proposed as a duplicate of the kind's own validator, to be replaced by
    /// `EndpointSlug::new(slug).is_ok()`. The two agree on every slug the Portal mints and
    /// disagree on one thing that matters here: `EndpointSlug` has no upper bound, because a
    /// long slug is only a long identifier in a manifest. In the sandbox the slug arrives in
    /// a URL a function wrote, and the ceiling is the bound on what a function can address.
    /// Collapsing the two would drop it silently, which is why this test names it.
    #[test]
    fn the_sandbox_bounds_a_slug_at_both_ends() {
        let long = "a".repeat(33);
        assert!(
            !is_slug(&long),
            "33 characters is past the sandbox's ceiling"
        );
        assert!(
            jc_core::kinds::EndpointSlug::new(&long).is_ok(),
            "the kind accepts it, which is why the ceiling lives here"
        );
        let minted = "a".repeat(26);
        assert!(is_slug(&minted));
        assert!(jc_core::kinds::EndpointSlug::new(&minted).is_ok());
        for short in ["", "bikes", &"a".repeat(25)] {
            assert!(!is_slug(short), "{short}");
            assert!(jc_core::kinds::EndpointSlug::new(short).is_err(), "{short}");
        }
    }
}
