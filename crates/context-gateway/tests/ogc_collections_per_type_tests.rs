//! OGC `/collections` of a space whose first page holds one type only (T-4104, EP-31).
//!
//! On dev praha-mesto listed two of its six located types: the collections came from one page of
//! a thousand entities across every type, and the broker's first thousand were of two types. The
//! stub here answers a page across several types with a thousand entities of the first one, as
//! the broker did, and each type asked alone with its own entities.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";

fn entity(typ: &str, n: usize, located: bool) -> Value {
    let mut e = json!({ "id": format!("urn:ngsi-ld:{typ}:praha:{n}"), "type": typ });
    if located {
        e["location"] = json!({ "type": "GeoProperty", "value": { "type": "Point", "coordinates": [14.42, 50.08] } });
    }
    e
}

type Asked = Arc<Mutex<Vec<String>>>;

/// WasteContainer: 1 200 entities, no location. OffStreetParking and CityDistrict: located.
async fn broker() -> (String, Asked) {
    let asked: Asked = Arc::default();
    let recorder = Arc::clone(&asked);
    let app = Router::new().fallback(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let query = request.uri().query().unwrap_or_default().to_owned();
            recorder
                .lock()
                .expect("no poisoned lock")
                .push(query.clone());
            let types = query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(name, _)| *name == "type")
                .map(|(_, value)| value.replace("%2C", ","))
                .unwrap_or_default();
            let rows: Vec<Value> = match types.split(',').next().unwrap_or_default() {
                // Asked across types, or for the first alone, the page is all WasteContainer.
                "WasteContainer" | "" => (0..1000)
                    .map(|n| entity("WasteContainer", n, false))
                    .collect(),
                "OffStreetParking" if !types.contains(',') => {
                    vec![entity("OffStreetParking", 1, true)]
                }
                "CityDistrict" if !types.contains(',') => vec![entity("CityDistrict", 1, true)],
                _ => Vec::new(),
            };
            axum::Json(Value::Array(rows)).into_response()
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

fn endpoint(information: &str) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "praha".to_owned(),
        project: "praha".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::OgcFeatures],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "praha".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec![
                "WasteContainer".to_owned(),
                "OffStreetParking".to_owned(),
                "CityDistrict".to_owned(),
            ],
            json_schema: None,
            context: None,
        }],
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        policies: vec![serde_norway::from_str(&format!(
            "contextSpaceRef: praha\n\
             assigner: did:web:praha.eu\n\
             assignee: {{ kind: role, id: public }}\n\
             operations: [queryEntity, retrieveEntity]\n\
             {information}"
        ))
        .expect("the policy spec parses")],
    }
}

async fn get(broker: &str, endpoint: Endpoint, path: &str) -> (StatusCode, Value) {
    let app = router(Arc::new(
        Gateway::new(Broker::new(broker), Box::new(PolicyPdp), "praha.eu").serve([endpoint]),
    ));
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/ogc/features{path}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn ids(answer: &Value) -> Vec<&str> {
    answer["collections"]
        .as_array()
        .expect("a collection list")
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect()
}

#[tokio::test]
async fn every_located_type_is_a_collection_whatever_the_first_page_holds() {
    let (broker, asked) = broker().await;
    let (status, answer) = get(&broker, endpoint(""), "/collections").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    // Sorted, located only: WasteContainer carries no geometry and is no Feature Collection.
    assert_eq!(
        ids(&answer),
        ["CityDistrict", "OffStreetParking"],
        "{answer}"
    );
    // One bounded read per type, never a whole-space read.
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert_eq!(asked.len(), 3, "{asked:?}");
    assert!(
        asked
            .iter()
            .all(|q| q.contains("limit=100") && !q.contains("%2C")),
        "{asked:?}"
    );
}

#[tokio::test]
async fn one_collection_asks_for_its_type_alone() {
    let (broker, asked) = broker().await;
    let (status, answer) = get(&broker, endpoint(""), "/collections/OffStreetParking").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["id"], json!("OffStreetParking"), "{answer}");
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].contains("type=OffStreetParking"), "{asked:?}");
}

#[tokio::test]
async fn a_type_the_grant_does_not_reach_is_not_asked_and_not_listed() {
    let (broker, asked) = broker().await;
    let grant = "information:\n  - entities: [{ type: OffStreetParking }]\n";
    let (status, answer) = get(&broker, endpoint(grant), "/collections").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(ids(&answer), ["OffStreetParking"], "{answer}");
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert!(
        asked.iter().all(|q| !q.contains("CityDistrict")),
        "{asked:?}"
    );
}
