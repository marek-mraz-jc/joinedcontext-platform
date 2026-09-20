//! The schema surface: what the data contains, narrowed to what the caller may read
//! (T-0162, EP-46…EP-52, DM-02, DM-22).
//!
//! Two halves are exercised, because a deployment will have both: a model whose artifacts
//! Model Tools committed beside the LinkML source, and one whose artifacts are not in the
//! checkout yet and is therefore derived from the grants.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// What Model Tools generates for the demo model: three readable slots, one the public
/// grant never names, and a class the public grant never names either.
fn compiled_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$defs": {
            "AirQualityObserved": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "type": { "const": "AirQualityObserved" },
                    "pm10": { "type": "number" },
                    "pm25": { "type": "number" },
                    "location": { "$ref": "#/$defs/GeoProperty" },
                    "internalNote": { "type": "string" },
                },
                "patternProperties": { "^x-": { "type": "string" } },
                "required": ["id", "type", "pm10", "internalNote"],
            },
            "InternalIncident": {
                "type": "object",
                "properties": { "severity": { "type": "string" } },
            },
            "GeoProperty": { "type": "object" },
        },
    })
}

fn compiled_context() -> Value {
    json!({
        "@context": {
            "@vocab": "https://bb.example.sk/schema/air-quality/",
            "AirQualityObserved": "https://bb.example.sk/schema/air-quality/AirQualityObserved",
            "InternalIncident": "https://bb.example.sk/schema/air-quality/InternalIncident",
            "pm10": "https://bb.example.sk/schema/air-quality/pm10",
            "internalNote": "https://bb.example.sk/schema/air-quality/internalNote",
        }
    })
}

fn air_quality(with_artifacts: bool) -> Model {
    Model {
        name: "bb-air-quality".to_owned(),
        version: "1.4.0".to_owned(),
        major: 1,
        classes: vec![
            "AirQualityObserved".to_owned(),
            "InternalIncident".to_owned(),
        ],
        json_schema: with_artifacts.then(compiled_schema),
        context: with_artifacts.then(compiled_context),
    }
}

/// The public grant of DEMO step 4: three attributes of one type, and nothing else.
fn endpoint(models: Vec<Model>) -> Endpoint {
    Endpoint {
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models,
        policies: vec![policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25]
    relationshipNames: [location]
"#,
        )],
    }
}

/// The same endpoint under a policy set that grants both classes and names no attribute, so
/// every slot of both survives the projection and nothing is left out.
fn keeping_nothing_back(models: Vec<Model>) -> Endpoint {
    Endpoint {
        policies: vec![policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
      - type: InternalIncident
"#,
        )],
        ..endpoint(models)
    }
}

fn serving(endpoint: Endpoint) -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint]),
    ))
}

fn app(models: Vec<Model>) -> axum::Router {
    serving(endpoint(models))
}

async fn call(
    models: Vec<Model>,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, Value) {
    let response = app(models)
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

/// The same call, for the formalisms whose body is text rather than JSON (T-0284).
async fn text(
    models: Vec<Model>,
    request: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, String) {
    let response = app(models)
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        headers,
        String::from_utf8(body.to_vec()).expect("a text document"),
    )
}

/// One request against an endpoint of the caller's choosing, with the whole answer kept as
/// text: a leak is looked for in the bytes that were sent, not in the fields a test remembers
/// to read.
async fn served(router: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = router.oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (
        status,
        String::from_utf8(body.to_vec()).expect("a text document"),
    )
}

/// What `describe_schema` answers an agent for the summary format: the same index the REST
/// route serves, inside the MCP result envelope (EP-52).
fn summary_call() -> Request<Body> {
    let payload = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "describe_schema", "arguments": { "format": "summary" } }
    });
    Request::builder()
        .method("POST")
        .uri(format!("/api/endpoint/{SLUG}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(payload.to_string()))
        .expect("a request")
}

fn parsed(raw: &str) -> Value {
    serde_json::from_str(raw.trim().trim_start_matches("data: ").trim())
        .unwrap_or_else(|error| panic!("the gateway answered {raw}: {error}"))
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a request")
}

fn accepting(path: &str, accept: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header("accept", accept)
        .body(Body::empty())
        .expect("a request")
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header == name)
        .map(|(_, value)| value.as_str())
}

/// EP-46: the index names the model, the types the caller may see, and the artifacts the
/// gateway actually serves, each with the digest of the document it will hand back.
#[tokio::test]
async fn the_index_describes_what_this_endpoint_publishes() {
    let (status, _, document) = call(
        vec![air_quality(true)],
        get(&format!("/api/endpoint/{SLUG}/schema/index.json")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(document["endpoint"], json!(SLUG));
    let models = document["models"].as_array().expect("models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["name"], json!("bb-air-quality"));
    assert_eq!(
        models[0]["version"],
        json!(1),
        "the major is what is served"
    );
    assert_eq!(models[0]["semver"], json!("1.4.0"));
    assert_eq!(models[0]["types"], json!(["AirQualityObserved"]));

    let descriptor = &models[0]["artifacts"]["model.schema.json"];
    assert_eq!(descriptor["type"], json!("application/schema+json"));
    assert!(descriptor["bytes"].as_u64().is_some_and(|bytes| bytes > 0));
    let digest = descriptor["sha256"].as_str().expect("a digest");
    assert_eq!(digest.len(), 64, "sha256 as lowercase hex");

    // The digest in the index is the digest of the document the surface serves, or a
    // client that trusts the index revalidates against nothing.
    let (_, headers, _) = call(
        vec![air_quality(true)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/json-schema")),
    )
    .await;
    assert_eq!(
        header(&headers, "etag"),
        Some(format!("\"{digest}\"").as_str())
    );

    // All seven documents of API/02 section 7a are listed, and each digest is the digest of
    // the projection a caller would fetch rather than of a file on disk (EP-46, EP-48).
    let artifacts = models[0]["artifacts"].as_object().expect("artifacts");
    let mut names: Vec<&str> = artifacts.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "context.jsonld",
            "model.linkml.yaml",
            "model.md",
            "model.owl.ttl",
            "model.rdf.ttl",
            "model.schema.json",
            "model.shacl.ttl",
        ]
    );
    for (name, media_type) in [
        ("model.shacl.ttl", "text/turtle"),
        ("model.owl.ttl", "text/turtle; profile=\"owl\""),
        ("model.rdf.ttl", "text/turtle"),
        ("model.linkml.yaml", "text/yaml"),
        ("model.md", "text/markdown"),
    ] {
        assert_eq!(artifacts[name]["type"], json!(media_type), "{name}");
        let listed = artifacts[name]["sha256"].as_str().expect("a digest");
        let (_, headers, body) = text(
            vec![air_quality(true)],
            get(&format!("/api/endpoint/{SLUG}/schema/v1/{name}")),
        )
        .await;
        assert_eq!(
            header(&headers, "etag"),
            Some(format!("\"{listed}\"").as_str()),
            "{name}: the index and the document disagree about the bytes"
        );
        assert_eq!(
            artifacts[name]["bytes"].as_u64(),
            Some(body.len() as u64),
            "{name}"
        );
    }
}

/// EP-47 and the task's security property: a slot the endpoint's policy set forbids is
/// absent from the schema, from `required`, and from the index's type list.
#[tokio::test]
async fn a_forbidden_class_and_slot_are_projected_out() {
    let (status, headers, schema) = call(
        vec![air_quality(true)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/json-schema")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/schema+json")
    );

    let air = &schema["$defs"]["AirQualityObserved"];
    for granted in ["id", "type", "pm10", "pm25", "location"] {
        assert!(
            !air["properties"][granted].is_null(),
            "{granted} is granted"
        );
    }
    assert!(
        air["properties"]["internalNote"].is_null(),
        "a slot no grant names is not described"
    );
    assert_eq!(
        air["required"],
        json!(["id", "type", "pm10"]),
        "a removed slot cannot stay required"
    );
    assert!(
        air["patternProperties"].is_null(),
        "a pattern could match a slot that was just removed"
    );

    assert!(
        schema["$defs"]["InternalIncident"].is_null(),
        "a class no grant names is not described"
    );
    // The shared definition a surviving class still `$ref`s stays, or the schema breaks.
    assert!(!schema["$defs"]["GeoProperty"].is_null());
    assert!(!schema.to_string().contains("severity"));
}

/// EP-47: the same projection on the `@context`, because a term is as much a disclosure
/// as a property.
#[tokio::test]
async fn the_context_carries_only_the_granted_terms() {
    let (status, headers, document) = call(
        vec![air_quality(true)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/context.jsonld")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/ld+json")
    );

    let parts = document["@context"].as_array().expect("a context array");
    assert_eq!(
        parts[0],
        json!("https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.8.jsonld")
    );
    let terms = &parts[1];
    assert!(!terms["AirQualityObserved"].is_null());
    assert!(!terms["pm10"].is_null());
    assert!(
        !terms["@vocab"].is_null(),
        "keywords survive the projection"
    );
    assert!(terms["internalNote"].is_null());
    assert!(terms["InternalIncident"].is_null());
}

/// EP-49: `schema/v{major}/model` is negotiated, and the formalisms only Model Tools can
/// produce answer 406 rather than something else.
#[tokio::test]
async fn content_negotiation_serves_what_the_gateway_has_and_refuses_the_rest() {
    let path = format!("/api/endpoint/{SLUG}/schema/v1/model");

    let (status, headers, schema) = call(
        vec![air_quality(true)],
        accepting(&path, "application/schema+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/schema+json")
    );
    assert!(!schema["$defs"]["AirQualityObserved"].is_null());

    let (status, headers, document) = call(
        vec![air_quality(true)],
        accepting(&path, "application/ld+json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/ld+json")
    );
    assert!(document["@context"].is_array());

    // Turtle picks the SHACL, the `owl` profile the ontology, YAML the LinkML source: each
    // formalism the gateway renders from the projection has its own media type (EP-49).
    for (accept, media_type) in [
        ("text/turtle", "text/turtle"),
        (
            "text/turtle; profile=\"owl\"",
            "text/turtle; profile=\"owl\"",
        ),
        ("text/yaml", "text/yaml"),
        ("text/markdown", "text/markdown"),
    ] {
        let (status, headers, body) = text(vec![air_quality(true)], accepting(&path, accept)).await;
        assert_eq!(status, StatusCode::OK, "{accept}");
        assert_eq!(
            header(&headers, "content-type"),
            Some(media_type),
            "{accept}"
        );
        assert!(!body.trim().is_empty(), "{accept} answered an empty body");
    }

    // And the same by name, which is how the index links them.
    for (name, media_type) in [
        ("model.shacl.ttl", "text/turtle"),
        ("model.owl.ttl", "text/turtle; profile=\"owl\""),
        ("model.rdf.ttl", "text/turtle"),
        ("model.linkml.yaml", "text/yaml"),
        ("model.md", "text/markdown"),
    ] {
        let (status, headers, _) = text(
            vec![air_quality(true)],
            get(&format!("/api/endpoint/{SLUG}/schema/v1/{name}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert_eq!(header(&headers, "content-type"), Some(media_type), "{name}");
    }

    // A name no formalism answers to is still a miss, not an empty document.
    let (status, _, _) = call(
        vec![air_quality(true)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/model.avro.json")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// EP-47: what the JSON Schema hides, every other formalism hides too. This is the whole
/// reason the gateway renders them instead of serving the committed bytes (T-0284).
#[tokio::test]
async fn a_forbidden_slot_is_absent_from_every_formalism() {
    for name in [
        "model.shacl.ttl",
        "model.owl.ttl",
        "model.rdf.ttl",
        "model.linkml.yaml",
        "model.md",
    ] {
        let (status, _, body) = text(
            vec![air_quality(true)],
            get(&format!("/api/endpoint/{SLUG}/schema/v1/{name}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}");

        // The grant names pm10, pm25 and location of one type, and nothing else.
        assert!(body.contains("pm10"), "{name} lost a granted slot");
        assert!(
            !body.contains("internalNote"),
            "{name} carries a slot the grant forbids:\n{body}"
        );
        assert!(
            !body.contains("InternalIncident"),
            "{name} carries a class the grant forbids:\n{body}"
        );
    }
}

/// The rendered Turtle says what a validator needs and nothing the projection cannot back.
#[tokio::test]
async fn the_shacl_shape_targets_the_class_and_is_never_closed() {
    let (status, _, shapes) = text(
        vec![air_quality(true)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/model.shacl.ttl")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert!(shapes.contains("@prefix sh: <http://www.w3.org/ns/shacl#> ."));
    assert!(shapes.contains("a sh:NodeShape"));
    assert!(shapes
        .contains("sh:targetClass <urn:joinedcontext:model:bb-air-quality:v1:AirQualityObserved>"));
    assert!(
        shapes.contains("sh:path <urn:joinedcontext:model:bb-air-quality:v1:pm10>"),
        "{shapes}"
    );
    assert!(
        shapes.contains("sh:datatype xsd:double"),
        "a numeric slot carries its datatype:\n{shapes}"
    );
    assert!(
        shapes.contains("sh:closed false"),
        "a projected shape is never closed: the entity carries slots this caller may not see"
    );
    // `pm10` is required in the compiled schema and survives the projection; `internalNote`
    // was required too and was removed, so no shape may still demand it.
    assert!(shapes.contains("sh:minCount 1"), "{shapes}");
}

/// The OWL and the RDFS renderings name the same terms as the SHACL, so a consumer that
/// reads two of them sees one vocabulary (T-0284).
#[tokio::test]
async fn the_rdf_family_shares_one_vocabulary() {
    let mut documents = Vec::new();
    for name in ["model.shacl.ttl", "model.owl.ttl", "model.rdf.ttl"] {
        let (_, _, body) = text(
            vec![air_quality(true)],
            get(&format!("/api/endpoint/{SLUG}/schema/v1/{name}")),
        )
        .await;
        documents.push(body);
    }
    for document in &documents {
        assert!(
            document.contains("urn:joinedcontext:model:bb-air-quality:v1:pm10"),
            "{document}"
        );
    }
    assert!(documents[1].contains("a owl:Class"), "{}", documents[1]);
    assert!(
        documents[1].contains("owl:DatatypeProperty"),
        "{}",
        documents[1]
    );
    assert!(documents[2].contains("a rdfs:Class"), "{}", documents[2]);
    assert!(documents[2].contains("a rdf:Property"), "{}", documents[2]);
}

/// The LinkML source and the Markdown describe the projection in their own shapes.
#[tokio::test]
async fn the_linkml_and_the_documentation_describe_the_projection() {
    let (_, _, source) = text(
        vec![air_quality(true)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/model.linkml.yaml")),
    )
    .await;
    let parsed: Value = serde_norway::from_str(&source).expect("the rendered LinkML is YAML");
    assert_eq!(parsed["id"], "urn:joinedcontext:model:bb-air-quality:v1");
    assert_eq!(parsed["name"], "bb-air-quality");
    let attributes = &parsed["classes"]["AirQualityObserved"]["attributes"];
    assert_eq!(attributes["pm10"]["range"], "float");
    assert!(
        attributes.get("internalNote").is_none(),
        "the LinkML carries only the granted slots"
    );
    assert!(parsed["classes"].get("InternalIncident").is_none());

    let (_, _, docs) = text(
        vec![air_quality(true)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/model.md")),
    )
    .await;
    assert!(docs.starts_with("# bb-air-quality 1.4.0"), "{docs}");
    assert!(docs.contains("## AirQualityObserved"), "{docs}");
    assert!(docs.contains("| pm10 | float | yes |"), "{docs}");
}

/// A model with no committed artifacts renders every formalism from the grant, so the
/// surface answers the same seven names either way (EP-46).
#[tokio::test]
async fn a_model_without_artifacts_still_answers_every_formalism() {
    for name in [
        "model.shacl.ttl",
        "model.owl.ttl",
        "model.rdf.ttl",
        "model.linkml.yaml",
        "model.md",
    ] {
        let (status, _, body) = text(
            vec![air_quality(false)],
            get(&format!("/api/endpoint/{SLUG}/schema/v1/{name}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}");
        assert!(body.contains("pm10"), "{name}: {body}");
        assert!(!body.contains("internalNote"), "{name}: {body}");
    }
}

/// EP-51: the document is a projection of the policy set, so it revalidates rather than
/// being cached as immutable — and a client holding the current bytes gets 304.
#[tokio::test]
async fn a_strong_etag_revalidates_instead_of_going_stale() {
    let path = format!("/api/endpoint/{SLUG}/schema/v1/json-schema");
    let (status, headers, _) = call(vec![air_quality(true)], get(&path)).await;
    assert_eq!(status, StatusCode::OK);

    let etag = header(&headers, "etag").expect("a strong ETag").to_owned();
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "{etag} is strong"
    );
    assert_eq!(
        header(&headers, "cache-control"),
        Some("private, no-cache"),
        "a grant that changes changes the schema, and the schema is one caller's (T-2261)"
    );

    let conditional = Request::builder()
        .uri(&path)
        .header("if-none-match", &etag)
        .body(Body::empty())
        .expect("a request");
    let (status, headers, _) = call(vec![air_quality(true)], conditional).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(header(&headers, "etag"), Some(etag.as_str()));

    let stale = Request::builder()
        .uri(&path)
        .header(
            "if-none-match",
            "\"0000000000000000000000000000000000000000000000000000000000000000\"",
        )
        .body(Body::empty())
        .expect("a request");
    let (status, _, _) = call(vec![air_quality(true)], stale).await;
    assert_eq!(status, StatusCode::OK, "a stale copy is replaced");
}

/// DM-02: a model whose artifacts are not committed still describes itself, from the
/// grants alone — and still says nothing the grants do not cover.
#[tokio::test]
async fn a_model_without_artifacts_is_derived_from_the_grants() {
    let (status, _, schema) = call(
        vec![air_quality(false)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/json-schema")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let air = &schema["$defs"]["AirQualityObserved"];
    assert_eq!(air["type"], json!("object"));
    assert_eq!(
        air["properties"]["type"]["const"],
        json!("AirQualityObserved")
    );
    for granted in ["pm10", "pm25", "location"] {
        assert!(!air["properties"][granted].is_null(), "{granted}");
    }
    assert!(air["properties"]["internalNote"].is_null());
    assert!(
        schema["$defs"]["InternalIncident"].is_null(),
        "a class outside the grant is not derived either"
    );

    let (status, _, document) = call(
        vec![air_quality(false)],
        get(&format!("/api/endpoint/{SLUG}/schema/v1/context.jsonld")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let terms = &document["@context"][1];
    assert!(!terms["pm10"].is_null());
    assert!(terms["internalNote"].is_null());
}

/// EP-47, T-2133: the index says *that* something was left out and never what.
///
/// A list of the hidden slots publishes the three things the projection exists to keep back: that
/// the attribute exists, which type carries it, and that somebody thought it worth hiding. So the
/// answer is one boolean — asked anonymously over REST, and over the MCP summary an agent reads.
#[tokio::test]
async fn an_endpoint_never_names_what_it_hides() {
    for (surface, router, request) in [
        (
            "REST",
            app(vec![air_quality(true)]),
            get(&format!("/api/endpoint/{SLUG}/schema/index.json")),
        ),
        ("MCP", app(vec![air_quality(true)]), summary_call()),
    ] {
        let (status, raw) = served(router, request).await;
        assert_eq!(status, StatusCode::OK, "{surface}");

        // `internalNote` is a slot of a granted class, `InternalIncident` a class of its own:
        // neither name may be anywhere in the bytes, envelope and text content included.
        for hidden in ["internalNote", "InternalIncident", "severity"] {
            assert!(
                !raw.contains(hidden),
                "{surface} names what it hides ({hidden}):\n{raw}"
            );
        }

        let document = parsed(&raw);
        let model = match surface {
            "REST" => &document["models"][0],
            _ => &document["result"]["structuredContent"]["schema"]["models"][0],
        };
        assert_eq!(
            model["redacted"],
            json!(true),
            "{surface} hid something and did not say so:\n{raw}"
        );
        assert_eq!(
            model["types"],
            json!(["AirQualityObserved"]),
            "{surface} lists the types the caller may read"
        );
    }
}

/// EP-47, T-2133: the other half of the boolean. A caller who may read every class and every
/// slot is told nothing was left out — the key is absent, not `false`, because a document that
/// carries it on every answer says "there is more here" about an endpoint that has no more.
#[tokio::test]
async fn a_caller_who_sees_everything_is_told_nothing_was_left_out() {
    for (surface, request) in [
        (
            "REST",
            get(&format!("/api/endpoint/{SLUG}/schema/index.json")),
        ),
        ("MCP", summary_call()),
    ] {
        let (status, raw) = served(
            serving(keeping_nothing_back(vec![air_quality(true)])),
            request,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{surface}");

        let document = parsed(&raw);
        let model = match surface {
            "REST" => &document["models"][0],
            _ => &document["result"]["structuredContent"]["schema"]["models"][0],
        };
        assert_eq!(
            model["types"],
            json!(["AirQualityObserved", "InternalIncident"]),
            "{surface}: this grant reaches both classes:\n{raw}"
        );
        assert!(
            model.get("redacted").is_none(),
            "{surface} claims something was kept back from a caller it kept nothing from:\n{raw}"
        );
        // Not under another name either: an empty list of hidden slots is still a field that
        // teaches a client to look for hidden slots.
        assert!(
            !raw.contains("redact"),
            "{surface} mentions redaction to a caller nothing was redacted from:\n{raw}"
        );
    }
}

/// DM-22: `v{major}` names a major the space actually publishes; anything else is not a
/// document, and an unknown slug says nothing about whether it exists (EP-03).
#[tokio::test]
async fn an_unknown_major_slug_or_document_is_a_miss() {
    for path in [
        format!("/api/endpoint/{SLUG}/schema/v9/json-schema"),
        format!("/api/endpoint/{SLUG}/schema/2/json-schema"),
        format!("/api/endpoint/{SLUG}/schema/v1/model.parquet"),
        "/api/endpoint/nosuchendpoint/schema/index.json".to_owned(),
        "/api/endpoint/nosuchendpoint/schema/v1/json-schema".to_owned(),
    ] {
        let (status, _, _) = call(vec![air_quality(true)], get(&path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}
