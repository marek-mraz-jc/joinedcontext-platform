//! T-3172: a public form's Endpoint mints the id, caps the day and takes only the form's
//! fields (EP-97).

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Creates, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "4fdpzhhoouikmq6njpvjib64ns3";
const SPACE: &str = "podnety";
const DOMAIN: &str = "zilina.sk";

type Bodies = Arc<Mutex<Vec<Value>>>;

/// A broker that records every body and answers 201, or 409 to an id it has already seen.
async fn broker() -> (String, Bodies) {
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&bodies);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .expect("a body");
            let body: Value = serde_json::from_slice(&bytes).expect("a JSON body");
            let mut seen = recorder.lock().expect("the body log");
            let duplicate = seen.iter().any(|b| b["id"] == body["id"]);
            seen.push(body);
            if duplicate {
                StatusCode::CONFLICT
            } else {
                StatusCode::CREATED
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
    (format!("http://{address}"), bodies)
}

/// Anonymous callers may create a Report with a description, and nothing else.
fn form_policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [createEntity, createBatch, upsertBatch]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Report\n\
         \x20   propertyNames: [description]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(per_day: Option<u32>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: Some(Creates {
            mint_ids: true,
            per_day,
        }),
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policies: vec![form_policy()],
    }
}

struct Form {
    app: Router,
    bodies: Bodies,
}

async fn form(per_day: Option<u32>) -> Form {
    let (upstream, bodies) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(per_day)]),
    );
    Form {
        app: router(gateway),
        bodies,
    }
}

impl Form {
    async fn post(&self, path: &str, body: Value) -> axum::response::Response {
        self.post_from("203.0.113.7", path, body).await
    }

    /// A post from one anonymous address, as the edge forwards it: whatever the client sent,
    /// then the address the edge saw.
    async fn post_from(&self, address: &str, path: &str, body: Value) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{path}"))
                    .header("content-type", "application/ld+json")
                    .header("x-forwarded-for", format!("10.0.0.1, {address}"))
                    .body(Body::from(body.to_string()))
                    .expect("a request"),
            )
            .await
            .expect("the gateway answers")
    }

    fn sent(&self) -> Vec<Value> {
        self.bodies.lock().expect("the body log").clone()
    }
}

fn report(id: &str) -> Value {
    json!({"id": id, "type": "Report", "description": {"type": "Property", "value": "pothole"}})
}

#[tokio::test]
async fn a_caller_chosen_id_is_not_used() {
    let form = form(None).await;
    let chosen = "urn:ngsi-ld:Report:zilina.sk:podnety:someone-elses";
    assert_eq!(
        form.post("/entities", report(chosen)).await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        form.post("/entities", report(chosen)).await.status(),
        StatusCode::CREATED
    );
    let sent = form.sent();
    assert_eq!(sent.len(), 2);
    for body in &sent {
        let id = body["id"].as_str().expect("an id");
        assert!(id.starts_with("urn:ngsi-ld:Report:"), "{id}");
        assert_ne!(id, chosen);
    }
    assert_ne!(sent[0]["id"], sent[1]["id"], "each create gets its own id");
}

#[tokio::test]
async fn an_attribute_outside_the_form_is_refused_before_the_broker() {
    let form = form(None).await;
    let mut body = report("urn:ngsi-ld:Report:x");
    body["website"] = json!({"type": "Property", "value": "spam"});
    // 403 without naming the rule that decided (GW6): the form knows its own fields.
    assert_eq!(
        form.post("/entities", body).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(form.sent().is_empty(), "the broker was asked");
}

#[tokio::test]
async fn the_cap_answers_429_past_its_count_and_a_refused_create_spends_none() {
    let form = form(Some(2)).await;
    // Refused before the broker: spends nothing.
    let mut spam = report("urn:ngsi-ld:Report:x");
    spam["website"] = json!({"type": "Property", "value": "spam"});
    assert_eq!(
        form.post("/entities", spam).await.status(),
        StatusCode::FORBIDDEN
    );
    // Two senders, so neither runs into its own share first (T-3285).
    for address in ["203.0.113.7", "198.51.100.4"] {
        let status = form
            .post_from(address, "/entities", report("urn:ngsi-ld:Report:x"))
            .await
            .status();
        assert_eq!(status, StatusCode::CREATED);
    }
    let spent = form
        .post_from("192.0.2.80", "/entities", report("urn:ngsi-ld:Report:x"))
        .await;
    assert_eq!(spent.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = spent
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("a Retry-After in seconds");
    assert!((1..=86_400).contains(&retry), "{retry}");
    assert_eq!(form.sent().len(), 2);
}

#[tokio::test]
async fn a_batch_create_or_upsert_cannot_name_its_ids() {
    let form = form(None).await;
    for path in ["/entityOperations/create", "/entityOperations/upsert"] {
        let status = form
            .post(path, json!([report("urn:ngsi-ld:Report:chosen")]))
            .await
            .status();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    assert!(form.sent().is_empty());
}

/// EP-97, T-3285: one sender takes a tenth of the day and is told so, while another sender
/// still creates; a client's own `X-Forwarded-For` entry changes nothing, the edge's does.
#[tokio::test]
async fn one_sender_takes_a_tenth_of_the_day_and_the_form_stays_open_to_others() {
    let form = form(Some(20)).await;
    for _ in 0..2 {
        assert_eq!(
            form.post_from("203.0.113.7", "/entities", report("urn:ngsi-ld:Report:x"))
                .await
                .status(),
            StatusCode::CREATED
        );
    }
    let mine = form
        .post_from("203.0.113.7", "/entities", report("urn:ngsi-ld:Report:x"))
        .await;
    assert_eq!(mine.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = axum::body::to_bytes(mine.into_body(), 1 << 16)
        .await
        .expect("a body");
    assert!(
        String::from_utf8_lossy(&body).contains("as many entries as one sender may today"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        form.post_from("198.51.100.4", "/entities", report("urn:ngsi-ld:Report:x"))
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(form.sent().len(), 3);
}
