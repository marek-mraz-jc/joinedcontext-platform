//! Attack vector: a caller widens what a Policy allows, through a filter language that is not
//! NGSI-LD's own (T-1696; EP-74, GW33, EP-35, MP-02, R9).
//!
//! `filter_oracle_tests.rs` plays this attack on the NGSI-LD surface: `q`, `attrs`, `orderBy`,
//! `geoproperty` and the selectors of a batch query body are all held to what the endpoint may
//! serve, so filtering on a hidden or foreign attribute considers no entity at all instead of
//! answering with the rows that matched. The SensorThings surface takes its filter in another
//! language, an OData-ish expression in `$filter`, compiled into an NGSI-LD `q`; this file is the
//! same attack written in it. The OGC surface is the broker's (T-3752): its CQL2 `filter` is held
//! to the view there (Antares `crates/antares-ogc/tests/ogc/filter.rs`,
//! `hidden_and_unknown_property_are_the_same_400` and `a_filter_never_widens_the_policy`).
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
            recorder.lock().expect("the hop log").push(format!(
                "{}?{}",
                request.uri().path(),
                request.uri().query().unwrap_or_default()
            ));
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
        declared_types: None,
        roles: Default::default(),
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
        creates: None,
        file_limits: None,
        hidden_attributes: ["secretPin".to_owned()].into_iter().collect(),
        projection: Some(Arc::new(parsed.spec)),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["User".to_owned(), "Vehicle".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
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

/// EP-12, EP-61: SensorThings is the broker's `antares-sta` surface of the Endpoint's view since
/// T-4463: a `$filter` on a hidden attribute goes to that view as sent, whose engine considers no
/// entity for it (Antares `addons/joinedcontext-policy/tests/engine.rs`
/// `bisecting_a_hidden_value_learns_nothing`), and never to the broker's plain root.
#[tokio::test]
async fn a_sensorthings_filter_on_a_hidden_attribute_goes_to_the_view_alone() {
    for value in ["4321", "9999"] {
        let (_, _, asked) = ask(&format!(
            "/sta/v1.1/Things?$filter=secretPin%20eq%20%27{value}%27"
        ))
        .await;
        assert_eq!(
            asked,
            [format!(
                "/x/views/{SLUG}/sta/v1.1/Things?$filter=secretPin%20eq%20%27{value}%27"
            )]
        );
    }
}
