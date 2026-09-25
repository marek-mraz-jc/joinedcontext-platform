//! MCP is on for every Endpoint (T-2901, EP-24): an Endpoint serves its MCP instance whatever
//! its audience and whether or not `enabledRepresentations` lists `mcp`, `mcp: false` is the one
//! way to turn it off, and the instance adds no right, so an internal Endpoint's MCP refuses an
//! outsider exactly as its NGSI-LD surface does.
//!
//! The manifests go through the store, as the repository serves them: the rule lives where a
//! manifest becomes the endpoint table, not in a hand-built table.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const HOST: &str = "https://hel.example.fi";
/// A public Endpoint that lists NGSI-LD only.
const LISTED_NGSI: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
/// A public Endpoint that opts out of MCP.
const OPTED_OUT: &str = "dddddddddddddddddddddddddd";
/// An internal Endpoint, for its own project only.
const INTERNAL: &str = "eeeeeeeeeeeeeeeeeeeeeeeeee";
/// A slug nothing serves.
const UNKNOWN: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzy";

const SPACE: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  \
                     name: helsinki\n  namespace: helsinki\nspec:\n  isSandbox: false\n  \
                     urnSegment: helsinki\n";

fn endpoint(name: &str, slug: &str, audience: &str, extra: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: {name}\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: helsinki\n  slug: {slug}\n  \
         audience: {audience}\n  enabledRepresentations: [\"ngsi-ld\"]\n{extra}"
    )
}

fn policy(name: &str, role: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: {name}\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: {{ kind: ContextSpace, name: helsinki }}\n  \
         assigner: did:web:hel.fi\n  assignee: {{ kind: role, id: {role} }}\n  \
         operations: [retrieveOps]\n  information:\n    - entities:\n        - type: BikeHireDockingStation\n"
    )
}

fn repo(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-mcp-default-{test}-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).expect("write");
    write("space.yaml", SPACE);
    write(
        "listed.yaml",
        &endpoint("helsinki-bikes", LISTED_NGSI, "public", ""),
    );
    write(
        "off.yaml",
        &endpoint("helsinki-quiet", OPTED_OUT, "public", "  mcp: false\n"),
    );
    write(
        "internal.yaml",
        &endpoint(
            "helsinki-ops",
            INTERNAL,
            "project-list",
            "  allowedProjects: [hel-mobility]\n",
        ),
    );
    write("public.yaml", &policy("public-bikes", "public"));
    write("staff.yaml", &policy("staff-bikes", "data-steward"));
    dir
}

fn app(dir: &Path, realm: &common::Realm) -> axum::Router {
    let (endpoints, ..) = store::load(dir).expect("the repository loads");
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "hel.fi",
        )
        .serve(endpoints)
        .authenticate(
            Arc::new(realm.verifier()),
            ServiceAccounts::new(),
            Some(HOST.to_owned()),
        ),
    ))
}

/// A person of `project`, holding `data-steward`, with a token for `slug`.
fn person(realm: &common::Realm, project: &str, slug: &str) -> String {
    realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": format!("someone-of-{project}"),
        "aud": slug,
        "preferred_username": format!("someone-of-{project}"),
        "groups": [format!("/{project}")],
        "realm_access": { "roles": ["data-steward"] },
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }))
}

async fn send(app: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn tools_list(slug: &str, token: Option<&str>) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{slug}/mcp"))
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    request
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
        ))
        .expect("a request")
}

fn entities(slug: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(format!(
            "/api/endpoint/{slug}/ngsi-ld/v1/entities?type=BikeHireDockingStation"
        ))
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("a request")
}

/// EP-24: an Endpoint whose manifest lists NGSI-LD only still serves MCP, with the tools its
/// Policy grants.
#[tokio::test]
async fn an_endpoint_serves_mcp_without_listing_it() {
    let realm = common::Realm::new();
    let dir = repo("listed");
    let (status, body) = send(app(&dir, &realm), tools_list(LISTED_NGSI, None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("a JSON-RPC answer");
    let tools = answer["result"]["tools"].as_array().expect("a tool list");
    assert!(
        !tools.is_empty(),
        "the public grant lists its read tools: {body}"
    );

    // The table says so too, so every surface that reads it (DCAT, the space record) agrees.
    let (endpoints, ..) = store::load(&dir).expect("the repository loads");
    let listed = endpoints
        .iter()
        .find(|e| e.slug == LISTED_NGSI)
        .expect("loaded");
    assert!(listed.serves(jc_core::kinds::Representation::Mcp));
}

/// EP-24, EP-23: `mcp: false` turns the instance off, and the path then answers the bytes an
/// unknown slug gets, so the opt-out is not readable from outside.
#[tokio::test]
async fn an_opted_out_endpoint_serves_no_mcp_and_says_nothing_about_it() {
    let realm = common::Realm::new();
    let dir = repo("off");
    let token = person(&realm, "helsinki", OPTED_OUT);
    let off = send(app(&dir, &realm), tools_list(OPTED_OUT, Some(&token))).await;
    let unknown = send(app(&dir, &realm), tools_list(UNKNOWN, Some(&token))).await;
    assert_eq!(off.0, StatusCode::NOT_FOUND, "{}", off.1);
    assert_eq!(off, unknown);

    // Its NGSI-LD surface is untouched by the opt-out: resolved and admitted, it goes on to the
    // broker (none listens here), rather than 404.
    let data = send(app(&dir, &realm), entities(OPTED_OUT, &token)).await;
    assert_ne!(data.0, StatusCode::NOT_FOUND, "{}", data.1);
}

/// EP-24, SP-16: the instance adds no right. An internal Endpoint's MCP answers its own
/// project's person, and refuses a person of another project exactly as its data surface does.
#[tokio::test]
async fn an_internal_endpoints_mcp_refuses_an_outsider_like_its_data_does() {
    let realm = common::Realm::new();
    let dir = repo("internal");

    let insider = person(&realm, "helsinki", INTERNAL);
    let (status, body) = send(app(&dir, &realm), tools_list(INTERNAL, Some(&insider))).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let outsider = person(&realm, "doprava", INTERNAL);
    let mcp = send(app(&dir, &realm), tools_list(INTERNAL, Some(&outsider))).await;
    let data = send(app(&dir, &realm), entities(INTERNAL, &outsider)).await;
    assert!(
        mcp.0.is_client_error(),
        "an outsider is refused: {} {}",
        mcp.0,
        mcp.1
    );
    assert_eq!(mcp.0, data.0, "MCP {} / data {}", mcp.1, data.1);

    // Without a token, a client is told where to get one, as on any internal instance (AG-32).
    let (status, _) = send(app(&dir, &realm), tools_list(INTERNAL, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
