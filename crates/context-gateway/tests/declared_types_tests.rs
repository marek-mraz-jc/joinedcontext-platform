//! T-2699: a space's one model decides which types it takes (DM-61, ADR-N-033).
//!
//! The space names its model in `spec.dataModelRef`; a write of a type that model does not
//! declare is a 400 naming the type and the model, before the broker is asked. A space that
//! names no model yet is not narrowed, so a repository being migrated keeps working.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use context_gateway::store;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k7r2m4xq9vbn3tdw6hcy5pajfe";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";

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
            StatusCode::CREATED
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

/// The grant takes both types, so any refusal below is the model's and not the policy's.
fn both_types() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [createEntity, upsertBatch]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20     - type: Device\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(declared: Option<&[&str]>) -> Endpoint {
    Endpoint {
        declared_types: declared.map(|classes| DeclaredTypes {
            model: "bb-air-quality".into(),
            classes: classes.iter().map(|class| (*class).to_owned()).collect(),
            units: Default::default(),
        }),
        catalog: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
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
        policies: vec![both_types()],
    }
}

fn entity(kind: &str, local: &str) -> Value {
    json!({
        "id": format!("urn:ngsi-ld:{kind}:{DOMAIN}:{SPACE}:{local}"),
        "type": kind,
        "name": { "type": "Property", "value": local }
    })
}

async fn send(
    declared: Option<&[&str]>,
    uri: &str,
    body: Value,
) -> (StatusCode, Value, Vec<String>) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .serve([endpoint(declared)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{uri}"))
                .header("content-type", "application/ld+json")
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body");
    let problem = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let asked = hops.lock().expect("the hop log").clone();
    (status, problem, asked)
}

#[tokio::test]
async fn a_type_the_model_lacks_is_a_400_naming_it_and_the_broker_is_not_asked() {
    let model: &[&str] = &["AirQualityObserved"];
    let (status, problem, asked) = send(Some(model), "/entities", entity("Device", "lamp-1")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("`Device`") && detail.contains("`bb-air-quality`"),
        "{problem}"
    );
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}

#[tokio::test]
async fn a_declared_type_reaches_the_broker() {
    let model: &[&str] = &["AirQualityObserved"];
    let (status, _, asked) = send(
        Some(model),
        "/entities",
        entity("AirQualityObserved", "station-1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(asked.len(), 1, "{asked:?}");
}

#[tokio::test]
async fn a_batch_with_one_undeclared_type_is_refused_whole() {
    let model: &[&str] = &["AirQualityObserved"];
    let (status, problem, asked) = send(
        Some(model),
        "/entityOperations/upsert",
        json!([
            entity("AirQualityObserved", "station-1"),
            entity("Device", "lamp-1")
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(problem["detail"]
        .as_str()
        .unwrap_or_default()
        .contains("`Device`"));
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}

#[tokio::test]
async fn a_space_that_names_no_model_is_not_narrowed() {
    let (status, _, asked) = send(None, "/entities", entity("Device", "lamp-1")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(asked.len(), 1);
}

#[test]
fn an_expanded_or_compacted_type_names_the_same_class() {
    let declared = DeclaredTypes {
        model: "m".into(),
        classes: ["AirQualityObserved".to_owned()].into(),
        units: Default::default(),
    };
    for written in [
        "AirQualityObserved",
        "https://smartdatamodels.org/dataModel.Environment/AirQualityObserved",
        "https://example.org/terms#AirQualityObserved",
        "sdm:AirQualityObserved",
    ] {
        assert!(declared.declares(written), "{written}");
    }
    for written in [
        "Device",
        "",
        "https://example.org/AirQuality",
        "AirQualityObservedX",
    ] {
        assert!(!declared.declares(written), "{written}");
    }
}

const SPACE_MANIFEST: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
  urnSegment: ovzdusie
"#;

const ENDPOINT_MANIFEST: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

const MODEL_MANIFEST: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: bb-air-quality
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  linkml: bb-air-quality.linkml.yaml
  version: 1.0.0
  lifecycle: draft
  classes: [AirQualityObserved, Device]
"#;

fn repo(test: &str, space: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-declared-{test}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    write(&dir, "space.yaml", space);
    write(&dir, "endpoint.yaml", ENDPOINT_MANIFEST);
    write(&dir, "model.yaml", MODEL_MANIFEST);
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn declared_of(dir: &Path) -> (Option<DeclaredTypes>, Option<DeclaredTypes>) {
    let (endpoints, spaces, ..) = store::load(dir).expect("the repository loads");
    let endpoint = endpoints
        .iter()
        .find(|endpoint| endpoint.slug == "zt4qm7ge2xdv6ksb3ncf5arw2y")
        .expect("the endpoint");
    let space = spaces.first().expect("the space");
    (
        endpoint.declared_types.clone(),
        space.endpoint.declared_types.clone(),
    )
}

#[test]
fn the_table_narrows_a_space_to_the_model_it_names_and_only_then() {
    let named = SPACE_MANIFEST.replace(
        "urnSegment: ovzdusie",
        "urnSegment: ovzdusie\n  dataModelRef: { kind: DataModel, name: bb-air-quality }",
    );
    let (endpoint, space) = declared_of(&repo("named", &named));
    let expected = DeclaredTypes {
        model: "bb-air-quality".into(),
        classes: ["AirQualityObserved".to_owned(), "Device".to_owned()].into(),
        units: Default::default(),
    };
    assert_eq!(endpoint.as_ref(), Some(&expected));
    assert_eq!(
        space.as_ref(),
        Some(&expected),
        "the /cs surface is narrowed the same way"
    );

    assert_eq!(declared_of(&repo("unnamed", SPACE_MANIFEST)), (None, None));

    let dangling = SPACE_MANIFEST.replace(
        "urnSegment: ovzdusie",
        "urnSegment: ovzdusie\n  dataModelRef: { kind: DataModel, name: gone }",
    );
    assert_eq!(declared_of(&repo("dangling", &dangling)), (None, None));
}
