//! Writes wait for a verified domain (PF-41, Architecture/03 §3, T-2572).
//!
//! Under `JC_GATEWAY_DOMAIN_VERIFICATION=enforce` a write through a live router is refused with
//! `403` naming the Organization unless the Portal says its domain is verified, and the broker is
//! never asked: a write refused after the hop is a write that happened. Reads pass whatever the
//! state, and `report` refuses nothing.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::domain_gate::{DomainGate, Item, Mode};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const DEVICE: &str = "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7";

type Hops = Arc<Mutex<Vec<String>>>;

async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder.lock().expect("the hop log").push(format!(
                "{} {}",
                request.method(),
                request.uri().path()
            ));
            if request.method() == Method::GET {
                (
                    StatusCode::OK,
                    [("content-type", "application/json")],
                    "[]".to_owned(),
                )
            } else {
                (
                    StatusCode::CREATED,
                    [("content-type", "application/json")],
                    String::new(),
                )
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

/// Anonymous callers may write devices of this space, and read them back.
fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [createEntity, queryEntity, retrieveEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Device\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policies: vec![policy()],
    }
}

/// A gate in `mode` that has heard `state` for this organization, or nothing when `None`.
fn gate(mode: Mode, state: Option<&str>) -> Arc<DomainGate> {
    let gate = DomainGate::new(mode);
    if let Some(state) = state {
        gate.replace_at(
            vec![Item {
                organization: "bbsk".to_owned(),
                domain: DOMAIN.to_owned(),
                state: state.to_owned(),
            }],
            Instant::now(),
        );
    }
    Arc::new(gate)
}

async fn send(
    gate: Arc<DomainGate>,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, Vec<String>) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .gate_writes_on(gate)
            .serve([endpoint()]),
    );
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{uri}"));
    if body.is_some() {
        request = request.header("content-type", "application/ld+json");
    }
    let response = router(gateway)
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let problem = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let asked = hops.lock().expect("the hop log").clone();
    (status, problem, asked)
}

fn device() -> Value {
    json!({
        "id": DEVICE,
        "type": "Device",
        "status": { "type": "Property", "value": "on" },
        "@context": "https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld"
    })
}

/// PF-41: under `enforce`, a write to a space of an Organization whose domain is pending,
/// failed, or not known at all is refused with the Organization named, and the broker is never
/// asked.
#[tokio::test]
async fn enforce_refuses_a_write_until_the_domain_is_verified_and_never_asks_the_broker() {
    for state in [Some("pending"), Some("failed"), None] {
        let (status, problem, asked) = send(
            gate(Mode::Enforce, state),
            Method::POST,
            "/entities",
            Some(device()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{state:?}: {problem}");
        assert!(
            problem["type"]
                .as_str()
                .unwrap_or_default()
                .ends_with("/domain-not-verified"),
            "{problem}"
        );
        assert!(
            problem["detail"]
                .as_str()
                .unwrap_or_default()
                .contains(DOMAIN),
            "the refusal names the domain: {problem}"
        );
        assert!(
            asked.is_empty(),
            "{state:?}: the broker was asked: {asked:?}"
        );
    }
}

/// PF-41: a verified domain lets the same write through, reads pass whatever the state, and
/// `report` refuses nothing.
#[tokio::test]
async fn a_verified_domain_writes_reads_always_pass_and_report_refuses_nothing() {
    let (status, _, asked) = send(
        gate(Mode::Enforce, Some("verified")),
        Method::POST,
        "/entities",
        Some(device()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        asked.len(),
        1,
        "the write reached the broker once: {asked:?}"
    );

    let (status, problem, _) = send(
        gate(Mode::Enforce, Some("failed")),
        Method::GET,
        "/entities?type=Device",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a read is never refused: {problem}");

    let (status, _, asked) = send(
        gate(Mode::Report, Some("failed")),
        Method::POST,
        "/entities",
        Some(device()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(asked.len(), 1);
}
