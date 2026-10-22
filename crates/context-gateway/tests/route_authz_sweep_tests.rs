//! Every route of the gateway, called as each kind of caller (T-3858, found by T-3832).
//!
//! The router's routes are read from the source of `app::router` and of every surface module it
//! merges (T-4472), so a route added without a row here is red. Each row is one concrete request on a project-bound Endpoint (its space and its named MCP
//! server), sent as four callers: nobody, a person of another project, a member of the project
//! the Policy grants nothing, and a member it grants `Device`. The broker behind it is the
//! narrowing stub, whose view serves the one secret entity to the granted role alone.
//!
//! Security: no route answers the secret to a caller the Endpoint's Policy does not admit, the
//! anonymous caller is a `401` wherever a token is needed, and the stranger and the ungranted
//! member are refused (`403` or `404`) wherever the route serves the Endpoint's data.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use common::{BrokerStub, View};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::auth::subject::{
    EDGE_AUDIENCE, HUB_AUDIENCE, HUB_PATH, PORTAL_CLIENT, SIMULATE_AUDIENCE,
};
use context_gateway::handlers::mappings::{MappingFile, PublishedMapping};
use context_gateway::mcp::server::McpServer;
use context_gateway::oauth::Issuer;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://gw.example.fi";
const SLUG: &str = "sweep7k2mzt6vhx3nbwrs5cjd8";
const SPACE: &str = "alerts";
const PROJECT: &str = "helsinki";
const SERVER: &str = "ops";
/// What only the granted role may ever read, on any route.
const SECRET: &str = "sweep-secret-7f3a";
const SPACE_ROW: &str = "/cs/{space}";

const GRANT: &str = r#"contextSpaceRef: alerts
assigner: did:web:hel.fi
assignee: { kind: role, id: reader }
operations: [queryEntity, retrieveEntity, retrieveTemporal, queryTemporal, createEntity, updateAttrs, upsertBatch, createSubscription]
information:
  - entities:
      - type: Device
"#;

/// The callers of every row: four people and the Portal's own account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caller {
    Anonymous,
    Stranger,
    Ungranted,
    Granted,
    Portal,
}

const CALLERS: [Caller; 5] = [
    Caller::Anonymous,
    Caller::Stranger,
    Caller::Ungranted,
    Caller::Granted,
    Caller::Portal,
];

/// What a row promises. Every row, whatever its guard, answers the secret to the granted
/// member alone.
#[derive(Debug, Clone, Copy)]
enum Guard {
    /// The Endpoint's data: refused to everyone but the granted member, who is served the secret.
    Data,
    /// About the Endpoint: refused to nobody and the stranger; a member is not refused for
    /// lack of a login, though the broker's engine may refuse the ungranted one.
    Member,
    /// The Portal's own simulator (EP-103): refused to every person.
    Portal,
    /// Answers everyone on purpose (the reason) and names the project-bound Endpoint and its
    /// space to its members alone.
    Listing(&'static str),
    /// Answers everyone on purpose (the reason).
    Open(&'static str),
    /// The anonymous OAuth server (ADR-N-052, T-4631): served to every caller alike, whatever
    /// token it carries, never a server error (the reason).
    Oauth(&'static str),
    /// Lists public Endpoints only (the reason): with none but the project-bound one, every
    /// caller, its granted member included, gets the same 404.
    Withheld(&'static str),
}

const DATA: Guard = Guard::Data;

struct Row {
    method: &'static str,
    route: &'static str,
    /// The concrete request: method, path and query, body.
    send: (Method, String, Option<Value>),
    guard: Guard,
}

fn rows() -> Vec<Row> {
    let ep = |rest: &str| format!("/api/endpoint/{SLUG}{rest}");
    let cs = |rest: &str| format!("/cs/{SPACE}{rest}");
    let get = |path: String| (Method::GET, path, None);
    let post = |path: String, body: Value| (Method::POST, path, Some(body));
    let call = |name: &str, arguments: Value| {
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}})
    };
    let query = call("query_entities", json!({"type": "Device"}));
    let hub_query = call(
        "query_entities",
        json!({"endpoint": SLUG, "type": "Device"}),
    );
    let check = json!({"operation": "queryEntity", "type": "Device"});
    use Guard::*;
    let row = |method, route, send, guard| Row {
        method,
        route,
        send,
        guard,
    };
    vec![
        row("GET", "/api/endpoint/{slug}", get(ep("")), Member),
        row("GET", "/api/endpoint/{slug}/", get(ep("/")), Member),
        row(
            "GET",
            "/api/endpoint/{slug}/ngsi-ld/v1",
            get(ep("/ngsi-ld/v1")),
            Member,
        ),
        row(
            "GET",
            "/api/endpoint/{slug}/ngsi-ld/v1/",
            get(ep("/ngsi-ld/v1/")),
            Member,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/ngsi-ld/v1/{*rest}",
            get(ep("/ngsi-ld/v1/entities?type=Device")),
            DATA,
        ),
        row(
            "GET",
            "/api/endpoint/{slug}/access",
            get(ep("/access")),
            Member,
        ),
        row(
            "POST",
            "/api/endpoint/{slug}/mcp",
            post(ep("/mcp"), query.clone()),
            DATA,
        ),
        row(
            "POST",
            HUB_PATH,
            post(HUB_PATH.to_owned(), hub_query.clone()),
            DATA,
        ),
        row(
            "GET",
            "/api/endpoints",
            get("/api/endpoints".to_owned()),
            Listing("the Endpoints the caller reaches, the public ones for nobody (EP-87)"),
        ),
        row(
            "GET",
            "/api/mcp/.well-known/oauth-protected-resource",
            get("/api/mcp/.well-known/oauth-protected-resource".to_owned()),
            Open("RFC 9728 metadata: the resource URL and the realm, for every caller"),
        ),
        row(
            "GET",
            "/.well-known/oauth-protected-resource/api/mcp",
            get("/.well-known/oauth-protected-resource/api/mcp".to_owned()),
            Open("RFC 9728 metadata at the RFC's own place (T-4631), for every caller"),
        ),
        row(
            "GET",
            "/.well-known/oauth-protected-resource/api/mcp/{project}/{name}",
            get(format!(
                "/.well-known/oauth-protected-resource/api/mcp/{PROJECT}/{SERVER}"
            )),
            Open("RFC 9728 metadata at the RFC's own place (T-4631), for every caller"),
        ),
        row(
            "GET",
            "/.well-known/oauth-protected-resource/api/endpoint/{slug}/mcp",
            get(format!(
                "/.well-known/oauth-protected-resource/api/endpoint/{SLUG}/mcp"
            )),
            Open("RFC 9728 metadata at the RFC's own place (T-4631), for every caller"),
        ),
        row(
            "GET",
            "/.well-known/oauth-authorization-server",
            get("/.well-known/oauth-authorization-server".to_owned()),
            Open("RFC 8414 metadata of the anonymous OAuth server (ADR-N-052)"),
        ),
        row(
            "POST",
            "/oauth/register",
            post(
                "/oauth/register".to_owned(),
                json!({ "redirect_uris": ["https://claude.ai/api/mcp/auth_callback"] }),
            ),
            Open("a registration stores nothing and names only redirect URIs (ADR-N-052)"),
        ),
        row(
            "GET",
            "/oauth/authorize",
            get("/oauth/authorize?client_id=forged&response_type=code".to_owned()),
            Oauth("a forged client_id is refused on this host, never redirected"),
        ),
        row(
            "POST",
            "/oauth/token",
            post(
                "/oauth/token".to_owned(),
                json!({ "grant_type": "password" }),
            ),
            Oauth("a token request that is no form is an OAuth error"),
        ),
        row(
            "POST",
            "/api/mcp/{project}/{name}",
            post(format!("/api/mcp/{PROJECT}/{SERVER}"), hub_query),
            DATA,
        ),
        row(
            "GET",
            "/api/mcp/{project}/{name}/.well-known/oauth-protected-resource",
            get(format!(
                "/api/mcp/{PROJECT}/{SERVER}/.well-known/oauth-protected-resource"
            )),
            Open("RFC 9728 metadata: the resource URL and the realm, for every caller"),
        ),
        row(
            "POST",
            "/api/endpoint/{slug}/access/check",
            post(ep("/access/check"), check.clone()),
            Member,
        ),
        row(
            "POST",
            "/api/endpoint/{slug}/access/simulate",
            post(
                ep("/access/simulate"),
                json!({"subject": {"roles": ["reader"]}, "action": {"name": "queryEntity"}}),
            ),
            Portal,
        ),
        row(
            "GET",
            "/api/endpoint/{slug}/file.csv",
            get(ep("/file.csv")),
            DATA,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/ogc/features",
            get(ep("/ogc/features")),
            Member,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/ogc/features/",
            get(ep("/ogc/features/")),
            Member,
        ),
        row("GET", "/api/endpoint/{slug}/ws", get(ep("/ws")), Member),
        row(
            "ANY",
            "/api/endpoint/{slug}/sta/v1.1",
            get(ep("/sta/v1.1")),
            Member,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/sta/v1.1/",
            get(ep("/sta/v1.1/")),
            Member,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/sta/v1.1/{*rest}",
            get(ep("/sta/v1.1/Things")),
            DATA,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/ogc/features/{*rest}",
            get(ep("/ogc/features/collections/Device/items")),
            DATA,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/odata/v4",
            get(ep("/odata/v4")),
            Member,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/odata/v4/",
            get(ep("/odata/v4/")),
            Member,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/odata/v4/{*rest}",
            get(ep("/odata/v4/Device")),
            DATA,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/ical/v1/{*rest}",
            get(ep("/ical/v1/events.ics")),
            DATA,
        ),
        row(
            "ANY",
            "/api/endpoint/{slug}/feed/v1/{*rest}",
            get(ep("/feed/v1/news/atom.xml")),
            DATA,
        ),
        row(
            "GET",
            "/catalog.jsonld",
            get("/catalog.jsonld".to_owned()),
            Listing("the public Endpoints' records for harvesters; a project-bound one is absent"),
        ),
        row(
            "GET",
            "/catalog.ttl",
            get("/catalog.ttl".to_owned()),
            Listing("the public Endpoints' records for harvesters; a project-bound one is absent"),
        ),
        row(
            "GET",
            "/catalog/{file}",
            get(format!("/catalog/{PROJECT}.jsonld")),
            Withheld("one publisher's public Endpoints' records for harvesters (EP-127)"),
        ),
        row(
            "GET",
            "/cs",
            get("/cs".to_owned()),
            Listing("the space catalogue, which lists what the caller may see"),
        ),
        row("GET", SPACE_ROW, get(cs("")), Member),
        row(
            "ANY",
            "/cs/{space}/ngsi-ld/v1/{*rest}",
            get(cs("/ngsi-ld/v1/entities?type=Device")),
            DATA,
        ),
        row("POST", "/cs/{space}/mcp", post(cs("/mcp"), query), DATA),
        row(
            "GET",
            "/cs/{space}/schema/index.json",
            get(cs("/schema/index.json")),
            Member,
        ),
        row(
            "GET",
            "/cs/{space}/schema/{version}/{artifact}",
            get(cs("/schema/v1/model.shacl.ttl")),
            Member,
        ),
        row(
            "GET",
            "/api/endpoint/{slug}/.well-known/oauth-protected-resource",
            get(ep("/.well-known/oauth-protected-resource")),
            Open("RFC 9728 metadata: the resource URL and the realm, for every slug"),
        ),
        row(
            "GET",
            "/api/endpoint/{slug}/schema/index.json",
            get(ep("/schema/index.json")),
            Member,
        ),
        row(
            "GET",
            "/api/endpoint/{slug}/openapi.json",
            get(ep("/openapi.json")),
            Member,
        ),
        row(
            "GET",
            "/api/endpoint/{slug}/schema/{version}/{artifact}",
            get(ep("/schema/v1/model.shacl.ttl")),
            Member,
        ),
        // EP-53 (T-4419): the published Mappings of a major, narrowed per caller like the model.
        row(
            "GET",
            "/api/endpoint/{slug}/schema/{version}/mappings/",
            get(ep("/schema/v1/mappings/")),
            Member,
        ),
        row(
            "GET",
            "/api/endpoint/{slug}/schema/{version}/mappings/{file}",
            get(ep("/schema/v1/mappings/sweep.blobl")),
            Member,
        ),
        row(
            "GET",
            "/healthz",
            get("/healthz".to_owned()),
            Open("liveness probe"),
        ),
        row(
            "GET",
            "/livez",
            get("/livez".to_owned()),
            Open("liveness probe"),
        ),
        row(
            "GET",
            "/metrics",
            get("/metrics".to_owned()),
            Open("Prometheus counters, reachable only inside the cluster, no request of anyone's"),
        ),
    ]
}

/// The source of `app::router` and of each surface module whose `routes()` it merges.
const SURFACES: &[(&str, &str)] = &[
    ("app", include_str!("../src/app.rs")),
    ("catalog", include_str!("../src/handlers/catalog.rs")),
    ("ngsi_ld", include_str!("../src/handlers/ngsi_ld.rs")),
    (
        "view_access",
        include_str!("../src/handlers/view_access.rs"),
    ),
    ("mcp", include_str!("../src/mcp/mod.rs")),
    ("files", include_str!("../src/handlers/files.rs")),
    ("ogc", include_str!("../src/handlers/ogc.rs")),
    ("ws", include_str!("../src/handlers/ws.rs")),
    (
        "space_routes",
        include_str!("../src/handlers/space_routes.rs"),
    ),
    (
        "schema_routes",
        include_str!("../src/handlers/schema_routes.rs"),
    ),
    (
        "openapi_doc",
        include_str!("../src/handlers/openapi_doc.rs"),
    ),
    ("mappings", include_str!("../src/handlers/mappings.rs")),
    ("oauth", include_str!("../src/oauth.rs")),
];

/// Every `.route(path, …)` the router serves, the constant `HUB_PATH` resolved. A module
/// `app::router` merges that `SURFACES` does not read is red, so no surface is swept blind.
fn served_routes() -> BTreeSet<String> {
    let merged: BTreeSet<&str> = SURFACES[0]
        .1
        .split(".merge(")
        .skip(1)
        .map(|tail| tail.split("::routes()").next().expect("a module name"))
        .collect();
    let read: BTreeSet<&str> = SURFACES[1..].iter().map(|(name, _)| *name).collect();
    assert_eq!(
        merged, read,
        "app::router merges a surface this sweep does not read"
    );
    let mut routes = BTreeSet::new();
    for (name, source) in SURFACES {
        unmounted_routers_refused(name, source);
        routes.extend(routes_in(source));
    }
    routes
}

/// A router mounted by `.nest`, `.nest_service` or `.route_service` serves paths no `.route(`
/// names, so this sweep would pass it without a row (T-4774): refused instead.
fn unmounted_routers_refused(name: &str, source: &str) {
    let source = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
    for mount in [".nest(", ".nest_service(", ".route_service("] {
        assert!(
            !source.contains(mount),
            "{name} mounts a router by `{mount}`, whose routes this sweep cannot read"
        );
    }
}

#[test]
#[should_panic(expected = "whose routes this sweep cannot read")]
fn a_nested_router_is_refused_not_swept_blind() {
    unmounted_routers_refused("fixture", r#"Router::new().nest("/internal", internal())"#);
}

fn routes_in(source: &str) -> BTreeSet<String> {
    let source = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
    let mut routes = BTreeSet::new();
    let mut rest = source;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let argument = rest.trim_start();
        let path = if let Some(quoted) = argument.strip_prefix('"') {
            quoted[..quoted.find('"').expect("a closed string")].to_owned()
        } else {
            let name: String = argument
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert_eq!(
                name, "HUB_PATH",
                "a route path this sweep cannot read: {name}"
            );
            HUB_PATH.to_owned()
        };
        routes.insert(path);
    }
    routes
}

fn endpoint(base_path: String) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::ProjectList,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Mcp,
            Representation::Csv,
            Representation::OgcFeatures,
            Representation::Sta,
            Representation::Odata,
            Representation::WebSocket,
            Representation::Ical,
            Representation::Feed,
        ],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path,
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: vec![serde_norway::from_str(
            "name: news\nsources: [{ type: Device, preset: generic }]\n",
        )
        .expect("the feed output parses")],
        calendars: Vec::new(),
        map_defaults: Default::default(),
        policy_names: vec!["readers".to_owned()],
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(GRANT).expect("the policy parses")],
    }
}

fn secret() -> Value {
    json!({
        "id": format!("urn:ngsi-ld:Device:{SECRET}"),
        "type": "Device",
        "name": { "type": "Property", "value": SECRET },
        "location": { "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [24.94, 60.17] } }
    })
}

/// The gateway in front of a broker whose view `view` serves the secret to `reader` alone.
async fn gateway(realm: &common::Realm, view: &str) -> axum::Router {
    let broker = BrokerStub::narrowing(
        vec![json!([secret()])],
        View::of(view, "reader", &["Device"], None),
    )
    .await;
    let gateway = Gateway::new(Broker::new(broker.url), Box::new(PolicyPdp), "hel.fi")
        .serve([endpoint(format!("/api/endpoint/{SLUG}"))])
        .serve_spaces([Space {
            endpoint: Arc::new(endpoint(format!("/cs/{SPACE}"))),
            title: Default::default(),
            description: Default::default(),
            is_sandbox: false,
            default_locale: None,
        }])
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        )
        .issue_anonymous_tokens(
            Issuer::new(
                HOST,
                &jc_core::Secret::new("sweep-oauth-key-".repeat(2)),
                vec!["claude.ai".to_owned()],
            )
            .expect("a long enough key"),
        );
    gateway.replace_servers(vec![McpServer {
        project: PROJECT.to_owned(),
        name: SERVER.to_owned(),
        title: None,
        description: None,
        members: vec![SLUG.to_owned()],
        select: None,
        endpoints: None,
        audience: Audience::ProjectList,
        allowed_projects: Vec::new(),
        knowledge_sources: Vec::new(),
    }]);
    // The project names its publisher, so its catalogue route runs to the Endpoint table (EP-127).
    gateway.replace_publishers(
        [(
            PROJECT.to_owned(),
            Arc::new(context_gateway::store::ProjectPublisher {
                title: Default::default(),
                description: Default::default(),
                domain: None,
                catalog: jc_core::kinds::ProjectCatalog {
                    publisher: jc_core::kinds::Publisher {
                        name: [("en".to_owned(), "City of Helsinki".to_owned())].into(),
                        uri: Some("https://www.hel.fi/".to_owned()),
                    },
                    contact_point: None,
                },
            }),
        )]
        .into(),
    );
    // One published Mapping, so its file route has something to serve (EP-53). It reads no
    // source name, so admission alone decides who gets it.
    gateway.replace_mappings(
        [(
            SLUG.to_owned(),
            vec![Arc::new(PublishedMapping {
                name: "sweep".to_owned(),
                title: Value::Null,
                source: ("alerts".to_owned(), "1".to_owned()),
                target: ("other".to_owned(), "1".to_owned()),
                major: 1,
                files: vec![MappingFile {
                    name: "sweep.blobl".to_owned(),
                    media_type: "text/plain",
                    bytes: Arc::from(b"root = this\n".as_slice()),
                }],
                reads: Default::default(),
            })],
        )]
        .into(),
    );
    router(Arc::new(gateway))
}

/// A person's token, valid on every audience this gateway honours.
fn token(realm: &common::Realm, caller: Caller) -> Option<String> {
    let (group, roles): (&str, &[&str]) = match caller {
        Caller::Anonymous => return None,
        Caller::Portal => {
            return Some(realm.mint(&json!({
                "iss": common::ISSUER,
                "sub": "f:1:portal",
                "aud": SIMULATE_AUDIENCE,
                "azp": PORTAL_CLIENT,
                "preferred_username": format!("service-account-{PORTAL_CLIENT}"),
                "exp": common::in_seconds(300),
                "iat": common::in_seconds(-10),
            })))
        }
        Caller::Stranger => ("/espoo", &["reader"]),
        Caller::Ungranted => ("/helsinki", &["viewer"]),
        Caller::Granted => ("/helsinki", &["reader"]),
    };
    Some(realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": format!("f:1:{caller:?}"),
        "aud": [SLUG, EDGE_AUDIENCE, HUB_AUDIENCE, format!("mcp-{PROJECT}-{SERVER}"),
            format!("{HOST}/api/endpoint/{SLUG}"), format!("{HOST}/cs/{SPACE}"),
            format!("{HOST}{HUB_PATH}"), SIMULATE_AUDIENCE],
        "azp": "edge",
        "preferred_username": format!("{caller:?}").to_lowercase(),
        "groups": [group],
        "realm_access": { "roles": roles },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    })))
}

/// One answer: status, `WWW-Authenticate` present, body.
async fn send(app: axum::Router, row: &Row, token: Option<String>) -> (StatusCode, bool, String) {
    let (method, uri, body) = &row.send;
    let mut request = Request::builder().method(method.clone()).uri(uri.as_str());
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if body.is_some() {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
    }
    let request = request
        .body(
            body.as_ref()
                .map_or_else(Body::empty, |b| Body::from(b.to_string())),
        )
        .expect("a request");
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let challenged = response.headers().contains_key(header::WWW_AUTHENTICATE);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        challenged,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// A refusal: a `401`, `403` or `404`, or a JSON-RPC error or tool error on an MCP route.
fn refused(status: StatusCode, body: &str) -> bool {
    if matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ) {
        return true;
    }
    serde_json::from_str::<Value>(body)
        .is_ok_and(|answer| answer.get("error").is_some() || answer["result"]["isError"] == true)
}

/// Why one answer breaks its row's promise, or `None`. `route` decides what an anonymous refusal
/// looks like: a `401` on an Endpoint's own URLs, where a client is told to log in; the space and
/// MCP routes may answer it as they answer a stranger.
fn broken(
    route: &str,
    guard: Guard,
    caller: Caller,
    status: StatusCode,
    body: &str,
) -> Option<String> {
    let granted = caller == Caller::Granted;
    if !granted && body.contains(SECRET) {
        return Some("the secret reached a caller the Policy does not grant it".to_owned());
    }
    let anonymous_401 = route.starts_with("/api/endpoint/");
    let ok = match (guard, caller) {
        (Guard::Open(_), _) => status.is_success(),
        (Guard::Oauth(_), _) => status != StatusCode::NOT_FOUND && !status.is_server_error(),
        (Guard::Withheld(_), _) => status == StatusCode::NOT_FOUND,
        (Guard::Portal, Caller::Portal) => !refused(status, body),
        // The Portal's token is bound to the simulator alone: everywhere else it is a token for
        // another audience, refused, or ignored by a listing that reads no token.
        (Guard::Listing(_), Caller::Portal) => true,
        (_, Caller::Portal) => refused(status, body),
        (Guard::Listing(_), _) => status.is_success(),
        (_, Caller::Anonymous) if anonymous_401 => status == StatusCode::UNAUTHORIZED,
        (Guard::Portal, _) => refused(status, body),
        (_, Caller::Anonymous | Caller::Stranger) => refused(status, body),
        (Guard::Data, Caller::Granted) => !refused(status, body) && body.contains(SECRET),
        (Guard::Data, _) => refused(status, body),
        (Guard::Member, Caller::Ungranted) => status != StatusCode::UNAUTHORIZED,
        (Guard::Member, _) => !refused(status, body),
    };
    if !ok {
        return Some(format!("answered {status}"));
    }
    let member = matches!(caller, Caller::Ungranted | Caller::Granted);
    let named = body.contains(SLUG) || body.contains(&format!("/cs/{SPACE}"));
    (matches!(guard, Guard::Listing(_)) && !member && named)
        .then(|| "named the project-bound Endpoint to a caller outside its project".to_owned())
}

#[test]
fn every_route_of_the_router_has_a_row_and_every_row_a_route() {
    let served = served_routes();
    let listed: BTreeSet<String> = rows().iter().map(|row| row.route.to_owned()).collect();
    let unlisted: Vec<_> = served.difference(&listed).collect();
    let gone: Vec<_> = listed.difference(&served).collect();
    assert!(
        unlisted.is_empty() && gone.is_empty(),
        "routes without a row: {unlisted:?}; rows without a route: {gone:?}"
    );
    for row in rows() {
        assert!(!row.method.is_empty(), "{}", row.route);
    }
}

#[tokio::test]
async fn every_route_refuses_whom_the_policy_does_not_admit() {
    let realm = common::Realm::new();
    let endpoint_app = gateway(&realm, SLUG).await;
    let space_app = gateway(&realm, &format!("cs-{SPACE}")).await;
    let mut failures = Vec::new();
    for row in rows() {
        let app = if row.route.starts_with(SPACE_ROW) {
            &space_app
        } else {
            &endpoint_app
        };
        for caller in CALLERS {
            let (status, challenged, body) = send(app.clone(), &row, token(&realm, caller)).await;
            if let Some(why) = broken(row.route, row.guard, caller, status, &body) {
                let promise = match row.guard {
                    Guard::Listing(reason)
                    | Guard::Open(reason)
                    | Guard::Oauth(reason)
                    | Guard::Withheld(reason) => reason,
                    _ => "",
                };
                failures.push(format!(
                    "{} {} as {caller:?} ({promise}): {why}: {}",
                    row.method,
                    row.route,
                    body.chars().take(160).collect::<String>()
                ));
            }
            // The anonymous refusal tells a client where to log in on the MCP routes (AG-32).
            if caller == Caller::Anonymous
                && status == StatusCode::UNAUTHORIZED
                && row.route.ends_with("/mcp")
                && row.route.starts_with("/api/endpoint/")
                && !challenged
            {
                failures.push(format!(
                    "{} as anonymous: a 401 with no challenge",
                    row.route
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every tool name of the hub's catalogue, `src/mcp/tools.rs` (`broker("name", …)` and the
/// gateway's own `Tool { name: "…" }`), read from the source so a tool added there is swept.
fn catalogue() -> Vec<String> {
    let source = include_str!("../src/mcp/tools.rs");
    let source = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
    let mut names = Vec::new();
    for mark in ["broker(", "name: \""] {
        let mut rest = source;
        while let Some(at) = rest.find(mark) {
            rest = &rest[at + mark.len()..];
            let quoted = if mark == "broker(" {
                match rest.trim_start().strip_prefix('"') {
                    Some(quoted) => quoted,
                    None => continue, // the `const fn broker(` itself
                }
            } else {
                rest
            };
            names.push(quoted[..quoted.find('"').expect("a closed string")].to_owned());
        }
    }
    names
}

#[tokio::test]
async fn every_tool_of_the_catalogue_refuses_whom_the_policy_does_not_admit() {
    let names = catalogue();
    assert!(
        names.len() > 20 && names.iter().any(|n| n == "describe_access"),
        "{names:?}"
    );
    let realm = common::Realm::new();
    let app = gateway(&realm, SLUG).await;
    let mut failures = Vec::new();
    for name in &names {
        let arguments = match name.as_str() {
            "query_entities" => json!({"endpoint": SLUG, "type": "Device"}),
            _ => json!({"endpoint": SLUG}),
        };
        let row = Row {
            method: "POST",
            route: HUB_PATH,
            send: (
                Method::POST,
                HUB_PATH.to_owned(),
                Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": name, "arguments": arguments}})),
            ),
            guard: DATA,
        };
        for caller in CALLERS {
            let (status, _, body) = send(app.clone(), &row, token(&realm, caller)).await;
            let granted = caller == Caller::Granted;
            let why = if !granted && body.contains(SECRET) {
                Some("the secret reached a caller the Policy does not grant it")
            } else if !granted && caller != Caller::Ungranted && !refused(status, &body) {
                Some("served a caller outside the Endpoint's audience")
            } else if granted && name == "query_entities" && !body.contains(SECRET) {
                Some("the granted member was not served")
            } else if granted && name.starts_with("describe_") && refused(status, &body) {
                Some("the granted member was refused")
            } else {
                None
            };
            // The ungranted member may be told what it may do (describe_access) and nothing
            // else: every other tool is refused to it.
            let why = why.or_else(|| {
                (caller == Caller::Ungranted
                    && name != "describe_access"
                    && !refused(status, &body))
                .then_some("served a member the Policy grants nothing")
            });
            if let Some(why) = why {
                failures.push(format!(
                    "{name} as {caller:?}: {why}: {status} {}",
                    body.chars().take(160).collect::<String>()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_secret_in_a_refusal_breaks_the_row() {
    let route = "/api/endpoint/{slug}/file.csv";
    for guard in [
        DATA,
        Guard::Member,
        Guard::Listing("catalogue"),
        Guard::Open("probe"),
    ] {
        assert!(broken(
            route,
            guard,
            Caller::Stranger,
            StatusCode::FORBIDDEN,
            SECRET
        )
        .is_some());
        assert!(broken(route, guard, Caller::Granted, StatusCode::OK, SECRET).is_none());
    }
    assert!(broken(route, DATA, Caller::Ungranted, StatusCode::OK, "[]").is_some());
    assert!(
        broken(route, DATA, Caller::Granted, StatusCode::OK, "[]").is_some(),
        "served nothing"
    );
    assert!(broken(route, DATA, Caller::Anonymous, StatusCode::NOT_FOUND, "").is_some());
    assert!(broken(
        "/cs/{space}",
        DATA,
        Caller::Anonymous,
        StatusCode::NOT_FOUND,
        ""
    )
    .is_none());
    let tool_error = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unknown tool"}}"#;
    assert!(broken(
        "/api/mcp",
        DATA,
        Caller::Ungranted,
        StatusCode::OK,
        tool_error
    )
    .is_none());
    assert!(broken(
        route,
        Guard::Member,
        Caller::Anonymous,
        StatusCode::OK,
        "{}"
    )
    .is_some());
    assert!(broken(route, Guard::Portal, Caller::Granted, StatusCode::OK, "{}").is_some());
    let listing = Guard::Listing("catalogue");
    assert!(broken("/cs", listing, Caller::Stranger, StatusCode::OK, SLUG).is_some());
    assert!(broken("/cs", listing, Caller::Ungranted, StatusCode::OK, SLUG).is_none());
}
