//! EP-53, EP-68 (T-4419): an Endpoint publishes its approved Mappings under
//! `schema/v{major}/mappings/` (API/02 §7a, "Mappings").
//!
//! The repository holds one space, its model at major 1, a public Endpoint and six Mappings: one
//! complete and readable by the caller, one whose gateway IR was never committed, one that reads a
//! slot the caller's schema does not show, one with a native block, one with a `secretRef` in its
//! transformation, and one over a model the Endpoint does not serve. A broker answers the view's
//! narrowed JSON Schema (`pm10`, `pm25`, `location`) and its schema index. Only the first is
//! listed, served and named in the record; the index says something was left out, never which.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::handlers::mappings::reads;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tower::ServiceExt;

const SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";
const BASE: &str = "/api/endpoint/zt4qm7ge2xdv6ksb3ncf5arw2y";

const MANIFESTS: &[(&str, &str)] = &[
    (
        "space.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata: { name: ovzdusie, namespace: ovzdusie }
spec: { isSandbox: false, urnSegment: ovzdusie }
"#,
    ),
    (
        "endpoint.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata: { name: public-air, namespace: ovzdusie }
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#,
    ),
    (
        "model.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata: { name: ovzdusie-air, namespace: ovzdusie }
spec:
  contextSpaceRef: ovzdusie
  linkml: ovzdusie-air/model.linkml.yaml
  version: 1.0.0
  lifecycle: published
  classes: [AirQualityObserved]
"#,
    ),
];

/// A Mapping of the space from `source` with `transformation` and `extra` spec lines, its two
/// artifacts named beside it.
fn mapping(name: &str, source: &str, transformation: &str, extra: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Mapping
metadata: {{ name: {name}, namespace: ovzdusie, title: {{ en: "{name} to SDM" }} }}
spec:
  contextSpaceRef: ovzdusie
  source: {{ kind: DataModel, name: {source}, version: "1" }}
  target: {{ kind: DataModel, name: sdm-air, version: "1" }}
  transformation:
{transformation}
  artifacts:
    bloblang: mappings/{name}.blobl
    gatewayIr: mappings/{name}.ir.json
  tests:
    - {{ input: examples/in.jsonld, expect: examples/out.jsonld }}
{extra}"#
    )
}

const READS_SHOWN: &str = r#"    id: https://example.org/mappings/air-to-sdm
    class_derivations:
      AirQualityObservedSdm:
        populated_from: AirQualityObserved
        slot_derivations:
          pm10: { populated_from: pm10 }
          pm2p5: { expr: "round(pm25 * 1.5, 1)" }
          location: {}"#;

const READS_HIDDEN: &str = r#"    class_derivations:
      AirQualityObservedSdm:
        populated_from: AirQualityObserved
        slot_derivations:
          pm10: { populated_from: pm10 }
          contact: { populated_from: operatorPhone }"#;

/// T-4423: a hidden slot named bare in an expression, as LinkML-Map evaluates it.
const READS_HIDDEN_BARE: &str = r#"    class_derivations:
      AirQualityObservedSdm:
        populated_from: AirQualityObserved
        slot_derivations:
          pm10: { populated_from: pm10 }
          note: { expr: "'call ' + operatorPhone" }"#;

const SECRET: &str = r#"    class_derivations:
      AirQualityObservedSdm:
        populated_from: AirQualityObserved
        slot_derivations:
          pm10: { populated_from: pm10, secretRef: { name: broker-password } }"#;

fn repo(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-mappings-{test}-{now}"));
    std::fs::create_dir_all(dir.join("mappings")).expect("create the repository");
    for (name, body) in MANIFESTS {
        write(&dir, name, body);
    }
    let native =
        "  native:\n    - { targetSlot: pm10, language: bloblang, source: \"root = this\" }\n";
    for (name, source, transformation, extra) in [
        ("air-to-sdm", "ovzdusie-air", READS_SHOWN, ""),
        ("air-uncompiled", "ovzdusie-air", READS_SHOWN, ""),
        ("air-phone", "ovzdusie-air", READS_HIDDEN, ""),
        ("air-phone-bare", "ovzdusie-air", READS_HIDDEN_BARE, ""),
        ("air-native", "ovzdusie-air", READS_SHOWN, native),
        ("air-secret", "ovzdusie-air", SECRET, ""),
        ("partner-to-sdm", "partner-air", READS_SHOWN, ""),
    ] {
        write(
            &dir,
            &format!("mapping-{name}.yaml"),
            &mapping(name, source, transformation, extra),
        );
        write(
            &dir,
            &format!("mappings/{name}.blobl"),
            "root.pm10 = this.pm10\n",
        );
        // The one Mapping nobody compiled for the gateway.
        if name != "air-uncompiled" {
            write(
                &dir,
                &format!("mappings/{name}.ir.json"),
                r#"{"classes":{"AirQualityObservedSdm":{"from":"AirQualityObserved"}}}"#,
            );
        }
    }
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a file");
}

/// The view's answers: the schema index and the JSON Schema narrowed to three slots.
async fn broker() -> String {
    let app = Router::new().fallback(|request: Request<Body>| async move {
        let path = request.uri().path().to_owned();
        let body = if path.ends_with("/linkml/schema/v1/model.schema.json") {
            json!({ "definitions": { "AirQualityObserved": {
                "type": "object",
                "properties": { "pm10": {}, "pm25": {}, "location": {} } } } })
        } else if path.ends_with("/linkml/schema/index.json") {
            json!({ "endpoint": SLUG, "models": [] })
        } else {
            return (StatusCode::NOT_FOUND, String::new());
        };
        (StatusCode::OK, body.to_string())
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the broker");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    url
}

async fn gateway(test: &str) -> Router {
    let (endpoints, spaces, .., mappings) = store::load(&repo(test)).expect("the repository loads");
    let gateway = Gateway::new(
        Broker::new(broker().await),
        Box::new(PolicyPdp),
        "banskabystrica.sk",
    )
    .serve(endpoints)
    .serve_spaces(spaces);
    gateway.replace_mappings(mappings);
    router(std::sync::Arc::new(gateway))
}

async fn get(app: &Router, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let request = Request::builder()
        .uri(path)
        .header("accept", "application/ld+json")
        .body(Body::empty())
        .expect("a request");
    let response = app.clone().oneshot(request).await.expect("an answer");
    let (status, headers) = (response.status(), response.headers().clone());
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    (status, headers, body.to_vec())
}

#[tokio::test]
async fn the_approved_readable_mapping_is_listed_and_served_and_no_other() {
    let app = gateway("served").await;

    let (status, _, body) = get(&app, &format!("{BASE}/schema/v1/mappings/")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let index: Value = serde_json::from_slice(&body).expect("a JSON index");
    let names: Vec<&str> = index["mappings"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|m| m["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, ["air-to-sdm"]);
    assert_eq!(index["version"], 1);
    // One was left out for this caller; the index says so and never which.
    assert_eq!(index["redacted"], true);
    assert!(!String::from_utf8_lossy(&body).contains("air-phone"));
    assert!(!String::from_utf8_lossy(&body).contains("operatorPhone"));
    let artifacts = index["mappings"][0]["artifacts"]
        .as_object()
        .expect("artifacts");
    assert_eq!(
        artifacts.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "air-to-sdm.blobl",
            "air-to-sdm.ir.json",
            "air-to-sdm.linkml-map.yaml"
        ]
    );

    for (file, media_type, expected) in [
        ("air-to-sdm.blobl", "text/plain", "root.pm10 = this.pm10"),
        (
            "air-to-sdm.ir.json",
            "application/json",
            "AirQualityObservedSdm",
        ),
        (
            "air-to-sdm.linkml-map.yaml",
            "text/yaml",
            "populated_from: AirQualityObserved",
        ),
    ] {
        let (status, headers, body) = get(&app, &format!("{BASE}/schema/v1/mappings/{file}")).await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::OK, "{file}: {text}");
        assert_eq!(headers["content-type"], media_type, "{file}");
        assert!(text.contains(expected), "{file}: {text}");
        assert!(!text.contains("secretRef"), "{file} carries a secretRef");
        let sha256 = artifacts[file]["sha256"].as_str().expect("a digest");
        assert_eq!(headers["etag"], format!("\"{sha256}\"").as_str(), "{file}");
    }

    // Uncompiled, reading a hidden slot, native, holding a secretRef, over another model, or
    // under another major: none is served.
    for path in [
        "v1/mappings/air-uncompiled.blobl",
        "v1/mappings/air-phone.blobl",
        "v1/mappings/air-phone.linkml-map.yaml",
        "v1/mappings/air-phone-bare.blobl",
        "v1/mappings/air-phone-bare.linkml-map.yaml",
        "v1/mappings/air-native.blobl",
        "v1/mappings/air-secret.linkml-map.yaml",
        "v1/mappings/partner-to-sdm.blobl",
        "v2/mappings/air-to-sdm.blobl",
        "vx/mappings/air-to-sdm.blobl",
    ] {
        let (status, _, _) = get(&app, &format!("{BASE}/schema/{path}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _, body) = get(&app, &format!("{BASE}/schema/v2/mappings/")).await;
    assert_eq!(status, StatusCode::OK);
    let empty: Value = serde_json::from_slice(&body).expect("a JSON index");
    assert_eq!(empty["mappings"], json!([]));
    assert!(
        empty.get("redacted").is_none(),
        "nothing of v2 was left out"
    );
}

#[tokio::test]
async fn the_schema_index_links_the_mappings_and_the_record_lists_their_files() {
    let app = gateway("record").await;

    let (status, headers, _) = get(&app, &format!("{BASE}/schema/index.json")).await;
    assert_eq!(status, StatusCode::OK);
    let links: Vec<&str> = headers
        .get_all("link")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    assert!(
        links.contains(
            &format!(
                "<{BASE}/schema/v1/mappings/>; rel=\"https://joinedcontext.com/rel/mappings\""
            )
            .as_str()
        ),
        "{links:?}"
    );

    let (status, _, body) = get(&app, BASE).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let record: Value = serde_json::from_slice(&body).expect("a JSON-LD record");
    let urls: Vec<&str> = record["dcat:distribution"]
        .as_array()
        .expect("distributions")
        .iter()
        .filter_map(|d| d["dcat:accessURL"].as_str())
        .filter(|url| url.contains("/mappings/"))
        .collect();
    assert_eq!(urls.len(), 3, "{urls:?}");
    assert!(
        urls.iter()
            .all(|url| url.contains("/schema/v1/mappings/air-to-sdm.")),
        "{urls:?}"
    );
    let ir = record["dcat:distribution"]
        .as_array()
        .expect("distributions")
        .iter()
        .find(|d| {
            d["dcat:accessURL"]
                .as_str()
                .is_some_and(|u| u.ends_with("air-to-sdm.ir.json"))
        })
        .expect("the IR distribution");
    assert!(
        ir["spdx:checksum"]["spdx:checksumValue"].is_string(),
        "{ir}"
    );
    assert_eq!(
        ir["dct:conformsTo"],
        "https://joinedcontext.com/ns/mapping-ir"
    );
}

#[test]
fn a_mapping_reads_its_sources_in_every_form_linkml_map_writes() {
    let read = |spec: Value| reads(&spec).expect("readable");
    // A map by name: populated_from, an expression's names, a slot of its own name, a dotted path.
    let spec = json!({ "class_derivations": { "Out": { "populated_from": "In", "slot_derivations": {
        "a": { "populated_from": "x.y" }, "b": { "expr": "{p} + ' (' + {q} + ')'" }, "c": {} } } } });
    let got = read(spec);
    assert_eq!(
        got["In"].iter().map(String::as_str).collect::<Vec<_>>(),
        ["c", "p", "q", "x"]
    );
    // A list of derivations, and a class read under its own name.
    let spec =
        json!({ "class_derivations": [{ "name": "Same", "slot_derivations": [{ "name": "s" }] }] });
    assert_eq!(read(spec)["Same"].iter().collect::<Vec<_>>(), ["s"]);
    // Nothing derived reads nothing; a derivation that is neither a map nor a list is refused.
    assert!(read(json!({ "id": "x" })).is_empty());
    assert!(reads(&json!({ "class_derivations": "Out" })).is_none());
    assert!(reads(&json!({ "class_derivations": { "Out": { "populated_from": 7 } } })).is_none());
}

/// T-4423: what an `expr` reads, braced or bare, and an expression that cannot be read.
#[test]
fn an_expression_reads_every_slot_it_names_braced_or_bare() {
    let slots = |expr: &str| {
        reads(
            &json!({ "class_derivations": { "Out": { "populated_from": "In",
            "slot_derivations": { "o": { "expr": expr } } } } }),
        )
        .map(|got| got["In"].iter().cloned().collect::<Vec<_>>())
    };
    let some = |names: &[&str]| Some(names.iter().map(|n| n.to_string()).collect::<Vec<_>>());
    assert_eq!(slots("{a} + ' ' + {b}"), some(&["a", "b"]));
    assert_eq!(slots("round({a} * 1.5)"), some(&["a"]));
    assert_eq!(slots("hiddenSlot * 2"), some(&["hiddenSlot"]));
    // Strings, numbers, keywords, called functions and attributes after a dot name no slot.
    assert_eq!(
        slots("str(level) + ' µg' if {flag} and not 2e3 > 0.5 else \"a {x} 'y'\""),
        some(&["flag", "level"])
    );
    assert_eq!(
        slots("{name}.upper() + street.title()"),
        some(&["name", "street"])
    );
    // A function's name used bare is a slot of that name.
    assert_eq!(slots("max + 1"), some(&["max"]));
    // Unreadable: the Mapping is not published at all.
    for bad in ["'open", "{a", "{}", "{a b}", "a | b", "`a`", "$a", "a; b"] {
        assert_eq!(slots(bad), None, "{bad}");
    }
}
