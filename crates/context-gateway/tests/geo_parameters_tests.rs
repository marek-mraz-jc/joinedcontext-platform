//! `geoproperty` and `geometryProperty`, on both surfaces, each with the rule that keeps it
//! narrow (T-2299; AG-84, AG-85, GW2, R9, R22, EP-25).
//!
//! Both parameters move the attribute a decision is taken on, which is why neither is a
//! passthrough: a grant's area was drawn on one GeoProperty, and a GeoJSON `geometry` is an
//! attribute's own value with the answer narrowing no longer able to see it. What is worth a
//! test: the refusal happens before the broker is asked, it names the parameter, the tool
//! refuses in the same words as the REST route, and the harmless case still reaches the broker.

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

/// The area a grant draws, in the one form a `Policy` writes it: no GeoProperty is named, so
/// it means CIM 009's default, `location`.
const GRANTED_AREA: &str =
    "georel=within;geometry=Polygon;coordinates=[[[19.10,48.70],[19.20,48.70],[19.20,48.76],[19.10,48.76],[19.10,48.70]]]";

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
        let _ = axum::serve(listener, app).await;
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
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: PROJECT.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned()],
            json_schema: None,
            context: None,
        }],
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

/// The same read as a `query_entities` tool call.
async fn mcp(endpoint: Endpoint, arguments: Value) -> (Value, Vec<String>) {
    let (app, hops) = serve(endpoint).await;
    let payload = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "query_entities", "arguments": arguments }
    });
    let (_, body) = answered(
        app,
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/endpoint/{SLUG}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(payload.to_string()))
            .expect("a request"),
    )
    .await;
    let asked = hops.lock().expect("the hop log").clone();
    (body["result"].clone(), asked)
}

/// The words a tool refusal carries.
fn tool_refusal(result: &Value) -> String {
    assert_eq!(result["isError"], json!(true), "not a refusal: {result}");
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// AG-85, GW2: the area a grant draws is drawn on `location`. A caller that points the geo
/// query at another GeoProperty would have that polygon tested against an attribute it was
/// never written for — an entity whose `location` is outside the area and whose
/// `homeLocation` is inside it would come back — so the request is refused by name.
#[tokio::test]
async fn a_geoproperty_beside_a_grants_own_area_is_refused_by_name() {
    let (status, body, asked) = rest(
        endpoint(policy(&format!("geoQ: \"{GRANTED_AREA}\"\n"))),
        "type=Vehicle&geoproperty=homeLocation",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("geoproperty:")),
        "the refusal names the parameter: {body}"
    );
    assert!(asked.is_empty(), "the broker was asked anyway: {asked:?}");
}

/// The same grant, the same request, over MCP: one rule in the shared handler, so the two
/// surfaces answer the same words (AG-84, the parity rule of T-1860).
#[tokio::test]
async fn the_tool_refuses_a_geoproperty_beside_a_grants_area_in_the_same_words() {
    let grant = format!("geoQ: \"{GRANTED_AREA}\"\n");
    let (_, rest_body, _) = rest(
        endpoint(policy(&grant)),
        "type=Vehicle&geoproperty=homeLocation",
    )
    .await;
    let (result, asked) = mcp(
        endpoint(policy(&grant)),
        json!({ "type": "Vehicle", "geoproperty": "homeLocation" }),
    )
    .await;

    let words = tool_refusal(&result);
    assert!(
        words.contains(rest_body["detail"].as_str().unwrap_or("|")),
        "the tool says {words}, the route says {}",
        rest_body["detail"]
    );
    assert!(asked.is_empty(), "the broker was asked anyway: {asked:?}");
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

/// A `geoproperty` with no geo query selects nothing and would only offer a grant's own area a
/// second property to be tested against, so it never rides alone.
#[tokio::test]
async fn a_geoproperty_alone_is_not_forwarded() {
    let (status, body, asked) = rest(
        endpoint(policy("")),
        "type=Vehicle&geoproperty=homeLocation",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = asked.first().expect("the broker was asked");
    assert!(
        !sent.contains("geoproperty"),
        "a GeoProperty with no geo query reached the broker: {sent}"
    );
}

/// R9, AG-85: the value the broker copies into a GeoJSON `geometry` is not that attribute any
/// more, so the answer narrowing cannot take it back. The attribute has to be one the grant
/// covers before the request is sent, and a request for one it does not cover is a bad
/// request rather than an empty answer.
#[tokio::test]
async fn a_geometry_property_naming_a_hidden_attribute_is_refused_by_name() {
    let narrow =
        "information:\n  - entities:\n      - type: Vehicle\n    propertyNames: [location]\n";
    let (status, body, asked) = rest(
        endpoint(policy(narrow)),
        "type=Vehicle&geometryProperty=homeLocation",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("geometryProperty:")),
        "the refusal names the parameter: {body}"
    );
    assert!(asked.is_empty(), "the broker was asked anyway: {asked:?}");

    // The granted one passes and reaches the broker.
    let (status, body, asked) = rest(
        endpoint(policy(narrow)),
        "type=Vehicle&geometryProperty=location",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        asked
            .first()
            .is_some_and(|sent| sent.contains("geometryProperty=location")),
        "the granted GeoProperty went upstream: {asked:?}"
    );
}

/// The MCP mirror of the same rule, over the same grant (AG-84).
#[tokio::test]
async fn the_tool_refuses_a_geometry_property_it_does_not_serve_in_the_same_words() {
    let narrow =
        "information:\n  - entities:\n      - type: Vehicle\n    propertyNames: [location]\n";
    let (_, rest_body, _) = rest(
        endpoint(policy(narrow)),
        "type=Vehicle&geometryProperty=homeLocation",
    )
    .await;
    let (result, asked) = mcp(
        endpoint(policy(narrow)),
        json!({ "type": "Vehicle", "geometryProperty": "homeLocation" }),
    )
    .await;

    let words = tool_refusal(&result);
    assert!(
        words.contains(rest_body["detail"].as_str().unwrap_or("|")),
        "the tool says {words}, the route says {}",
        rest_body["detail"]
    );
    assert!(asked.is_empty(), "the broker was asked anyway: {asked:?}");
}

/// AG-21, T-2299: `accept` and `context` are in the contract's table and served by neither
/// surface, so the tool refuses them as unknown arguments rather than dropping them silently.
#[tokio::test]
async fn accept_and_context_are_still_refused_as_unknown_arguments() {
    for argument in ["accept", "context"] {
        let (result, asked) = mcp(
            endpoint(policy("")),
            json!({ "type": "Vehicle", argument: "json-ld" }),
        )
        .await;

        let words = tool_refusal(&result);
        assert!(
            words.contains(argument),
            "`{argument}` is refused by name: {words}"
        );
        assert!(asked.is_empty(), "the broker was asked anyway: {asked:?}");
    }
}
