//! A person's own connectors, `/api/mcp/me/{name}` (EP-123, UI-98, ADR-N-043 §7.7).
//!
//! The Portal keeps each person's connectors in their preferences, never in Git. The gateway
//! reads them by the token's `sub` from the Portal's internal listener, `GET
//! /internal/connectors/{sub}`, with its own workload token (audience `portal-internal`, API/01
//! §8), holds an answer at most a minute, and serves the named one as an organization server
//! with no members and the connector's selector: the same reach rules as every named server, so
//! it only narrows. Another person's token at that address finds no connector of that name and
//! gets the bytes of an unknown server.

use crate::app::Gateway;
use crate::auth::subject::EDGE_AUDIENCE;
use crate::auth::token;
use crate::mcp::server::{challenge, serve, McpServer};
use crate::previews::WorkloadToken;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::AUTHORIZATION;
use axum::http::Response;
use axum::response::IntoResponse;
use jc_core::kinds::catalog::DataTheme;
use jc_core::kinds::mcp_server::{AccessLevel, Select};
use jc_core::kinds::Audience;
use jc_core::ProblemDetails;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The first segment of a personal address, `/api/mcp/me/{name}`.
pub const PROJECT: &str = "me";

/// How long one person's list is held (EP-123).
pub const KEEP: Duration = Duration::from_secs(60);

/// The most people whose lists are held at once; past it the held lists are dropped and read
/// again, so a flood of tokens cannot grow the gateway's memory.
const MAX_HELD: usize = 10_000;

/// One connector as the Portal stores it (API/01 §8).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connector {
    /// A DNS label, unique in the person's list, the last segment of its address.
    pub name: String,
    /// What it selects; nothing given selects every Endpoint the person reads.
    #[serde(default)]
    pub select: PersonalSelect,
}

/// An McpServer's selector plus the person's favourite Endpoints (MF-55, UI-98).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PersonalSelect {
    /// Project names.
    pub projects: Option<Vec<String>>,
    /// Context Spaces as `{project}/{space}`.
    pub spaces: Option<Vec<String>>,
    /// Smart Data Models domains of the Endpoint's classification.
    pub domains: Option<Vec<String>>,
    /// EU data themes.
    pub themes: Option<Vec<DataTheme>>,
    /// How the person reaches the Endpoint.
    pub access_level: Option<Vec<AccessLevel>>,
    /// The person's favourite Endpoints by slug, up to 50.
    pub endpoints: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Listed {
    connectors: Vec<Connector>,
}

/// One person's list and when it was read.
type Held = (Instant, Arc<Vec<Connector>>);

/// The Portal's lists, read by `sub` and held for [`KEEP`].
pub struct Personal {
    url: String,
    token: Option<Arc<WorkloadToken>>,
    http: reqwest::Client,
    keep: Duration,
    held: tokio::sync::Mutex<HashMap<String, Held>>,
}

impl Personal {
    /// Reads from `url` (`…/internal/connectors`), as `token` when the gateway has one; the
    /// Portal refuses a call without it.
    pub fn new(url: String, token: Option<Arc<WorkloadToken>>) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            token,
            // No redirect: the workload token goes to the Portal and nowhere it points.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client with a timeout builds"),
            keep: KEEP,
            held: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Holds a list `keep` instead of [`KEEP`]; never longer than a minute.
    pub fn keeping(mut self, keep: Duration) -> Self {
        self.keep = keep.min(KEEP);
        self
    }

    /// `sub`'s connectors: the held list while it is younger than the hold, else the Portal's
    /// answer now. `Err` when the Portal cannot be read: never an older list in its place.
    pub async fn of(&self, sub: &str) -> Result<Arc<Vec<Connector>>, String> {
        if !plain(sub) {
            return Ok(Arc::new(Vec::new()));
        }
        let now = Instant::now();
        {
            let mut held = self.held.lock().await;
            match held.get(sub) {
                Some((at, list)) if now.duration_since(*at) < self.keep => {
                    return Ok(Arc::clone(list))
                }
                Some(_) => {
                    held.remove(sub);
                }
                None => {}
            }
        }
        let mut request = self.http.get(format!("{}/{sub}", self.url));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.get().await?);
        }
        let answer = request
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        if !answer.status().is_success() {
            return Err(format!("the Portal answered {}", answer.status()));
        }
        let listed: Listed = answer
            .json()
            .await
            .map_err(|error| error.without_url().to_string())?;
        let list = Arc::new(listed.connectors);
        let mut held = self.held.lock().await;
        if held.len() >= MAX_HELD {
            held.clear();
        }
        held.insert(sub.to_owned(), (now, Arc::clone(&list)));
        Ok(list)
    }
}

/// A `sub` that is one path segment of letters, digits and `-_.:@`; anything else names nobody.
fn plain(sub: &str) -> bool {
    (1..=255).contains(&sub.len())
        && sub
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:@".contains(c))
}

/// The server a personal address names before whose it is: its path, client and audience, no
/// reach. `None` for a name that is no DNS label.
pub fn addressed(name: &str) -> Option<McpServer> {
    jc_core::names::validate_dns1123_label(name).ok()?;
    Some(McpServer {
        project: PROJECT.to_owned(),
        name: name.to_owned(),
        title: Some(format!("My connector {name}")),
        description: Some(
            "Your own connector: the Endpoints you chose in the Portal, as far as you may read \
             them."
                .to_owned(),
        ),
        members: Vec::new(),
        select: Some(Select::default()),
        endpoints: None,
        audience: Audience::Organization,
        allowed_projects: Vec::new(),
    })
}

/// `connector` as the server it is served as.
fn server_of(connector: &Connector) -> Option<McpServer> {
    let mut server = addressed(&connector.name)?;
    let select = connector.select.clone();
    server.select = Some(Select {
        projects: select.projects,
        spaces: select.spaces,
        domains: select.domains,
        themes: select.themes,
        access_level: select.access_level,
    });
    server.endpoints = select.endpoints;
    Some(server)
}

/// One JSON-RPC message to `/api/mcp/me/{name}` (EP-123).
pub async fn message(gateway: Arc<Gateway>, name: String, request: Request) -> Response<Body> {
    let (Some(personal), Some(address)) = (gateway.personal.clone(), addressed(&name)) else {
        return ProblemDetails::not_found().into_response();
    };
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let raw = match token::bearer(presented) {
        Ok(raw) => raw,
        Err(token::Rejected::NoToken) => return challenge(&gateway, &address),
        Err(rejected) => return ProblemDetails::from(rejected).into_response(),
    };
    let Some(verifier) = gateway.verifier.as_ref() else {
        return ProblemDetails::unauthorized().into_response();
    };
    let mut audiences = gateway.server_audiences(&address);
    audiences.push(EDGE_AUDIENCE.to_owned());
    let claims = match verifier.verify(raw, &audiences) {
        Ok(claims) => claims,
        Err(rejected) => return ProblemDetails::from(rejected).into_response(),
    };
    let connectors = match personal.of(&claims.sub).await {
        Ok(connectors) => connectors,
        Err(why) => {
            tracing::warn!(%why, "a person's connectors could not be read from the Portal");
            return ProblemDetails::new(503, "unavailable", "Service Unavailable")
                .with_detail("your connectors cannot be read right now; try again in a minute")
                .into_response();
        }
    };
    match connectors
        .iter()
        .find(|connector| connector.name == name)
        .and_then(server_of)
    {
        Some(server) => serve(gateway, server, request).await,
        None => ProblemDetails::not_found().into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sub_is_one_plain_segment() {
        assert!(plain("0f5a-9c1e"));
        assert!(plain("service-account-portal@x.y:z"));
        for odd in ["", "a/b", "../x", "a b", "a?b", "ä"] {
            assert!(!plain(odd), "{odd}");
        }
    }

    #[test]
    fn a_connector_is_served_as_an_organization_server_of_its_favourites() {
        let connector: Connector = serde_json::from_value(serde_json::json!({
            "name": "my-air",
            "select": { "themes": ["ENVI"], "endpoints": ["aaaa"] }
        }))
        .expect("a connector");
        let server = server_of(&connector).expect("a server");
        assert_eq!(server.path(), "/api/mcp/me/my-air");
        assert_eq!(server.audience, Audience::Organization);
        assert!(server.members.is_empty());
        assert_eq!(server.endpoints.as_deref(), Some(&["aaaa".to_owned()][..]));
        assert!(addressed("My Air").is_none());
        let unknown = serde_json::from_value::<Connector>(serde_json::json!({
            "name": "x", "select": { "everything": true }
        }));
        assert!(unknown.is_err());
    }
}
