//! Usage per Endpoint and surface for the publishers' Usage tab (T-5118).
//!
//! The surface counts every answered call by the Endpoint slug of its path and the surface its
//! route belongs to, and posts what it counted to the Portal's internal listener every five
//! minutes (`JC_GATEWAY_USAGE_URL`). The Portal adds the counts into one row per Endpoint,
//! surface and day. Each replica sends only its own counts since its last post, so any number of
//! replicas add up to the right total.
//!
//! Each count also carries the kind of caller (T-6186): a person, a workload, an agent or MCP
//! client, or nobody signed in, read from the token the realm signed and never from one it did
//! not. Counts only: no address, token, subject, client id, query or entity id is kept here, so
//! nothing personal leaves the gateway. A refused call (status 400 and above) is no use of the data and
//! is not counted, which also keeps a scanner's invented slugs (404) out of the tally.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

use crate::previews::WorkloadToken;

/// How often the counts are posted.
pub const INTERVAL: Duration = Duration::from_secs(300);
/// Distinct (endpoint, surface, caller) triples one replica keeps between two posts. Endpoints are few;
/// the cap bounds memory if the Portal stays away for days.
const MAX_KEYS: usize = 10_000;

/// One line of a post: what one Endpoint's surface answered since the last post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Count {
    /// The Endpoint's slug, as in `/api/endpoint/{slug}`.
    pub endpoint: String,
    /// One of the names [`surface`] gives.
    pub surface: &'static str,
    /// One of [`CALLERS`].
    pub caller: &'static str,
    /// Answered calls since the last post.
    pub count: u64,
}

#[derive(Debug, Serialize)]
struct Post<'a> {
    counts: &'a [Count],
}

type Tally = HashMap<(String, &'static str, &'static str), u64>;

/// The kinds of caller a count names, as [`caller`] gives them.
pub const CALLERS: &[&str] = &["person", "workload", "agent", "anonymous"];

/// The kind of caller a verified token names; `None`, a token the realm did not sign or none at
/// all, is `anonymous`. An MCP connector's public client (`mcp-*`, PLATFORM_CLIENTS), a client
/// metadata document's URL (CIMD) and the Portal's assistant are agents, whoever signed in through
/// them; a Keycloak service account (`service-account-<client>`) or a token without a username is
/// a workload; any other username is a person.
pub fn caller(claims: Option<&jc_core::auth::token::Claims>) -> &'static str {
    let Some(claims) = claims else {
        return "anonymous";
    };
    let azp = claims.azp.as_deref().unwrap_or_default();
    if azp.starts_with("mcp-") || azp.contains("://") || azp == "jc-assistant" {
        return "agent";
    }
    match claims.preferred_username.as_deref() {
        Some(name) if !name.starts_with("service-account-") => "person",
        _ => "workload",
    }
}

fn tally() -> &'static Mutex<Tally> {
    static TALLY: OnceLock<Mutex<Tally>> = OnceLock::new();
    TALLY.get_or_init(Mutex::default)
}

/// The surface a matched route of `/api/endpoint/{slug}/...` belongs to; `None` for the
/// Endpoint's landing, access, schema and OpenAPI pages, which describe the data rather than
/// serve it.
pub fn surface(route: &str) -> Option<&'static str> {
    let rest = route.strip_prefix("/api/endpoint/{slug}/")?;
    let first = rest.split('/').next().unwrap_or_default();
    Some(match first {
        "ngsi-ld" => "ngsi-ld",
        "odata" => "odata",
        "ogc" => "ogc",
        "sta" => "sta",
        "ical" => "ical",
        "feed" => "feed",
        "mcp" => "mcp",
        "ws" => "websocket",
        "file.csv" => "download",
        _ => return None,
    })
}

/// Counts one answered call by a `caller` of [`CALLERS`].
pub fn count(endpoint: &str, route: &str, status: u16, caller: &'static str) {
    let Some(surface) = surface(route) else {
        return;
    };
    if status >= 400 || endpoint.is_empty() || endpoint == "-" {
        return;
    }
    add(
        &mut tally().lock().unwrap_or_else(|e| e.into_inner()),
        endpoint,
        surface,
        caller,
        1,
    );
}

fn add(tally: &mut Tally, endpoint: &str, surface: &'static str, caller: &'static str, n: u64) {
    let key = (endpoint.to_owned(), surface, caller);
    if let Some(held) = tally.get_mut(&key) {
        *held = held.saturating_add(n);
    } else if tally.len() < MAX_KEYS {
        tally.insert(key, n);
    }
}

/// Everything counted since the last take, sorted, and an empty tally behind it.
pub fn take() -> Vec<Count> {
    let taken = std::mem::take(&mut *tally().lock().unwrap_or_else(|e| e.into_inner()));
    let mut counts: Vec<Count> = taken
        .into_iter()
        .map(|((endpoint, surface, caller), count)| Count {
            endpoint,
            surface,
            caller,
            count,
        })
        .collect();
    counts.sort_by(|a, b| {
        (&a.endpoint, a.surface, a.caller).cmp(&(&b.endpoint, b.surface, b.caller))
    });
    counts
}

/// Puts counts a failed post did not deliver back, so the next post carries them.
pub fn restore(counts: Vec<Count>) {
    let mut tally = tally().lock().unwrap_or_else(|e| e.into_inner());
    for count in counts {
        add(
            &mut tally,
            &count.endpoint,
            count.surface,
            count.caller,
            count.count,
        );
    }
}

/// Posts the counts every [`INTERVAL`] with the gateway's own token (PF-46). A post the Portal
/// did not take is kept for the next one; a count is sent at most once when the Portal answers.
pub async fn follow(url: String, token: Option<Arc<WorkloadToken>>) {
    let built = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build();
    let Ok(client) =
        built.inspect_err(|error| tracing::error!(%error, "no HTTP client for the usage counts"))
    else {
        return;
    };
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        flush(&client, &url, token.as_deref()).await;
    }
}

/// Posts everything counted since the last flush; what the Portal did not take is kept.
async fn flush(client: &reqwest::Client, url: &str, token: Option<&WorkloadToken>) {
    let counts = take();
    if !counts.is_empty() && !post(client, url, token, &counts).await {
        restore(counts);
    }
}

/// One post of `counts`; true when the Portal took them.
async fn post(
    client: &reqwest::Client,
    url: &str,
    token: Option<&WorkloadToken>,
    counts: &[Count],
) -> bool {
    let mut request = client.post(url).json(&Post { counts });
    if let Some(token) = token {
        match token.get().await {
            Ok(bearer) => request = request.bearer_auth(bearer),
            Err(error) => {
                tracing::warn!(%error, "no token for the Portal's usage counts");
                return false;
            }
        }
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => true,
        Ok(response) => {
            tracing::warn!(status = %response.status(), "the Portal did not take the usage counts");
            false
        }
        Err(error) => {
            tracing::warn!(%error, "the Portal did not take the usage counts");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_names_its_surface_and_a_describing_page_none() {
        assert_eq!(
            surface("/api/endpoint/{slug}/ngsi-ld/v1/{*rest}"),
            Some("ngsi-ld")
        );
        assert_eq!(surface("/api/endpoint/{slug}/ogc/features/"), Some("ogc"));
        assert_eq!(surface("/api/endpoint/{slug}/odata/v4"), Some("odata"));
        assert_eq!(
            surface("/api/endpoint/{slug}/sta/v1.1/{*rest}"),
            Some("sta")
        );
        assert_eq!(
            surface("/api/endpoint/{slug}/ical/v1/{*rest}"),
            Some("ical")
        );
        assert_eq!(
            surface("/api/endpoint/{slug}/feed/v1/{*rest}"),
            Some("feed")
        );
        assert_eq!(surface("/api/endpoint/{slug}/mcp"), Some("mcp"));
        assert_eq!(surface("/api/endpoint/{slug}/ws"), Some("websocket"));
        assert_eq!(surface("/api/endpoint/{slug}/file.csv"), Some("download"));
        assert_eq!(surface("/api/endpoint/{slug}/openapi.json"), None);
        assert_eq!(surface("/api/endpoint/{slug}/access"), None);
        assert_eq!(surface("/api/endpoint/{slug}"), None);
        assert_eq!(surface("/cs/{space}/ngsi-ld/v1/{*rest}"), None);
        assert_eq!(surface("unmatched"), None);
    }

    /// One test drives the process-wide tally, so the steps cannot race each other.
    #[tokio::test]
    async fn answered_calls_are_counted_refused_ones_are_not_and_a_failed_post_is_kept() {
        let mine = |counts: Vec<Count>| -> Vec<Count> {
            counts
                .into_iter()
                .filter(|c| c.endpoint == "usage-test")
                .collect()
        };
        let data = "/api/endpoint/{slug}/ngsi-ld/v1/{*rest}";
        count("usage-test", data, 200, "anonymous");
        count("usage-test", data, 200, "anonymous");
        count(
            "usage-test",
            "/api/endpoint/{slug}/file.csv",
            200,
            "anonymous",
        );
        count("usage-test", "/api/endpoint/{slug}/ws", 101, "anonymous");
        count("usage-test", data, 403, "anonymous");
        count("nope", data, 404, "anonymous");
        count("-", data, 200, "anonymous");
        count(
            "usage-test",
            "/api/endpoint/{slug}/openapi.json",
            200,
            "anonymous",
        );
        let counts = mine(take());
        let line = |surface: &'static str, count: u64| Count {
            endpoint: "usage-test".to_owned(),
            surface,
            caller: "anonymous",
            count,
        };
        assert_eq!(
            counts,
            vec![
                line("download", 1),
                line("ngsi-ld", 2),
                line("websocket", 1)
            ]
        );
        assert_eq!(mine(take()), Vec::new());
        count("usage-test", data, 200, "anonymous");
        restore(counts);
        assert_eq!(
            mine(take()),
            vec![
                line("download", 1),
                line("ngsi-ld", 3),
                line("websocket", 1)
            ]
        );

        // A flush the Portal refuses keeps the counts; one it takes empties the tally.
        let app = axum::Router::new()
            .route(
                "/ok",
                axum::routing::post(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .route(
                "/down",
                axum::routing::post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        let client = reqwest::Client::new();
        count("usage-test", data, 200, "anonymous");
        flush(&client, &format!("http://{addr}/down"), None).await;
        flush(&client, "http://127.0.0.1:1/none", None).await;
        count("usage-test", data, 200, "anonymous");
        assert_eq!(mine(take()), vec![line("ngsi-ld", 2)]);
        count("usage-test", data, 200, "anonymous");
        flush(&client, &format!("http://{addr}/ok"), None).await;
        assert_eq!(mine(take()), Vec::new());
    }

    /// Each kind from the claims of a token the realm signed; no token, or one it did not sign,
    /// is anonymous (T-6186).
    #[test]
    fn a_verified_token_names_its_callers_kind() {
        let kind = |claims: serde_json::Value| {
            let mut base = serde_json::json!({"sub": "s", "iss": "i", "exp": 1});
            if let (Some(base), Some(extra)) = (base.as_object_mut(), claims.as_object()) {
                base.extend(extra.clone());
            }
            let claims: jc_core::auth::token::Claims =
                serde_json::from_value(base).expect("claims");
            caller(Some(&claims))
        };
        assert_eq!(caller(None), "anonymous");
        assert_eq!(
            kind(serde_json::json!({"azp": "portal-ui", "preferred_username": "ana"})),
            "person"
        );
        assert_eq!(
            kind(
                serde_json::json!({"azp": "city-pipelines", "preferred_username": "service-account-city-pipelines"})
            ),
            "workload"
        );
        assert_eq!(kind(serde_json::json!({"azp": "edge"})), "workload");
        assert_eq!(
            kind(serde_json::json!({"azp": "mcp-local", "preferred_username": "ana"})),
            "agent"
        );
        assert_eq!(
            kind(
                serde_json::json!({"azp": "https://claude.ai/oauth/mcp-oauth-client-metadata", "preferred_username": "ana"})
            ),
            "agent"
        );
        assert_eq!(kind(serde_json::json!({"azp": "jc-assistant"})), "agent");
        assert!(CALLERS.contains(&kind(serde_json::json!({}))));
    }

    #[test]
    fn the_tally_holds_at_most_its_cap_of_pairs() {
        let mut tally = Tally::new();
        for i in 0..MAX_KEYS + 5 {
            add(&mut tally, &format!("e{i}"), "ngsi-ld", "person", 1);
        }
        assert_eq!(tally.len(), MAX_KEYS);
        add(&mut tally, "e0", "ngsi-ld", "person", 2);
        assert_eq!(tally.get(&("e0".to_owned(), "ngsi-ld", "person")), Some(&3));
    }

    /// What the Portal reads: names, a caller kind and numbers, nothing else.
    #[test]
    fn a_post_carries_endpoint_surface_and_count_only() {
        let counts = [Count {
            endpoint: "air".to_owned(),
            surface: "ogc",
            caller: "agent",
            count: 7,
        }];
        assert_eq!(
            serde_json::to_value(Post { counts: &counts }).unwrap_or_default(),
            serde_json::json!({"counts": [{"endpoint": "air", "surface": "ogc", "caller": "agent", "count": 7}]})
        );
    }

    /// A post the Portal answers 2xx is taken; any other answer or no answer is not.
    #[tokio::test]
    async fn a_post_is_taken_only_on_a_success_answer() {
        use axum::{http::StatusCode, routing::post as route_post, Json, Router};
        let app = Router::new()
            .route(
                "/ok",
                route_post(|Json(body): Json<serde_json::Value>| async move {
                    if body["counts"][0]["count"] == 7 {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::BAD_REQUEST
                    }
                }),
            )
            .route(
                "/down",
                route_post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        let client = reqwest::Client::new();
        let counts = [Count {
            endpoint: "air".to_owned(),
            surface: "ogc",
            caller: "agent",
            count: 7,
        }];
        assert!(post(&client, &format!("http://{addr}/ok"), None, &counts).await);
        assert!(!post(&client, &format!("http://{addr}/down"), None, &counts).await);
        assert!(!post(&client, "http://127.0.0.1:1/none", None, &counts).await);
    }
}
