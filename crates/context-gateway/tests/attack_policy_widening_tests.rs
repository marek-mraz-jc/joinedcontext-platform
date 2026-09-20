//! Attack vector: a caller widens what a Policy allows, through a filter language that is not
//! NGSI-LD's own (T-1696; EP-74, GW33, EP-35, MP-02, R9).
//!
//! `filter_oracle_tests.rs` plays this attack on the NGSI-LD surface: `q`, `attrs`, `orderBy`,
//! `geoproperty` and the selectors of a batch query body are all held to what the endpoint may
//! serve, so filtering on a hidden or foreign attribute considers no entity at all instead of
//! answering with the rows that matched. The OGC and SensorThings surfaces take their filter in
//! another language — CQL2-text in `filter`, an OData-ish expression in `$filter` — and both are
//! compiled into an NGSI-LD `q`. This file is the same attack written in those two languages.
//!
//! The property each case asserts is equality, not emptiness: the same request with two different
//! values for the hidden attribute has to give the same answer and ask the broker the same
//! question, because an answer that differs by the value is the value. The two control cases are
//! there so the file fails when the defence is switched off: a filter the endpoint does serve is
//! compiled and does reach the broker.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "4hq2vbn7kzxr3m5tjw6cyd9pfs";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";

const VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: User
      slots: [age, name]
    - name: Vehicle
      slots: [weight, location]
"#;

/// Every entity carries every attribute, so anything that leaks is visible in the answer.
fn everything() -> Value {
    json!([
        {
            "id": "urn:ngsi-ld:User:hel.fi:fleet:aino",
            "type": "User",
            "name": { "type": "Property", "value": "Aino" },
            "age": { "type": "Property", "value": 41 },
            "secretPin": { "type": "Property", "value": "1234" }
        },
        {
            "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
            "type": "Vehicle",
            "weight": { "type": "Property", "value": 12_000 },
            "age": { "type": "Property", "value": 7 },
            "location": {
                "type": "GeoProperty",
                "value": { "type": "Point", "coordinates": [24.9, 60.2] }
            },
            "secretPin": { "type": "Property", "value": "4321" }
        }
    ])
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
            axum::Json(everything())
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
         operations: [queryEntity, retrieveEntity, queryBatch, queryTemporal]\n"
    ))
    .expect("the policy spec parses")
}

/// One endpoint serving all three read surfaces, projected, with `secretPin` hidden (EP-61).
fn endpoint() -> Endpoint {
    let parsed = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(VIEW).expect("parses");
    parsed.validate().expect("valid");
    Endpoint {
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::OgcFeatures,
            Representation::Sta,
        ],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: ["secretPin".to_owned()].into_iter().collect(),
        projection: Some(Arc::new(parsed.spec)),
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["User".to_owned(), "Vehicle".to_owned()],
            json_schema: None,
            context: None,
        }],
        policies: vec![policy()],
    }
}

/// One read, with the status, the answer and every query string the broker was asked.
async fn ask(uri: &str) -> (StatusCode, Value, Vec<String>) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/endpoint/{SLUG}{uri}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let asked = hops.lock().expect("the hop log").clone();
    (status, body, asked)
}

/// An answer with the two members that differ for reasons the attack cannot use: the `links` a
/// surface builds from the caller's own request, which echo the query back, and the timestamp the
/// page is stamped with. A caller learns nothing from reading their own filter or the clock.
fn comparable(body: &Value) -> Value {
    match body {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .filter(|(name, _)| !matches!(name.as_str(), "links" | "timeStamp"))
                .map(|(name, value)| (name.clone(), comparable(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(comparable).collect()),
        other => other.clone(),
    }
}

/// The same request under two values of the hidden attribute: the caller may learn nothing from
/// the difference, so there has to be none — in the answer or in what the broker was asked.
async fn indistinguishable(one: &str, other: &str) {
    let (first_status, first_body, first_asked) = ask(one).await;
    let (second_status, second_body, second_asked) = ask(other).await;
    assert_eq!(first_status, second_status, "{one} vs {other}");
    assert_eq!(
        comparable(&first_body),
        comparable(&second_body),
        "{one} vs {other}"
    );
    assert_eq!(first_asked, second_asked, "{one} vs {other}");
    for asked in first_asked.iter().chain(second_asked.iter()) {
        assert!(
            !asked.contains("secretPin"),
            "a hidden name reached the broker inside a selector: {asked}"
        );
    }
    let text = comparable(&first_body).to_string();
    for value in ["1234", "4321"] {
        assert!(
            !text.contains(value),
            "a hidden value is in the answer to {one}: {text}"
        );
    }
}

/// EP-61, T-1862: CQL2 is compiled into `q` before the enforcement point decides, so a hidden
/// name in a `filter` is the hidden name in a `q` — and bisecting it learns nothing.
#[tokio::test]
async fn a_cql2_filter_on_a_hidden_attribute_is_not_an_oracle() {
    indistinguishable(
        "/ogc/features/collections/Vehicle/items?filter=secretPin%3D%274321%27",
        "/ogc/features/collections/Vehicle/items?filter=secretPin%3D%279999%27",
    )
    .await;
    let (status, _, asked) =
        ask("/ogc/features/collections/Vehicle/items?filter=secretPin%3D%274321%27").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an oracle is closed by emptiness, not by an error"
    );
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}

/// The same for an `OR`: a branch still decides whether a row comes back, so the whole filter is
/// judged by every name it mentions.
#[tokio::test]
async fn a_hidden_name_in_an_or_branch_of_a_cql2_filter_is_not_an_oracle() {
    indistinguishable(
        "/ogc/features/collections/Vehicle/items?filter=weight%3E1%20OR%20secretPin%3D%274321%27",
        "/ogc/features/collections/Vehicle/items?filter=weight%3E1%20OR%20secretPin%3D%279999%27",
    )
    .await;
}

/// MP-02: `age` is a `User`'s slot in this projection, so a `Vehicle` may not be selected on it.
#[tokio::test]
async fn a_cql2_filter_on_an_attribute_of_another_type_selects_no_vehicle() {
    let (status, body, _) = ask("/ogc/features/collections/Vehicle/items?filter=age%3E5").await;
    assert_eq!(status, StatusCode::OK);
    let features = body["features"].as_array().cloned().unwrap_or_default();
    assert!(
        features.iter().all(
            |feature| feature["id"].as_str() != Some("urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01")
        ),
        "a vehicle was selected on an attribute this endpoint does not serve for it: {body}"
    );
    assert!(
        !body.to_string().contains("4321"),
        "the vehicle's hidden value is in the answer: {body}"
    );
}

/// The control: a filter the endpoint does serve is compiled and does reach the broker. Without
/// it this file would pass just as well with the whole surface refusing everything.
#[tokio::test]
async fn a_cql2_filter_the_endpoint_serves_is_compiled_and_forwarded() {
    let (status, body, asked) =
        ask("/ogc/features/collections/Vehicle/items?filter=weight%3E100").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(asked.len(), 1, "the broker was asked {asked:?}");
    assert!(
        asked[0].contains("q=%28weight%3E100%29"),
        "the compiled filter did not reach the broker: {}",
        asked[0]
    );
    assert!(
        body["features"]
            .as_array()
            .is_some_and(|features| !features.is_empty()),
        "the control returned nothing, so the other cases prove nothing: {body}"
    );
}

/// EP-12, EP-61: SensorThings takes its filter in `$filter`, and it is compiled into the same `q`.
#[tokio::test]
async fn a_sensorthings_filter_on_a_hidden_attribute_is_not_an_oracle() {
    indistinguishable(
        "/sta/v1.1/Things?$filter=secretPin%20eq%20%274321%27",
        "/sta/v1.1/Things?$filter=secretPin%20eq%20%279999%27",
    )
    .await;
}

/// The SensorThings control.
#[tokio::test]
async fn a_sensorthings_filter_the_endpoint_serves_is_compiled_and_forwarded() {
    let (status, body, asked) = ask("/sta/v1.1/Things?$filter=weight%20gt%20100").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(asked.len(), 1, "the broker was asked {asked:?}");
    assert!(
        asked[0].contains("q=%28weight%3E100%29"),
        "the compiled filter did not reach the broker: {}",
        asked[0]
    );
    assert!(
        body["value"]
            .as_array()
            .is_some_and(|value| !value.is_empty()),
        "the control returned nothing, so the other cases prove nothing: {body}"
    );
}

/// A parameter this surface does not implement is not a selector the broker gets to see: an OGC
/// queryable named after a hidden attribute changes neither the answer nor the question.
#[tokio::test]
async fn a_queryable_this_surface_does_not_implement_never_reaches_the_broker() {
    indistinguishable(
        "/ogc/features/collections/Vehicle/items?secretPin=4321",
        "/ogc/features/collections/Vehicle/items?secretPin=9999",
    )
    .await;
}
