//! A download of a space holding many types (T-4078, T-4456).
//!
//! An unselected download asks the Endpoint's view for every type the space holds, in one
//! request: since T-4079 and T-4455 the broker's view composes per type and leaves out a type
//! it refuses the caller, refusing the read only when it refuses every type named. The stub here
//! is that view, over the 21 types of the Helsinki space. The view, not the gateway, decides
//! which types a caller reads (T-3652).

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

/// The view: `shown` lists the types it answers this caller, every one when `None`.
async fn view_broker(shown: Option<&'static [&'static str]>) -> (String, Asked) {
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
            let read: Vec<&String> = types
                .iter()
                .filter(|name| shown.is_none_or(|shown| shown.contains(&name.as_str())))
                .collect();
            if read.is_empty() {
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
            let rows: Vec<Value> = read
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

/// Every download of the 21-type space answers with the rows of every type, read in one
/// request that names all of them (T-4456).
#[tokio::test]
async fn a_download_of_a_space_of_many_types_holds_every_type() {
    for file in ["file.geojson", "file.csv", "file.xlsx", "file.zip"] {
        let (broker, asked) = view_broker(None).await;
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
        assert_eq!(asked.len(), 1, "{file}: {asked:?}");
        assert_eq!(asked[0].len(), TYPES, "{file}: {asked:?}");
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

/// A view that shows 2 of the 21 types is asked once, and the download holds the 2 it shows: a
/// type it refuses is left out, not a refusal of the whole file (T-3652).
#[tokio::test]
async fn a_type_the_view_refuses_is_left_out_of_the_download() {
    let (broker, asked) = view_broker(Some(&["Type03", "Type17"])).await;
    let (status, body) = get(
        gateway(&broker, endpoint(&[])),
        &format!("/api/endpoint/{SLUG}/file.geojson"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    let collection: Value = serde_json::from_slice(&body).expect("a FeatureCollection");
    assert_eq!(collection["features"].as_array().map(Vec::len), Some(6));
}

/// A view that refuses the caller every type refuses the download and the page, as it refuses
/// the caller's own query.
#[tokio::test]
async fn a_view_refusing_every_type_refuses_the_read() {
    for path in ["file.csv", "sta/v1.1/Things?$top=5"] {
        let (broker, _) = view_broker(Some(&[])).await;
        let (status, body) = get(
            gateway(&broker, endpoint(&[])),
            &format!("/api/endpoint/{SLUG}/{path}"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

/// An Endpoint whose slug no view id can hold has no view, and nothing decides for it: its
/// downloads and pages answer `503 no-view` and the broker is not asked (T-3652).
#[tokio::test]
async fn an_endpoint_without_a_view_answers_no_view() {
    let long = "a".repeat(64);
    for path in ["file.csv", "sta/v1.1/Things"] {
        let (broker, asked) = view_broker(None).await;
        let mut endpoint = endpoint(&[]);
        endpoint.slug = long.clone();
        endpoint.base_path = format!("/api/endpoint/{long}");
        let (status, body) = get(
            gateway(&broker, endpoint),
            &format!("/api/endpoint/{long}/{path}"),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        let problem: Value = serde_json::from_slice(&body).expect("a problem");
        assert!(
            problem["type"]
                .as_str()
                .is_some_and(|t| t.ends_with("no-view")),
            "{path}: {problem}"
        );
        assert!(asked.lock().expect("no poisoned lock").is_empty(), "{path}");
    }
}

/// A Policy that names no type the space holds narrows nothing: the view is still asked, once,
/// and decides.
#[tokio::test]
async fn a_grant_naming_no_held_type_leaves_the_list_to_the_view() {
    let (broker, asked) = view_broker(None).await;
    let (status, _) = get(
        gateway(&broker, endpoint(&["Elsewhere"])),
        &format!("/api/endpoint/{SLUG}/file.csv"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
}

/// A single page through the view (the STA surface's read) is one request with the caller's
/// window: rows 59 to 63 of 63 span two types.
#[tokio::test]
async fn a_page_of_a_space_of_many_types_spans_the_types_in_order() {
    let (broker, asked) = view_broker(None).await;
    let (status, body) = get(
        gateway(&broker, endpoint(&[])),
        &format!("/api/endpoint/{SLUG}/sta/v1.1/Things?$top=5&$skip=59"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].len(), TYPES, "{asked:?}");
    let answer: Value = serde_json::from_slice(&body).expect("an STA collection");
    assert_eq!(
        answer["value"].as_array().map(Vec::len),
        Some(4),
        "{answer}"
    );
}
