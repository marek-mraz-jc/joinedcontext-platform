//! T-2776, EP-85: `POST /api/endpoint/{slug}/preview` answers what the endpoint would serve if its
//! filter were the draft, and nothing the endpoint's Policies do not grant.
//!
//! What is worth a test: the draft answers exactly what the same filter saved answers through a
//! normal read (the same broker query, the same entities), a draft cannot reach a type the Policy
//! does not grant, a hidden attribute of the draft is not served, the ids restrict the page, a
//! caller the space does not admit is told the slug does not exist, and a bad draft is refused
//! naming the member at fault.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SAVED: &str = "savedsavedsavedsavedsaved2";
const DRAFTED: &str = "drafteddrafteddrafteddraft";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";

const PARTNER_VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: Vehicle
      slots: [name, speed]
  filter:
    q: category=="public"
"#;

fn saved_projection() -> Arc<ModelProjectionSpec> {
    let parsed = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(PARTNER_VIEW).expect("parses");
    parsed.validate().expect("valid");
    Arc::new(parsed.spec)
}

/// The same projection as the draft body carries it.
fn draft_projection() -> Value {
    json!({
        "classes": [{ "name": "Vehicle", "slots": ["name", "speed"] }],
        "filter": { "q": "category==\"public\"" }
    })
}

fn bus() -> Value {
    json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
        "type": "Vehicle",
        "name": { "type": "Property", "value": "Bus 01" },
        "speed": { "type": "Property", "value": 32 },
        "odometer": { "type": "Property", "value": 120_000 }
    })
}

type Hops = Arc<Mutex<Vec<String>>>;

async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.uri().query().unwrap_or_default().to_owned());
            axum::Json(json!([bus()]))
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

/// A public grant on Vehicle only: User and Depot are in the model and in nobody's grant.
fn vehicles_for_everybody() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities:\n      - type: Vehicle\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(slug: &str, base: &str, projection: Option<Arc<ModelProjectionSpec>>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: slug.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection,
        view_mapping: None,
        catalog: None,
        base_path: base.to_owned(),
        models: Vec::new(),
        policies: vec![vehicles_for_everybody()],
    }
}

/// The space's canonical surface, which admits the anonymous caller here because its grant is
/// public: the caller of every test below is a caller the space admits unless it says otherwise.
fn space() -> Space {
    Space {
        endpoint: Arc::new(endpoint(SPACE, &format!("/cs/{SPACE}"), None)),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    }
}

struct Answer {
    status: StatusCode,
    body: Value,
    asked: Vec<String>,
}

async fn call(with_space: bool, method: Method, uri: &str, body: Option<Value>) -> Answer {
    let (upstream, hops) = broker().await;
    let mut gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([
        endpoint(
            SAVED,
            &format!("/api/endpoint/{SAVED}"),
            Some(saved_projection()),
        ),
        endpoint(DRAFTED, &format!("/api/endpoint/{DRAFTED}"), None),
    ]);
    if with_space {
        gateway = gateway.serve_spaces([space()]);
    }
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = router(Arc::new(gateway))
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let asked = hops.lock().expect("the hop log").clone();
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        asked,
    }
}

async fn preview(draft: Value) -> Answer {
    call(
        true,
        Method::POST,
        &format!("/api/endpoint/{DRAFTED}/preview"),
        Some(draft),
    )
    .await
}

#[tokio::test]
async fn a_draft_answers_what_the_same_filter_saved_answers() {
    let saved = call(
        true,
        Method::GET,
        &format!(
            "/api/endpoint/{SAVED}/ngsi-ld/v1/entities?type=Vehicle&limit=50&offset=0&count=true"
        ),
        None,
    )
    .await;
    let drafted = preview(json!({ "projection": draft_projection(), "type": "Vehicle" })).await;

    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(drafted.status, StatusCode::OK, "{}", drafted.body);
    assert_eq!(
        drafted.asked, saved.asked,
        "the broker is asked the same question"
    );
    assert!(drafted.asked[0].contains("category"), "{:?}", drafted.asked);
    assert_eq!(drafted.body, saved.body);
    // The projection keeps name and speed of a Vehicle: the odometer the broker holds is not served.
    assert!(
        drafted.body[0].get("odometer").is_none(),
        "{}",
        drafted.body
    );
    assert_eq!(drafted.body[0]["speed"]["value"], json!(32));
}

#[tokio::test]
async fn a_draft_cannot_serve_a_type_the_policy_does_not_grant() {
    let widened = preview(json!({
        "projection": {
            "classes": [
                { "name": "Vehicle", "slots": ["name"] },
                { "name": "User", "slots": ["name", "age"] }
            ]
        },
        "type": "User"
    }))
    .await;
    assert!(
        widened.asked.is_empty(),
        "the broker is never asked for a type nobody granted: {:?}",
        widened.asked
    );
    // The answer a normal read gives a type outside the grants: an empty page, not a refusal
    // that would say the type exists.
    assert_eq!(widened.status, StatusCode::OK, "{}", widened.body);
    assert_eq!(widened.body, json!([]));
}

#[tokio::test]
async fn a_hidden_attribute_of_the_draft_is_not_served() {
    let answer = preview(json!({ "hiddenAttributes": ["speed"], "type": "Vehicle" })).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let entity = &answer.body[0];
    assert!(entity.get("speed").is_none(), "{entity}");
    assert_eq!(entity["name"]["value"], json!("Bus 01"));
}

#[tokio::test]
async fn the_ids_restrict_the_page_the_two_sides_are_aligned_on() {
    let answer = preview(json!({
        "type": "Vehicle",
        "id": ["urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01"],
        "limit": 10,
        "offset": 20
    }))
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let asked = &answer.asked[0];
    assert!(asked.contains("bus-01"), "{asked}");
    assert!(asked.contains("limit=10"), "{asked}");
    assert!(asked.contains("offset=20"), "{asked}");
}

#[tokio::test]
async fn a_caller_the_space_does_not_admit_is_told_the_slug_does_not_exist() {
    let answer = call(
        false,
        Method::POST,
        &format!("/api/endpoint/{DRAFTED}/preview"),
        Some(json!({ "type": "Vehicle" })),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND, "{}", answer.body);
    assert!(answer.asked.is_empty());
    let unknown = call(
        true,
        Method::POST,
        "/api/endpoint/nosuchslugnosuchslugnosuchs/preview",
        Some(json!({ "type": "Vehicle" })),
    )
    .await;
    assert_eq!(unknown.status, answer.status);
}

#[tokio::test]
async fn a_bad_draft_is_refused_naming_the_member_at_fault() {
    for (draft, member) in [
        (
            json!({ "projection": { "classes": [] }, "type": "Vehicle" }),
            "projection",
        ),
        (
            json!({ "hiddenAttributes": ["id"], "type": "Vehicle" }),
            "hiddenAttributes",
        ),
        (json!({ "type": "Vehicle", "limit": 101 }), "limit"),
        (json!({ "type": "Vehicle", "limit": 0 }), "limit"),
        (json!({ "type": "not a type" }), "type"),
        (json!({ "type": "Vehicle", "id": ["a,b"] }), "id"),
        (json!({ "type": "Vehicle", "policies": [] }), "policies"),
        (
            json!({ "projection": { "classes": [{ "name": "Vehicle" }], "filter": { "q": " " } }, "type": "Vehicle" }),
            "projection.filter.q",
        ),
    ] {
        let answer = preview(draft.clone()).await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{draft}: {}",
            answer.body
        );
        let detail = answer.body["detail"].as_str().unwrap_or_default();
        assert!(detail.contains(member), "{draft}: {detail}");
        assert!(answer.asked.is_empty(), "{draft}: the broker was asked");
    }
    let not_json = call(
        true,
        Method::POST,
        &format!("/api/endpoint/{DRAFTED}/preview"),
        None,
    )
    .await;
    assert_eq!(not_json.status, StatusCode::BAD_REQUEST);
}
