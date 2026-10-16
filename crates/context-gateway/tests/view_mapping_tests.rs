//! T-0175: serving one space as somebody else's model (EP-54, DM-51, DM-52).
//!
//! Since T-3799 the broker's view inverts the caller's query and rebuilds every entity in the
//! target model (`crates/antares-views/tests/views/views_mapped.rs` in Antares: the target-model
//! answer, the filter in the source model, a non-invertible or unserved filter and a write
//! refused, `pick`/`omit` and `attrs` of a computed slot). What is left in the gateway is the
//! request-level case and the parser that decides which IR a gateway loads (T-4361).

mod common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::translators::view_mapping::{InvalidIr, ViewMapping};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "v6h3zq8ntm2xc7wrb4sdk9jfp5";
const PROJECT: &str = "banskabystrica";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const STATION: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:senzor-01";

/// The IR Model Tools compiles: the local model on the left, Smart Data Models on the right.
fn ir() -> Value {
    json!({
        "version": 2,
        "sourceClass": "MestskySenzor",
        "targetClass": "AirQualityObserved",
        "slots": [
            { "target": "pm25", "source": "pm2p5", "kind": "rename", "filterable": true },
            {
                "target": "temperature", "source": "teplota", "kind": "unitConversion",
                "factor": 1.0, "offset": -273.15, "filterable": true
            },
            {
                "target": "reliability", "source": "spolahlivost", "kind": "valueMappings",
                "forward": { "vysoka": "high", "nizka": "low" },
                "inverse": { "high": "vysoka", "low": "nizka" },
                "filterable": true
            },
            {
                "target": "stationCount", "source": "pocet", "kind": "cast",
                "range": "integer", "filterable": true
            },
            { "target": "dataProvider", "kind": "constant", "value": "bb", "filterable": false },
            {
                // `nazov + " (" + spolahlivost + ")"`. A computed slot reads the SOURCE model:
                // the expression names `spolahlivost`, not the `reliability` the view serves.
                "target": "label", "kind": "expr", "filterable": false,
                "expression": {
                    "binary": "+",
                    "left": {
                        "binary": "+",
                        "left": { "slot": "nazov" }, "right": { "const": " (" }
                    },
                    "right": {
                        "binary": "+",
                        "left": { "slot": "spolahlivost" }, "right": { "const": ")" }
                    }
                }
            }
        ]
    })
}

/// The entity as the broker holds it, in the source model's own names.
fn stored() -> Value {
    json!({
        "id": STATION,
        "type": "MestskySenzor",
        "modifiedAt": "2026-09-07T08:00:00Z",
        "nazov": { "type": "Property", "value": "Senzor Fončorda" },
        "pm2p5": { "type": "Property", "value": 12.5 },
        "teplota": { "type": "Property", "value": 292.15 },
        "spolahlivost": { "type": "Property", "value": "vysoka" },
        "pocet": { "type": "Property", "value": "4" },
        "kalibracia": { "type": "Property", "value": 0.3 }
    })
}

/// One request the gateway made upstream.
#[derive(Debug, Clone, Default)]
struct Hop;

type Hops = Arc<Mutex<Vec<Hop>>>;

async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |_: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder.lock().expect("the hop log").push(Hop);
            axum::Json(json!([stored()]))
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

fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, createEntity, updateAttrs, deleteEntity]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(view: Option<ViewMapping>) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: view.map(Arc::new),
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        policies: vec![policy()],
    }
}

/// The same endpoint without a view mapping still writes: read-only is a property of the
/// view, not of the gateway.
#[tokio::test]
async fn an_endpoint_without_a_view_mapping_still_accepts_a_write() {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint(None)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::PATCH)
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{STATION}/attrs"
                ))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "pm2p5": { "type": "Property", "value": 1 } }).to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    assert_ne!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(!hops.lock().expect("the hop log").is_empty());
}

#[test]
fn an_ir_from_a_newer_model_tools_is_refused_rather_than_half_understood() {
    let mut newer = ir();
    newer["version"] = json!(3);
    assert_eq!(ViewMapping::parse(&newer), Err(InvalidIr::Version(3)));
}

#[test]
fn a_slot_kind_this_gateway_does_not_run_is_refused() {
    let refused = ViewMapping::parse(&json!({
        "version": 2,
        "sourceClass": "A",
        "targetClass": "B",
        "slots": [{ "target": "x", "source": "y", "kind": "join", "filterable": true }]
    }));
    assert!(
        matches!(refused, Err(InvalidIr::Slot { index: 0, ref reason }) if reason.contains("join")),
        "{refused:?}"
    );
}

fn slot_of(target: &str, expression: Value) -> Value {
    json!({ "target": target, "kind": "expr", "filterable": false, "expression": expression })
}

#[test]
fn a_computed_slot_without_its_expression_is_refused_rather_than_served_empty() {
    let refused = ViewMapping::parse(&json!({
        "version": 2,
        "sourceClass": "A",
        "targetClass": "B",
        "slots": [{ "target": "label", "kind": "expr", "filterable": false }]
    }));
    assert!(
        matches!(refused, Err(InvalidIr::Slot { index: 0, ref reason })
                 if reason.contains("no expression")),
        "{refused:?}"
    );
}

#[test]
fn an_expression_node_this_gateway_does_not_evaluate_is_refused() {
    for node in [
        json!({ "call": "upper", "operand": { "slot": "a" } }),
        json!({ "binary": "**", "left": { "slot": "a" }, "right": { "const": 2 } }),
        json!({ "binary": "+", "left": { "slot": "a" } }),
        json!({ "boolean": "xor", "operands": [] }),
        json!({ "unary": "~", "operand": { "slot": "a" } }),
        json!("a"),
    ] {
        let refused = ViewMapping::parse(&json!({
            "version": 2,
            "sourceClass": "A",
            "targetClass": "B",
            "slots": [slot_of("label", node.clone())]
        }));
        assert!(
            matches!(refused, Err(InvalidIr::Slot { index: 0, .. })),
            "{node} was accepted: {refused:?}"
        );
    }
}
