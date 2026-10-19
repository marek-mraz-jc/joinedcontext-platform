//! `geoproperty` and `geometryProperty`, on both surfaces, each with the rule that keeps it
//! narrow (T-2299; AG-84, AG-85, GW2, R9, R22, EP-25).
//!
//! Both parameters move the attribute a decision is taken on, which is why neither is a
//! passthrough: a grant's area was drawn on one GeoProperty, and a GeoJSON `geometry` is an
//! attribute's own value with the answer narrowing no longer able to see it. What is worth a
//! test: the refusal happens before the broker is asked, it names the parameter, the tool
//! refuses in the same words as the REST route, and the harmless case still reaches the broker.

mod common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const PROJECT: &str = "helsinki";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";

/// A geo query of the caller's own, inside that area.
const CALLER_AREA: &str =
    "georel=within&geometry=Polygon&coordinates=[[[19.12,48.72],[19.15,48.72],[19.15,48.74],[19.12,48.74],[19.12,48.72]]]";

type Hops = Arc<Mutex<Vec<String>>>;

fn bus() -> Value {
    json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
        "type": "Vehicle",
        "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.13, 48.73] } },
        "homeLocation": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.90, 48.99] } },
    })
}

/// A broker that records the query string of every request it is asked, so a test can prove
/// it was never asked at all.
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
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    (format!("http://{address}"), hops)
}

/// A public read grant, with whatever extra constraint lines the case needs.
fn policy(extra: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         {extra}"
    ))
    .expect("the policy spec parses")
}

fn endpoint(policy: PolicySpec) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

async fn serve(endpoint: Endpoint) -> (Router, Hops) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint]),
    );
    (router(gateway), hops)
}

async fn answered(app: Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// One NGSI-LD read over the REST surface.
async fn rest(endpoint: Endpoint, query: &str) -> (StatusCode, Value, Vec<String>) {
    let (app, hops) = serve(endpoint).await;
    let (status, body) = answered(
        app,
        Request::builder()
            .method(Method::GET)
            .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities?{query}"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    let asked = hops.lock().expect("the hop log").clone();
    (status, body, asked)
}

/// Where no grant draws an area, nothing has been written for one GeoProperty rather than
/// another: the caller's own pair goes upstream together and narrows only itself.
#[tokio::test]
async fn a_geoproperty_without_a_granted_area_reaches_the_broker_beside_the_geo_query() {
    let (status, body, asked) = rest(
        endpoint(policy("")),
        &format!("type=Vehicle&geoproperty=homeLocation&{CALLER_AREA}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = asked.first().expect("the broker was asked");
    assert!(
        sent.contains("geoproperty=homeLocation"),
        "the caller's GeoProperty went with its area: {sent}"
    );
    assert!(
        sent.contains("georel=within"),
        "and so did the area: {sent}"
    );
}
