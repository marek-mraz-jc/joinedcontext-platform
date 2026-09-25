//! The MCP façade and the REST schema route describe the same model (MP-02, EP-47, SP-16).
//!
//! Two doors onto one artifact. The façade holds no rendering path of its own, so whatever the
//! REST route serves for a caller is byte for byte what `describe_schema` answers — and neither
//! may name a slot the projection gives to another type, which would tell an agent an attribute
//! exists on a type whose data door hides it.

use axum::body::Body;
use axum::extract::Request;
use axum::http::Method;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const DOMAIN: &str = "hel.fi";

/// A projection that gives each type its own slots: `age` belongs to `User`, `weight` to
/// `Vehicle`, and neither is a slot of the other.
const VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata: { name: view, namespace: helsinki }
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: User
      slots: [name, age, ssn]
    - name: Vehicle
      slots: [name, weight]
"#;

const ATTRS: &[&str] = &["name", "age", "weight", "ssn"];

/// `ssn` is a slot of the projection and an attribute the endpoint hides: the projection
/// alone would not prove that `hidden_attributes` reaches every formalism (EP-61).
const HIDDEN: &str = "ssn";

/// The endpoint whose model is larger than one answer carries.
const BIG_SLUG: &str = "7wgzlmhjq4hlpjlrj4dcdp6dsu";

/// The formalisms `describe_schema` renders, LinkML first, as the summary lists them.
const FORMALISMS: [&str; 7] = [
    "linkml",
    "json-schema",
    "context",
    "shacl",
    "owl",
    "rdf",
    "markdown",
];

async fn broker() -> String {
    let app = Router::new().fallback(any(|| async { axum::Json(json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("the stub serves") });
    format!("http://{address}")
}

fn endpoint() -> Endpoint {
    let projection = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(VIEW).expect("it parses");
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: fleet\nassigner: did:web:hel.fi\nassignee: { kind: role, id: public }\n\
         operations: [queryEntity, retrieveEntity, retrieveEntityTypes]\n",
    )
    .expect("the policy parses");
    let properties = |kind: &str| {
        let mut members = json!({ "id": { "type": "string" }, "type": { "const": kind } });
        for attr in ATTRS {
            members[attr] =
                json!({ "type": "string", "description": format!("DESC-{kind}-{attr}") });
        }
        json!({ "type": "object", "properties": members })
    };
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "fleet".to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: [HIDDEN.to_owned()].into_iter().collect(),
        projection: Some(Arc::new(projection.spec)),
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["User".to_owned(), "Vehicle".to_owned()],
            json_schema: Some(json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$defs": { "User": properties("User"), "Vehicle": properties("Vehicle") }
            })),
            context: Some(json!({ "@context": { "@vocab": "https://hel.fi/schema/" } })),
        }],
        policies: vec![policy],
    }
}

/// An endpoint whose one model is far larger than [`DOCUMENT_BOUND`]: eighty classes of
/// forty described slots each, which renders to well over a mebibyte in every formalism.
fn big_endpoint() -> Endpoint {
    let filler = "l".repeat(512);
    let mut defs = serde_json::Map::new();
    let mut classes = Vec::new();
    for class in 0..80 {
        let name = format!("Class{class}");
        let mut members = json!({ "id": { "type": "string" }, "type": { "const": name } });
        for slot in 0..40 {
            members[format!("slot{slot}")] =
                json!({ "type": "string", "description": format!("{name} slot{slot} {filler}") });
        }
        defs.insert(
            name.clone(),
            json!({ "type": "object", "properties": members }),
        );
        classes.push(name);
    }
    let policy: PolicySpec = serde_norway::from_str(
        "contextSpaceRef: wide\nassigner: did:web:hel.fi\nassignee: { kind: role, id: public }\n\
         operations: [queryEntity, retrieveEntity, retrieveEntityTypes]\n",
    )
    .expect("the policy parses");
    Endpoint {
        roles: Default::default(),
        slug: BIG_SLUG.to_owned(),
        space: "wide".to_owned(),
        base_path: format!("/api/endpoint/{BIG_SLUG}"),
        projection: None,
        models: vec![Model {
            name: "wide".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes,
            json_schema: Some(json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "$defs": Value::Object(defs),
            })),
            context: Some(json!({ "@context": { "@vocab": "https://hel.fi/schema/" } })),
        }],
        policies: vec![policy],
        ..endpoint()
    }
}

async fn gateway() -> Router {
    let upstream = broker().await;
    router(Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .serve([endpoint(), big_endpoint()]),
    ))
}

async fn body_of(request: Request<Body>) -> String {
    let response = gateway()
        .await
        .oneshot(request)
        .await
        .expect("an answer to the request");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The REST artifact for one formalism.
async fn rest(path: &str) -> String {
    body_of(
        Request::builder()
            .uri(format!("/api/endpoint/{SLUG}{path}"))
            .body(Body::empty())
            .expect("a request"),
    )
    .await
}

/// One `tools/call` of the façade, as the JSON-RPC `result` object.
async fn called(slug: &str, name: &str, arguments: Value) -> Value {
    let payload = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let raw = body_of(
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/endpoint/{slug}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(payload.to_string()))
            .expect("a request"),
    )
    .await;
    let answer: Value = serde_json::from_str(raw.trim_start_matches("data: ").trim())
        .unwrap_or_else(|error| panic!("the façade answered {raw}: {error}"));
    answer["result"].clone()
}

/// `describe_schema` on one endpoint, as the JSON-RPC `result` object.
async fn describe(slug: &str, format: &str, entity_type: Option<Value>) -> Value {
    let mut arguments = json!({ "format": format });
    if let Some(wanted) = entity_type {
        arguments["entityType"] = wanted;
    }
    called(slug, "describe_schema", arguments).await
}

/// The structured half of a `describe_schema` answer: the document for a JSON formalism, the
/// `{format, mediaType, document}` envelope for a text one.
async fn structured(format: &str, entity_type: Option<Value>) -> Value {
    describe(SLUG, format, entity_type).await["structuredContent"]["schema"].clone()
}

/// The document `describe_schema` answers for one format, unwrapped from the MCP envelope.
async fn mcp(format: &str, entity_type: Option<&str>) -> String {
    let result = describe(SLUG, format, entity_type.map(|wanted| json!(wanted))).await;
    let document = &result["structuredContent"]["schema"];
    match document.get("document").and_then(Value::as_str) {
        Some(text) => text.to_owned(),
        // A JSON formalism is the structured result itself; a refusal carries none, and its
        // words are in the result's own content.
        None if document.is_object() => {
            serde_json::to_string(document).expect("the result serializes")
        }
        None => serde_json::to_string(&result).expect("the result serializes"),
    }
}

/// The bytes a caller receives for one artifact, which is what its digest is taken over.
fn served_bytes(artifact: &Value) -> Vec<u8> {
    match artifact.get("document").and_then(Value::as_str) {
        Some(text) => text.as_bytes().to_vec(),
        None => serde_json::to_vec(artifact).expect("the document serializes"),
    }
}

fn sha256_of(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// EP-47, MP-02: the slots of one type are not described under another, in any formalism the
/// endpoint renders.
#[tokio::test]
async fn a_slot_of_another_type_is_in_no_mcp_formalism() {
    for format in ["linkml", "shacl", "owl", "rdf", "markdown", "json-schema"] {
        let document = mcp(format, None).await;
        assert!(
            !document.contains("DESC-Vehicle-age"),
            "`age` is a slot of User alone, and {format} describes it under Vehicle:\n{document}"
        );
        assert!(
            !document.contains("DESC-User-weight"),
            "`weight` is a slot of Vehicle alone, and {format} describes it under User:\n{document}"
        );
    }
}

/// SP-16: the façade renders nothing of its own, so the two doors answer the same bytes.
#[tokio::test]
async fn the_mcp_and_the_rest_artifact_are_byte_equal() {
    for (format, path) in [
        ("linkml", "/schema/v1/model.linkml.yaml"),
        ("shacl", "/schema/v1/model.shacl.ttl"),
        ("owl", "/schema/v1/model.owl.ttl"),
        ("rdf", "/schema/v1/model.rdf.ttl"),
        ("markdown", "/schema/v1/model.md"),
        ("json-schema", "/schema/v1/model.schema.json"),
    ] {
        assert_eq!(
            mcp(format, None).await,
            rest(path).await,
            "the two doors disagree about {format}"
        );
    }
}

/// The same holds once the caller narrows the description to one type.
#[tokio::test]
async fn a_narrowed_description_holds_only_that_types_slots() {
    let document = mcp("linkml", Some("Vehicle")).await;

    assert!(
        document.contains("DESC-Vehicle-weight"),
        "Vehicle's own slot is described:\n{document}"
    );
    assert!(
        !document.contains("DESC-Vehicle-age") && !document.contains("DESC-User-age"),
        "`age` is not Vehicle's:\n{document}"
    );
}

/// An unknown type is refused by name rather than answered with an empty model, so a traversal
/// probe cannot read an empty document as "the argument went through" (AG-21).
#[tokio::test]
async fn a_type_the_endpoint_does_not_describe_is_refused_by_name() {
    let raw = mcp("linkml", Some("Depot")).await;

    assert!(
        raw.contains("not an entity type this endpoint describes"),
        "answered {raw}"
    );
}

/// EP-25, EP-26, AG-29: the catalogue an agent reads first names the formalism to load and
/// describes every artifact it could load instead, so the choice is not a guess.
#[tokio::test]
async fn the_summary_recommends_linkml_and_lists_every_artifact_with_its_digest() {
    let summary = structured("summary", None).await;

    assert_eq!(summary["recommended"], json!("linkml"));
    assert!(
        summary["recommendedBecause"]
            .as_str()
            .is_some_and(|why| why.contains("LinkML")),
        "the recommendation says why: {summary}"
    );

    let artifacts = summary["artifacts"]
        .as_array()
        .unwrap_or_else(|| panic!("the summary lists its artifacts: {summary}"));
    assert_eq!(
        artifacts[0]["format"],
        json!("linkml"),
        "LinkML is listed first: {summary}"
    );
    let listed: Vec<&str> = artifacts
        .iter()
        .filter_map(|artifact| artifact["format"].as_str())
        .collect();
    for format in FORMALISMS {
        assert!(listed.contains(&format), "{format} is listed: {summary}");
    }
    for artifact in artifacts {
        for member in ["format", "mediaType", "bytes", "sha256", "uri"] {
            assert!(
                artifact.get(member).is_some_and(|value| !value.is_null()),
                "every artifact carries `{member}`: {artifact}"
            );
        }
        assert!(
            artifact["bytes"].as_u64().is_some_and(|bytes| bytes > 0),
            "an artifact that renders to nothing is not published: {artifact}"
        );
        assert_eq!(
            artifact["uri"],
            json!(format!(
                "schema://{SLUG}/v1/{}",
                artifact["format"].as_str().unwrap_or_default()
            )),
            "the resource URI is the one `resources/read` answers: {artifact}"
        );
    }
    // A caller reading the catalogue learns nothing about a slot the grant hides.
    assert!(
        !summary.to_string().contains(HIDDEN),
        "the catalogue names no hidden attribute: {summary}"
    );
}

/// EP-47, EP-61: an attribute the endpoint hides is in none of the seven documents, not only
/// in the one a test happened to render.
#[tokio::test]
async fn a_hidden_attribute_is_in_no_formalism() {
    for format in FORMALISMS {
        let document = mcp(format, None).await;
        assert!(
            !document.contains(HIDDEN),
            "`{HIDDEN}` is hidden by the endpoint and {format} describes it:\n{document}"
        );
        assert!(
            !document.contains(&format!("DESC-User-{HIDDEN}")),
            "its description is hidden with it, in {format}:\n{document}"
        );
    }
}

/// EP-47, SP-20: a type the grant does not cover is refused in the very words an unknown type
/// is refused in, so the argument is no way to ask which types exist.
#[tokio::test]
async fn a_type_outside_the_grant_is_refused_like_an_unknown_type() {
    let ungranted = mcp("linkml", Some("Depot")).await;
    let unknown = mcp("linkml", Some("Nonexistent")).await;

    for answer in [&ungranted, &unknown] {
        assert!(
            answer.contains("not an entity type this endpoint describes"),
            "answered {answer}"
        );
    }
    // Only the name the caller sent differs between the two refusals.
    assert_eq!(
        ungranted.replace("Depot", "X"),
        unknown.replace("Nonexistent", "X"),
        "the two refusals differ by more than the name that was asked for"
    );
}

/// EP-46, T-0284: each formalism is the language it claims to be, so a client that parses one
/// is not handed another. No Turtle parser is a dependency of this workspace, so the three
/// Turtle documents are checked structurally; the LinkML is parsed as the YAML it is.
#[tokio::test]
async fn linkml_parses_as_yaml_and_shacl_rdf_owl_parse_as_turtle() {
    let linkml: Value = serde_norway::from_str(&mcp("linkml", None).await)
        .unwrap_or_else(|error| panic!("the LinkML is not YAML: {error}"));
    assert!(
        linkml.get("classes").is_some(),
        "a LinkML schema declares its classes: {linkml}"
    );

    for format in ["shacl", "owl", "rdf"] {
        let document = mcp(format, None).await;
        assert!(
            document.contains("@prefix"),
            "{format} declares its prefixes:\n{document}"
        );
        assert_eq!(
            document.matches('"').count() % 2,
            0,
            "{format} leaves a string open:\n{document}"
        );
        // Turtle statements are separated by a blank line here, and each one ends in a
        // period; a prefix declaration is a statement of its own on one line.
        let statements: Vec<&str> = document
            .split("\n\n")
            .map(str::trim)
            .filter(|block| !block.is_empty())
            .collect();
        assert!(
            statements.len() > 1,
            "{format} renders no statement:\n{document}"
        );
        for statement in statements {
            let last = statement
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty() && !line.starts_with('#'))
                .unwrap_or_default();
            assert!(
                last.ends_with('.'),
                "a {format} statement is not terminated:\n{statement}"
            );
        }
    }
}

/// AG-29: a model larger than one answer carries is refused with the argument that makes it
/// smaller, and never cut down to the bound — half a schema is not a schema.
#[tokio::test]
async fn a_document_over_the_bound_says_how_to_narrow() {
    let refused = describe(BIG_SLUG, "linkml", None).await;

    assert_eq!(refused["isError"], json!(true), "answered {refused}");
    let words = refused["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        words.contains("entityType"),
        "the refusal names the way to narrow: {words}"
    );
    assert!(
        refused["structuredContent"]["schema"].is_null(),
        "nothing of the document is served with the refusal: {refused}"
    );

    // And the way out works: one class of the same model is under the bound.
    let narrowed = describe(BIG_SLUG, "linkml", Some(json!("Class3"))).await;
    let document = narrowed["structuredContent"]["schema"]["document"]
        .as_str()
        .unwrap_or_else(|| panic!("one class renders: {narrowed}"));
    assert!(
        document.contains("Class3") && !document.contains("Class40"),
        "the narrowed document holds the class that was asked for and no other"
    );
}

/// EP-46, EP-48: the digest in the catalogue is the digest of the bytes a fetch returns, so a
/// client can cache on it. A catalogue whose digests describe some other rendering is worse
/// than one with none.
#[tokio::test]
async fn the_digest_in_the_summary_is_the_digest_of_the_document() {
    let summary = structured("summary", None).await;
    let artifacts = summary["artifacts"]
        .as_array()
        .expect("the summary lists its artifacts")
        .clone();

    for artifact in artifacts {
        let format = artifact["format"].as_str().expect("a format name");
        let served = served_bytes(&structured(format, None).await);
        assert_eq!(
            artifact["bytes"].as_u64(),
            Some(served.len() as u64),
            "the size listed for {format} is the size of the document"
        );
        assert_eq!(
            artifact["sha256"].as_str(),
            Some(sha256_of(&served).as_str()),
            "the digest listed for {format} is the digest of the document"
        );
    }
}

/// EP-47, T-1858: an agent that needs two classes loads them in one read, and the narrowing
/// still holds — a list is not a way past it.
#[tokio::test]
async fn a_list_of_entity_types_describes_each_of_them() {
    let document = mcp_list("linkml", &["User", "Vehicle"]).await;

    assert!(
        document.contains("DESC-User-age") && document.contains("DESC-Vehicle-weight"),
        "both classes are described:\n{document}"
    );
    assert!(
        !document.contains(HIDDEN),
        "the hidden attribute is hidden in a list too:\n{document}"
    );

    // One ungranted name in the list refuses the whole call, by that name.
    let refused = mcp_list("linkml", &["User", "Depot"]).await;
    assert!(
        refused.contains("`Depot` is not an entity type"),
        "answered {refused}"
    );
}

/// `describe_schema` narrowed to a list of entity types.
async fn mcp_list(format: &str, types: &[&str]) -> String {
    let result = describe(SLUG, format, Some(json!(types))).await;
    match result["structuredContent"]["schema"]["document"].as_str() {
        Some(text) => text.to_owned(),
        None => serde_json::to_string(&result).expect("the result serializes"),
    }
}

/// EP-52, DM-46: every artifact the summary advertises is listed as a resource and reads back
/// through the URI the summary gave — a catalogue of URIs that answer nothing is worse than no
/// catalogue. The REST file name names the same document.
#[tokio::test]
async fn every_advertised_schema_uri_reads_back() {
    let listed = rpc(SLUG, "resources/list", json!({})).await;
    let resources = listed["resources"]
        .as_array()
        .unwrap_or_else(|| panic!("the server lists its resources: {listed}"))
        .clone();

    for format in FORMALISMS {
        let uri = format!("schema://{SLUG}/v1/{format}");
        assert!(
            resources
                .iter()
                .any(|resource| resource["uri"] == json!(uri.clone())),
            "{uri} is listed: {listed}"
        );
        let read = rpc(SLUG, "resources/read", json!({ "uri": uri })).await;
        let text = read["contents"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{format} reads back: {read}"));
        assert!(
            !text.contains(HIDDEN),
            "a resource read is narrowed like a tool call:\n{text}"
        );
    }

    // The same document under the spelling the REST route publishes it by.
    let by_file_name = rpc(
        SLUG,
        "resources/read",
        json!({ "uri": format!("schema://{SLUG}/v1/model.shacl.ttl") }),
    )
    .await;
    let by_format = rpc(
        SLUG,
        "resources/read",
        json!({ "uri": format!("schema://{SLUG}/v1/shacl") }),
    )
    .await;
    assert_eq!(
        by_file_name["contents"][0]["text"], by_format["contents"][0]["text"],
        "the two spellings name one document"
    );
}

/// One JSON-RPC method of the façade, as its `result` object.
async fn rpc(slug: &str, method: &str, params: Value) -> Value {
    let payload = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let raw = body_of(
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/endpoint/{slug}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(payload.to_string()))
            .expect("a request"),
    )
    .await;
    let answer: Value = serde_json::from_str(raw.trim_start_matches("data: ").trim())
        .unwrap_or_else(|error| panic!("the façade answered {raw}: {error}"));
    answer["result"].clone()
}
