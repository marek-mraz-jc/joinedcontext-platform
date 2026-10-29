//! `GET /ngsi-ld/v1/types` and `/attributes` through an Endpoint (CIM 009 5.7.5, 5.7.8; T-4080,
//! T-5391).
//!
//! The Endpoint's view answers both from its type and Attribute catalogue, filtered by the
//! caller's grant (Antares T-4934): the gateway forwards the request and adds no name of its own.
//! It used to read the space's type list and probe the view once per type, 0.3-3.7 s on dev.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, RateLimits, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";

/// Every path the broker was asked, with its query.
type Asked = Arc<Mutex<Vec<String>>>;

/// The broker: the space holds Alert, GtfsStop, Secret and Vehicle with a `secretNote` Attribute;
/// the view's catalogue shows this caller GtfsStop and Vehicle and hides `secretNote`, as the
/// engine filters it by the grant. `refused` is an engine refusing the caller the read itself.
async fn view_broker(refused: bool) -> (String, Asked) {
    let asked: Asked = Arc::default();
    let recorder = Arc::clone(&asked);
    let app = Router::new().fallback(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            recorder
                .lock()
                .expect("no poisoned lock")
                .push(format!("{path}?{query}"));
            let view = format!("/x/views/{SLUG}/ngsi-ld/v1");
            if refused && path.starts_with(&view) {
                return (
                    StatusCode::FORBIDDEN,
                    axum::Json(json!({ "title": "AccessDenied", "status": 403 })),
                )
                    .into_response();
            }
            let answer = if path == "/ngsi-ld/v1/types" {
                json!({ "id": "urn:ngsi-ld:EntityTypeList:all", "type": "EntityTypeList",
                        "typeList": ["Alert", "GtfsStop", "Secret", "Vehicle"] })
            } else if path == format!("{view}/types") && query.contains("details=true") {
                json!([{ "id": "GtfsStop", "type": "EntityType", "typeName": "GtfsStop",
                         "attributeNames": ["name"] }])
            } else if path == format!("{view}/types") {
                json!({ "id": "urn:ngsi-ld:EntityTypeList:view", "type": "EntityTypeList",
                        "typeList": ["GtfsStop", "Vehicle"] })
            } else if path == format!("{view}/attributes") {
                json!({ "id": "urn:ngsi-ld:AttributeList:view", "type": "AttributeList",
                        "attributeList": ["location", "name"] })
            } else {
                json!([{ "id": "urn:ngsi-ld:Secret:1", "type": "Secret", "secretNote": "x" }])
            };
            axum::Json(answer).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}"), asked)
}

fn endpoint(operations: &str, granted: &[&str]) -> Endpoint {
    let information = if granted.is_empty() {
        String::new()
    } else {
        let entities: Vec<String> = granted
            .iter()
            .map(|name| format!("{{ type: {name} }}"))
            .collect();
        format!("information:\n  - entities: [{}]\n", entities.join(", "))
    };
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "air".to_owned(),
        project: "air".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![serde_norway::from_str(&format!(
            "contextSpaceRef: air\n\
             assigner: did:web:example.org\n\
             assignee: {{ kind: role, id: public }}\n\
             operations: {operations}\n\
             {information}"
        ))
        .expect("the policy spec parses")],
    }
}

/// An anonymous `GET` of `rest` under the Endpoint's NGSI-LD root, as a client with no account
/// sends it.
async fn get(broker: &str, endpoint: Endpoint, rest: &str) -> (StatusCode, Value) {
    let app = router(Arc::new(
        Gateway::new(Broker::new(broker), Box::new(PolicyPdp), "example.org").serve([endpoint]),
    ));
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/{rest}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn asked(asked: &Asked) -> Vec<String> {
    asked.lock().expect("no poisoned lock").clone()
}

/// The type list is the view's: one request to it, no read of the space's own list and no probe
/// per type, so a type the view refuses the caller (Alert, Secret) never appears.
#[tokio::test]
async fn the_type_list_is_the_views_answer_in_one_request() {
    let (broker, hops) = view_broker(false).await;
    let (status, answer) = get(
        &broker,
        endpoint("[retrieveOps]", &["GtfsStop", "Vehicle"]),
        "types",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["type"], json!("EntityTypeList"));
    assert_eq!(answer["typeList"], json!(["GtfsStop", "Vehicle"]));
    assert_eq!(
        asked(&hops),
        [format!("/x/views/{SLUG}/ngsi-ld/v1/types?")],
        "the gateway asked the view once and nothing else"
    );
}

/// One request costs the caller one request of the rate limit: the list no longer charges a
/// probe per type the space holds (F-SEC1-017 charged them before T-5391).
#[tokio::test]
async fn the_type_list_costs_one_request() {
    let mut limited = endpoint("[retrieveOps]", &["GtfsStop", "Vehicle"]);
    limited.rate_limit = Some(RateLimits {
        requests_per_minute: 1,
        burst: Some(1),
    });
    let (broker, _) = view_broker(false).await;
    let (status, answer) = get(&broker, limited, "types").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
}

/// The Attribute list and the type details are the view's too: an Attribute the view hides
/// (`secretNote`) never appears, and the gateway adds none.
#[tokio::test]
async fn the_attribute_list_and_type_details_are_the_views_answers() {
    let (broker, hops) = view_broker(false).await;
    let (status, answer) = get(&broker, endpoint("[retrieveOps]", &[]), "attributes").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["attributeList"], json!(["location", "name"]));
    let (status, details) = get(
        &broker,
        endpoint("[retrieveOps]", &[]),
        "types?details=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{details}");
    assert_eq!(details[0]["typeName"], json!("GtfsStop"));
    for answer in [answer, details] {
        assert!(!answer.to_string().contains("secretNote"), "{answer}");
        assert!(!answer.to_string().contains("Secret\""), "{answer}");
    }
    assert!(
        asked(&hops)
            .iter()
            .all(|hop| hop.starts_with(&format!("/x/views/{SLUG}/ngsi-ld/v1/"))),
        "{:?}",
        asked(&hops)
    );
}

/// A caller the view refuses is answered by the view's refusal: no list is made up for them.
#[tokio::test]
async fn without_a_read_grant_the_view_refuses() {
    let (broker, hops) = view_broker(true).await;
    for rest in ["types", "attributes"] {
        let (status, answer) = get(&broker, endpoint("[createEntity]", &[]), rest).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{rest}: {answer}");
        assert!(answer.get("typeList").is_none(), "{answer}");
    }
    assert!(
        asked(&hops)
            .iter()
            .all(|hop| hop.starts_with(&format!("/x/views/{SLUG}/ngsi-ld/v1/"))),
        "{:?}",
        asked(&hops)
    );
}
