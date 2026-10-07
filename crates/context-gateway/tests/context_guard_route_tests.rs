//! T-3287: the gateway decides on the names the core context gives them, so a caller's own
//! context never reaches the broker. A create whose context remaps a name the grant allows is
//! refused before the broker is asked, on a public form and on an authenticated write alike, and
//! so is a read whose `Link` names another context; the core context passes as it always has.

mod common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const FORM: &str = "4fdpzhhoouikmq6njpvjib64ns3";
const STAFF: &str = "s7m2qz4tv6xh3n5jb2ryd3wcfa";
const SPACE: &str = "podnety";
const DOMAIN: &str = "zilina.sk";
const CORE: &str = "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld";
const EDITOR: &str = "jana@zilina.sk";

type Hops = Arc<Mutex<Vec<String>>>;

/// A broker that records each hop and answers 201 to a write, an empty list to a read.
async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let method = request.method().clone();
            recorder
                .lock()
                .expect("hops")
                .push(format!("{method} {}", request.uri().path()));
            if method == Method::GET {
                (StatusCode::OK, "[]").into_response()
            } else {
                (StatusCode::CREATED, "").into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), hops)
}

fn policy(assignee: &str, operations: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {assignee}\n\
         operations: {operations}\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Report\n\
         \x20   propertyNames: [description]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(slug: &str, audience: Audience, policies: Vec<PolicySpec>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{slug}"),
        models: Vec::new(),
        policies,
    }
}

struct World {
    app: Router,
    hops: Hops,
    token: String,
}

async fn world() -> World {
    let realm = common::Realm::new();
    let (upstream, hops) = broker().await;
    let gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
        .serve([
            endpoint(
                FORM,
                Audience::Public,
                vec![policy(
                    "{ kind: role, id: public }",
                    "[createEntity, queryEntity]",
                )],
            ),
            endpoint(
                STAFF,
                Audience::Organization,
                vec![policy(
                    &format!("{{ kind: user, id: \"{EDITOR}\" }}"),
                    "[createEntity]",
                )],
            ),
        ])
        .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None);
    let token = realm.mint(&json!({
        "iss": common::ISSUER,
        "sub": "f:1:jana",
        "aud": "context-gateway",
        "preferred_username": EDITOR,
        "email": EDITOR,
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    World {
        app: router(Arc::new(gateway)),
        hops,
        token,
    }
}

impl World {
    async fn send(
        &self,
        method: Method,
        slug: &str,
        path: &str,
        body: Option<Value>,
        extra: &[(&str, String)],
    ) -> (StatusCode, String) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/api/endpoint/{slug}/ngsi-ld/v1{path}"));
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/ld+json");
        }
        for (name, value) in extra {
            request = request.header(*name, value.as_str());
        }
        let response = self
            .app
            .clone()
            .oneshot(
                request
                    .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("a body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn hops(&self) -> Vec<String> {
        self.hops.lock().expect("hops").clone()
    }
}

/// A Report whose `description` the caller's context sends to another IRI.
fn remapped() -> Value {
    json!({
        "id": "urn:ngsi-ld:Report:1",
        "type": "Report",
        "description": { "type": "Property", "value": "pothole" },
        "@context": [{ "description": "https://example.org/vocab/somethingElse" }, CORE],
    })
}

fn plain(context: Option<&str>) -> Value {
    let mut entity = json!({ "id": "urn:ngsi-ld:Report:2", "type": "Report", "description": { "type": "Property", "value": "pothole" } });
    if let Some(context) = context {
        entity["@context"] = json!(context);
    }
    entity
}

#[tokio::test]
async fn a_public_form_create_whose_context_remaps_a_field_is_refused_and_nothing_is_written() {
    let world = world().await;
    let (status, body) = world
        .send(Method::POST, FORM, "/entities", Some(remapped()), &[])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("only the NGSI-LD core context"), "{body}");
    assert!(
        world.hops().is_empty(),
        "the broker was asked: {:?}",
        world.hops()
    );
    // The form's own create, with the core context or none, is written as before.
    for context in [Some(CORE), None] {
        let (status, body) = world
            .send(Method::POST, FORM, "/entities", Some(plain(context)), &[])
            .await;
        assert_eq!(status, StatusCode::CREATED, "{context:?}: {body}");
    }
    assert_eq!(world.hops().len(), 2);
}

#[tokio::test]
async fn an_authenticated_write_with_a_context_of_its_own_is_refused_the_same_way() {
    let world = world().await;
    let bearer = format!("Bearer {}", world.token);
    let (status, body) = world
        .send(
            Method::POST,
            STAFF,
            "/entities",
            Some(remapped()),
            &[("authorization", bearer.clone())],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(world.hops().is_empty());
    let (status, body) = world
        .send(
            Method::POST,
            STAFF,
            "/entities",
            Some(plain(None)),
            &[("authorization", bearer)],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn a_link_naming_another_context_is_refused_on_a_read_and_the_core_one_passes() {
    let world = world().await;
    let foreign = "<https://example.org/c.jsonld>; rel=\"http://www.w3.org/ns/json-ld#context\"; type=\"application/ld+json\"".to_owned();
    let (status, body) = world
        .send(
            Method::GET,
            FORM,
            "/entities?type=Report",
            None,
            &[("link", foreign)],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        world.hops().is_empty(),
        "the broker was asked: {:?}",
        world.hops()
    );
    let core = format!(
        "<{CORE}>; rel=\"http://www.w3.org/ns/json-ld#context\"; type=\"application/ld+json\""
    );
    let (status, body) = world
        .send(
            Method::GET,
            FORM,
            "/entities?type=Report",
            None,
            &[("link", core)],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
