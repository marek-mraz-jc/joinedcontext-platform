//! A download of a space holding many types (T-4078).
//!
//! An unselected download asks the Endpoint's view for every type the space holds, and the
//! broker's view refuses a query whose types compose past its rule bound: on dev the 21 types
//! of the Helsinki space were a `403` for file.csv, .geojson, .xlsx and .zip of
//! helsinki-transport and helsinki-all, while every NGSI-LD read answered. The stub here is that
//! view: 21 types, and a refusal of any query naming 16 or more of them.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "ajaezssvx3bwqgejgepmq4ig3frgan4e";
const TYPES: usize = 21;
/// The fewest distinct types the stub's view refuses in one query, as dev's did.
const BOUND: usize = 16;

fn type_name(n: usize) -> String {
    format!("Type{n:02}")
}

/// Three entities of every type, so each type's rows are told apart from the others'.
fn entities_of(name: &str) -> Vec<Value> {
    (0..3)
        .map(|n| {
            json!({
                "id": format!("urn:ngsi-ld:{name}:hel.fi:helsinki:{n}"),
                "type": name,
                "label": { "type": "Property", "value": format!("{name} {n}") },
                "location": {
                    "type": "GeoProperty",
                    "value": { "type": "Point", "coordinates": [24.94, 60.17] }
                }
            })
        })
        .collect()
}

/// The types of every entity query the view was asked, one list per hop.
type Asked = Arc<Mutex<Vec<Vec<String>>>>;

fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

async fn view_broker() -> (String, Asked) {
    let asked: Asked = Arc::default();
    let recorder = Arc::clone(&asked);
    let app = Router::new().fallback(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            if path == "/ngsi-ld/v1/types" {
                let list: Vec<String> = (0..TYPES).map(type_name).collect();
                return axum::Json(json!({ "type": "EntityTypeList", "typeList": list }))
                    .into_response();
            }
            if path != format!("/x/views/{SLUG}/ngsi-ld/v1/entities") {
                return StatusCode::NOT_FOUND.into_response();
            }
            let types: Vec<String> = param(&query, "type")
                .unwrap_or_default()
                .replace("%2C", ",")
                .split(',')
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            recorder
                .lock()
                .expect("no poisoned lock")
                .push(types.clone());
            let distinct: BTreeSet<&String> = types.iter().collect();
            if distinct.len() >= BOUND {
                return (
                    StatusCode::FORBIDDEN,
                    axum::Json(json!({
                        "type": "https://uri.etsi.org/ngsi-ld/errors/Forbidden",
                        "title": "Access Denied by Policy",
                        "status": 403,
                    })),
                )
                    .into_response();
            }
            let number = |name: &str, default: usize| {
                param(&query, name)
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(default)
            };
            let rows: Vec<Value> = types
                .iter()
                .flat_map(|name| entities_of(name))
                .skip(number("offset", 0))
                .take(number("limit", 20))
                .collect();
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

/// The Helsinki shape: anonymous callers read, and the Policy names `granted` types or none.
fn endpoint(granted: &[&str]) -> Endpoint {
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
        space: "helsinki".to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::GeoJson,
            Representation::Csv,
            Representation::Xlsx,
            Representation::Zip,
            Representation::Sta,
        ],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        policies: vec![serde_norway::from_str(&format!(
            "contextSpaceRef: helsinki\n\
             assigner: did:web:hel.fi\n\
             assignee: {{ kind: role, id: public }}\n\
             operations: [queryEntity, retrieveEntity]\n\
             {information}"
        ))
        .expect("the policy spec parses")],
    }
}

fn gateway(broker: &str, endpoint: Endpoint) -> Router {
    router(Arc::new(
        Gateway::new(Broker::new(broker), Box::new(PolicyPdp), "hel.fi").serve([endpoint]),
    ))
}

async fn get(app: Router, path: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(path)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("a readable body");
    (status, body.to_vec())
}

/// Every download of the 21-type space answers, with the rows of every type, and no query the
/// view was asked names more than one type.
#[tokio::test]
async fn a_download_of_a_space_of_many_types_holds_every_type() {
    for file in ["file.geojson", "file.csv", "file.xlsx", "file.zip"] {
        let (broker, asked) = view_broker().await;
        let (status, body) = get(
            gateway(&broker, endpoint(&[])),
            &format!("/api/endpoint/{SLUG}/{file}"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{file}: {}",
            String::from_utf8_lossy(&body)
        );
        let asked = asked.lock().expect("no poisoned lock").clone();
        assert!(
            asked.iter().all(|types| types.len() == 1),
            "{file}: {asked:?}"
        );
        let covered: BTreeSet<&String> = asked.iter().flatten().collect();
        assert_eq!(covered.len(), TYPES, "{file}: {asked:?}");
        if file == "file.geojson" {
            let collection: Value = serde_json::from_slice(&body).expect("a FeatureCollection");
            assert_eq!(
                collection["features"].as_array().map(Vec::len),
                Some(TYPES * 3)
            );
        }
        if file == "file.csv" {
            let text = String::from_utf8_lossy(&body);
            for n in [0, 15, 16, 20] {
                assert!(
                    text.contains(&format!("{} 2", type_name(n))),
                    "{file}: {text}"
                );
            }
        }
    }
}

/// An Endpoint whose Policy grants 2 of the 21 types asks the view for those 2 and no other.
#[tokio::test]
async fn a_policy_granting_two_types_asks_the_view_for_those_two() {
    let (broker, asked) = view_broker().await;
    let (status, body) = get(
        gateway(&broker, endpoint(&["Type03", "Type17"])),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert_eq!(
        asked,
        vec![vec!["Type03".to_owned()], vec!["Type17".to_owned()]]
    );
    let collection: Value = serde_json::from_slice(&body).expect("a FeatureCollection");
    assert_eq!(collection["features"].as_array().map(Vec::len), Some(6));
}

/// A Policy that names no type the space holds narrows nothing: the view is still asked, type by
/// type, and decides.
#[tokio::test]
async fn a_grant_naming_no_held_type_leaves_the_list_to_the_view() {
    let (broker, asked) = view_broker().await;
    let (status, _) = get(
        gateway(&broker, endpoint(&["Elsewhere"])),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert_eq!(asked.len(), TYPES, "{asked:?}");
}

/// A single page through the view (the STA surface's read) cuts the caller's window out of the
/// types read in turn: rows 59 to 63 of 63 span two types and stop at the last one.
#[tokio::test]
async fn a_page_of_a_space_of_many_types_spans_the_types_in_order() {
    let (broker, asked) = view_broker().await;
    let (status, body) = get(
        gateway(&broker, endpoint(&[])),
        &format!("/api/endpoint/{SLUG}/sta/v1.1/Things?$top=5&$skip=59"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert!(asked.iter().all(|types| types.len() == 1), "{asked:?}");
    let answer: Value = serde_json::from_slice(&body).expect("an STA collection");
    assert_eq!(
        answer["value"].as_array().map(Vec::len),
        Some(4),
        "{answer}"
    );
    // The read stopped at the type that filled the window, the last one: none was asked twice.
    let distinct: BTreeSet<&Vec<String>> = asked.iter().collect();
    assert_eq!(distinct.len(), asked.len(), "{asked:?}");
}
