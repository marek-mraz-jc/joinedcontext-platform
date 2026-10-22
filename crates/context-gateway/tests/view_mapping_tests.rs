//! T-0175: serving one space as somebody else's model (EP-54, DM-51, DM-52).
//!
//! Since T-3799 the broker's view inverts the caller's query and rebuilds every entity in the
//! target model (`crates/antares-views/tests/views/views_mapped.rs` in Antares: the target-model
//! answer, the filter in the source model, a non-invertible or unserved filter and a write
//! refused, `pick`/`omit` and `attrs` of a computed slot). What is left in the gateway is the
//! request-level case and the version check that decides which IR a gateway loads (T-4361,
//! T-4547); every slot rule is the broker's, tested in Antares `antares-ops/src/mapping/tests.rs`.

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
use context_gateway::view_mapping::{InvalidIr, ViewMapping};
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
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
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
fn a_document_without_a_version_is_refused() {
    for document in [json!([]), json!({}), json!({ "version": "2" })] {
        assert_eq!(
            ViewMapping::parse(&document),
            Err(InvalidIr::Malformed),
            "{document}"
        );
    }
    assert!(ViewMapping::parse(&ir()).is_ok());
}

/// The slug of the Endpoint `repository` writes, as a manifest must spell one (base32).
const LOADED: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";

/// A repository whose Endpoint names a Mapping with its gateway IR at `version`.
fn repository(version: u64) -> std::path::PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-view-mapping-{version}-{now}"));
    std::fs::create_dir_all(dir.join("mappings")).expect("create the repository");
    let mut document = ir();
    document["version"] = json!(version);
    for (name, body) in [
        (
            "space.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\n\
             metadata: { name: ovzdusie, namespace: banskabystrica }\n\
             spec: { isSandbox: false, urnSegment: ovzdusie }\n"
                .to_owned(),
        ),
        (
            "endpoint.yaml",
            format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\n\
                 metadata: {{ name: air-sdm, namespace: banskabystrica }}\n\
                 spec:\n  contextSpaceRef: ovzdusie\n  slug: {LOADED}\n  audience: public\n\
                 \x20 enabledRepresentations: [\"ngsi-ld\"]\n\
                 \x20 viewMappingRef: {{ kind: Mapping, name: air-to-sdm }}\n"
            ),
        ),
        (
            "mapping.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Mapping\n\
             metadata: { name: air-to-sdm, namespace: banskabystrica }\n\
             spec:\n  contextSpaceRef: ovzdusie\n\
             \x20 source: { kind: DataModel, name: ovzdusie-air, version: \"1\" }\n\
             \x20 target: { kind: DataModel, name: sdm-air, version: \"1\" }\n\
             \x20 transformation: { class_derivations: {} }\n\
             \x20 artifacts: { bloblang: mappings/air-to-sdm.blobl, gatewayIr: mappings/air-to-sdm.ir.json }\n\
             \x20 tests: [{ input: examples/in.jsonld, expect: examples/out.jsonld }]\n"
                .to_owned(),
        ),
        ("mappings/air-to-sdm.blobl", "root = this\n".to_owned()),
        ("mappings/air-to-sdm.ir.json", document.to_string()),
    ] {
        std::fs::write(dir.join(name), body).expect("write a file");
    }
    dir
}

/// Everything the gateway logs while `run` runs.
fn logged<T>(run: impl FnOnce() -> T) -> (T, String) {
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("a poisoned log"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let out = tracing::subscriber::with_default(subscriber, run);
    let text = String::from_utf8_lossy(&log.0.lock().expect("the log")).into_owned();
    (out, text)
}

/// T-4547: the gateway checks the IR's version when the repository loads and hands the rest to
/// the broker; an IR from a newer Model Tools is left out with the warning, and the Endpoint
/// stays as the space's own, never the source model under the target's name.
#[test]
fn an_unknown_ir_version_leaves_the_mapping_out_with_a_warning_and_version_2_loads() {
    let (loaded, log) = logged(|| context_gateway::store::load(&repository(3)));
    let (endpoints, ..) = loaded.expect("the repository loads");
    let endpoint = endpoints
        .iter()
        .find(|e| e.slug == LOADED)
        .unwrap_or_else(|| panic!("the Endpoint stays: {log}"));
    assert!(endpoint.view_mapping.is_none());
    assert!(log.contains("version 3"), "{log}");
    assert!(log.contains("not usable"), "{log}");

    let (loaded, log) = logged(|| context_gateway::store::load(&repository(2)));
    let (endpoints, ..) = loaded.expect("the repository loads");
    let endpoint = endpoints
        .iter()
        .find(|e| e.slug == LOADED)
        .expect("the Endpoint");
    assert!(endpoint.view_mapping.is_some(), "{log}");
    assert!(!log.contains("not usable"), "{log}");
}
