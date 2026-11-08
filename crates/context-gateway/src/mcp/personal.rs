//! A person's own connectors, `/api/mcp/me/{name}` (EP-123, ADR-N-043 §7.7, API/14, T-5970).
//!
//! The Portal keeps every connector in one store, the connector record of API/14. The gateway
//! reads a person's data-reading ones by the token's `sub` from the Portal's internal listener,
//! `GET /internal/connectors/{sub}`, with its own workload token (audience `portal-internal`,
//! API/01 §8), holds an answer at most a minute, and serves the named one as an organization
//! server with no members whose Endpoints are the ones the connector's scope names: the same
//! reach rules as every named server, so it only narrows, and it reads only (writes arrive with
//! plan_change, T-5956). An expired connector, or a client the connector does not allow, is
//! refused. Another person's token at that address finds no connector of that name and gets the
//! bytes of an unknown server.

use crate::app::Gateway;
use crate::auth::subject::EDGE_AUDIENCE;
use crate::auth::token;
use crate::mcp::server::{challenge, serve, McpServer};
use crate::previews::WorkloadToken;
use crate::resolver::Endpoint;
use axum::body::Body;
use axum::extract::Request;
use axum::http::header::AUTHORIZATION;
use axum::http::Response;
use axum::response::IntoResponse;
use jc_core::kinds::mcp_server::Select;
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

/// One connector record as the Portal hands it to the gateway (API/14 §1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Connector {
    /// The record's id, `/c/{id}` on the Portal.
    pub id: String,
    /// A DNS label, unique among the person's live connectors, the last segment of its address.
    pub name: String,
    /// What it reaches: each item a project, narrowed to one space, Endpoint or pipeline.
    pub scope: Vec<ScopeItem>,
    /// RFC 3339; past it the connector answers nothing.
    pub expires_at: String,
    /// The OAuth clients (`azp`) it answers; none listed answers any.
    #[serde(default)]
    pub allowed_clients: Vec<String>,
}

/// One scope item of a connector (API/14 §1); `*` names every one of its kind.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeItem {
    /// A project slug.
    pub project: String,
    /// A Context Space of the project by name, or `*`.
    #[serde(default)]
    pub space: Option<String>,
    /// An Endpoint of the project by manifest name, or `*`.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// A pipeline of the project by name, or `*`: it reaches no Endpoint here.
    #[serde(default)]
    pub pipeline: Option<String>,
    /// `read` or `write`; this door reads only whichever it says.
    pub access: String,
    /// Whether the person ticked personal data for this item (AG-158). Without it an Endpoint
    /// that serves an attribute its model marks personal is left out of the connector.
    #[serde(default, rename = "personalData")]
    pub personal_data: bool,
}

impl ScopeItem {
    /// Whether this item reaches `endpoint`: same project, and its space or its name when the
    /// item names one. An item narrowed to a pipeline reaches no Endpoint.
    fn reaches(&self, endpoint: &Endpoint) -> bool {
        let named = |wanted: &Option<String>, has: &str| {
            wanted.as_deref().is_none_or(|w| w == "*" || w == has)
        };
        self.project == endpoint.project
            && self.pipeline.is_none()
            && named(&self.space, &endpoint.space)
            && named(&self.endpoint, &endpoint.manifest_name)
    }
}

impl Connector {
    /// Whether `now` (seconds since the epoch) lies past its expiry; an unreadable one is past.
    fn expired(&self, now: i64) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.expires_at)
            .map_or(true, |at| at.timestamp() <= now)
    }

    /// Whether a token obtained by `azp` may use it.
    fn admits(&self, azp: Option<&str>) -> bool {
        self.allowed_clients.is_empty()
            || azp.is_some_and(|azp| self.allowed_clients.iter().any(|c| c == azp))
    }
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
    /// Reads from `url` (`…/internal/connectors`), as `token` when the gateway has one (the
    /// Portal refuses a call without it), holding each list `keep`, never longer than [`KEEP`].
    pub fn new(url: String, token: Option<Arc<WorkloadToken>>, keep: Duration) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            token,
            // No redirect: the workload token goes to the Portal and nowhere it points.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client with a timeout builds"),
            keep: keep.min(KEEP),
            held: tokio::sync::Mutex::new(HashMap::new()),
        }
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
        knowledge_sources: Vec::new(),
    })
}

/// The personal attributes `endpoint` serves, `Err` when its model's schema cannot be read
/// (AG-158: such an answer is refused). An Endpoint of a space with no model serves none.
fn personal_of(endpoint: &Endpoint) -> Result<Vec<String>, ()> {
    match &endpoint.declared_types {
        Some(declared) if declared.schema.is_none() => Err(()),
        _ => Ok(endpoint.personal_attributes()),
    }
}

/// `connector` as the server it is served as: every Endpoint of `endpoints` its scope reaches.
/// An Endpoint serving personal attributes is reached only by an item that ticks personal data
/// (AG-158, as `/c/{id}` withholds them); the description names each one left out and why,
/// never a value.
fn server_of(connector: &Connector, endpoints: &[Arc<Endpoint>]) -> Option<McpServer> {
    let mut server = addressed(&connector.name)?;
    let mut kept = Vec::new();
    let mut left_out = Vec::new();
    for endpoint in endpoints {
        let reaching: Vec<&ScopeItem> = connector
            .scope
            .iter()
            .filter(|item| item.reaches(endpoint))
            .collect();
        if reaching.is_empty() {
            continue;
        }
        if reaching.iter().any(|item| item.personal_data) {
            kept.push(endpoint.slug.clone());
            continue;
        }
        match personal_of(endpoint) {
            Ok(personal) if personal.is_empty() => kept.push(endpoint.slug.clone()),
            Ok(personal) => left_out.push(format!(
                "{} (personal data: {})",
                endpoint.slug,
                personal.join(", ")
            )),
            Err(()) => left_out.push(format!("{} (its model cannot be read)", endpoint.slug)),
        }
    }
    if !left_out.is_empty() {
        let description = server.description.take().unwrap_or_default();
        server.description = Some(format!(
            "{description} Left out because this connector does not reach personal data: {}.",
            left_out.join("; ")
        ));
    }
    server.endpoints = Some(kept);
    Some(server)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
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
            return ProblemDetails::new(503, "service-unavailable", "Service Unavailable")
                .with_detail("your connectors cannot be read right now; try again in a minute")
                .into_response();
        }
    };
    let Some(connector) = connectors.iter().find(|connector| connector.name == name) else {
        return ProblemDetails::not_found().into_response();
    };
    if connector.expired(now()) {
        return ProblemDetails::forbidden()
            .with_detail(format!(
                "connector {name} expired at {}; renew it under My connectors in the Portal",
                connector.expires_at
            ))
            .into_response();
    }
    if !connector.admits(claims.azp.as_deref()) {
        return ProblemDetails::forbidden()
            .with_detail(format!(
                "connector {name} answers only {}; this token was issued to {}",
                connector.allowed_clients.join(", "),
                claims.azp.as_deref().unwrap_or("no client")
            ))
            .into_response();
    }
    match server_of(connector, &gateway.resolver.endpoints()) {
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

    fn connector(value: serde_json::Value) -> Connector {
        serde_json::from_value(value).expect("a connector")
    }

    fn endpoint(slug: &str, project: &str, space: &str, name: &str) -> Arc<Endpoint> {
        let mut e = crate::mcp::catalog::tests::fixture();
        e.slug = slug.to_owned();
        e.project = project.to_owned();
        e.space = space.to_owned();
        e.manifest_name = name.to_owned();
        Arc::new(e)
    }

    #[test]
    fn a_connector_is_served_as_an_organization_server_of_what_its_scope_reaches() {
        let endpoints = [
            endpoint("aaaa", "doprava", "transit", "buses"),
            endpoint("bbbb", "doprava", "parking", "lots"),
            endpoint("cccc", "ovzdusie", "air", "air"),
            endpoint("dddd", "skoly", "schools", "schools"),
        ];
        let c = connector(serde_json::json!({
            "id": "x", "name": "my-air", "expiresAt": "2999-01-01T00:00:00Z",
            "scope": [
                { "project": "doprava", "space": "transit", "access": "read" },
                { "project": "ovzdusie", "access": "read" },
                { "project": "skoly", "pipeline": "*", "access": "read" },
            ],
        }));
        let server = server_of(&c, &endpoints).expect("a server");
        assert_eq!(server.path(), "/api/mcp/me/my-air");
        assert_eq!(server.audience, Audience::Organization);
        assert!(server.members.is_empty());
        assert_eq!(server.select, Some(Select::default()));
        assert_eq!(
            server.endpoints.as_deref(),
            Some(&["aaaa".to_owned(), "cccc".to_owned()][..])
        );
        let named = connector(serde_json::json!({
            "id": "x", "name": "one", "expiresAt": "2999-01-01T00:00:00Z",
            "scope": [{ "project": "doprava", "endpoint": "lots", "access": "read" }],
        }));
        assert_eq!(
            server_of(&named, &endpoints).and_then(|s| s.endpoints),
            Some(vec!["bbbb".to_owned()])
        );
        assert!(addressed("My Air").is_none());
        let old = serde_json::from_value::<Connector>(serde_json::json!({
            "name": "x", "select": { "endpoints": ["aaaa"] }
        }));
        assert!(old.is_err(), "the old selector shape names no connector");
    }

    #[test]
    fn an_expired_connector_or_another_client_is_refused() {
        let c = connector(serde_json::json!({
            "id": "x", "name": "n", "expiresAt": "2026-10-28T12:00:00Z", "scope": [],
            "allowedClients": ["mcp-local"],
        }));
        let noon = chrono::DateTime::parse_from_rfc3339("2026-10-28T12:00:00Z")
            .expect("a time")
            .timestamp();
        assert!(!c.expired(noon - 1));
        assert!(c.expired(noon));
        assert!(connector(serde_json::json!({
            "id": "x", "name": "n", "expiresAt": "soon", "scope": [],
        }))
        .expired(0));
        assert!(c.admits(Some("mcp-local")));
        assert!(!c.admits(Some("claude-ai")));
        assert!(!c.admits(None));
        let any = Connector {
            allowed_clients: Vec::new(),
            ..c
        };
        assert!(any.admits(None));
    }

    fn trips(slug: &str, hidden: &[&str], readable: bool) -> Arc<Endpoint> {
        let schema = jc_manifest::model_schema::ModelSchema::compile(
            "bike-trips",
            "1.0.0",
            &serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "definitions": { "BikeTrip": { "type": "object", "properties": {
                    "id": { "type": "string" },
                    "rider": { "type": "string", "x-personal-data": true },
                    "startStation": { "type": "string" }
                } } }
            }),
            &["BikeTrip".to_owned()],
            true,
        );
        let mut e = (*endpoint(slug, "doprava", "trips", slug)).clone();
        e.declared_types = Some(Arc::new(crate::resolver::DeclaredTypes {
            model: "bike-trips".into(),
            classes: ["BikeTrip".to_owned()].into(),
            units: Default::default(),
            relationships: Default::default(),
            quality: Vec::new(),
            schema: readable.then(|| Arc::new(schema)),
        }));
        e.hidden_attributes = hidden.iter().map(|h| (*h).to_owned()).collect();
        Arc::new(e)
    }

    /// AG-158 (F-07): without a personal-data tick the personal door leaves out an Endpoint that
    /// serves a personal attribute, or whose model cannot be read, and names it without a value.
    #[test]
    fn personal_data_is_withheld_unless_the_scope_ticks_it() {
        let endpoints = [
            trips("riders", &[], true),
            trips("hidden", &["rider"], true),
            trips("unread", &[], false),
        ];
        let plain = connector(serde_json::json!({
            "id": "x", "name": "trips", "expiresAt": "2999-01-01T00:00:00Z",
            "scope": [{ "project": "doprava", "access": "read" }],
        }));
        let server = server_of(&plain, &endpoints).expect("a server");
        assert_eq!(server.endpoints, Some(vec!["hidden".to_owned()]));
        let said = server.description.expect("a description");
        assert!(
            said.contains("riders (personal data: BikeTrip.rider)"),
            "{said}"
        );
        assert!(said.contains("unread (its model cannot be read)"), "{said}");
        let ticked = connector(serde_json::json!({
            "id": "x", "name": "trips", "expiresAt": "2999-01-01T00:00:00Z",
            "scope": [{ "project": "doprava", "access": "read", "personalData": true }],
        }));
        assert_eq!(
            server_of(&ticked, &endpoints).and_then(|s| s.endpoints),
            Some(vec![
                "riders".to_owned(),
                "hidden".to_owned(),
                "unread".to_owned()
            ])
        );
    }
}
