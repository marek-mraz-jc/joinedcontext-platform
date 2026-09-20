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
      slots: [name, age]
    - name: Vehicle
      slots: [name, weight]
"#;

const ATTRS: &[&str] = &["name", "age", "weight"];

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
        hidden_attributes: Default::default(),
        projection: Some(Arc::new(projection.spec)),
        view_mapping: None,
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

async fn gateway() -> Router {
    let upstream = broker().await;
    router(Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
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

/// The document `describe_schema` answers for one format, unwrapped from the MCP envelope.
async fn mcp(format: &str, entity_type: Option<&str>) -> String {
    let mut arguments = json!({ "format": format });
    if let Some(wanted) = entity_type {
        arguments["entityType"] = json!(wanted);
    }
    let payload = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "describe_schema", "arguments": arguments }
    });
    let raw = body_of(
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/endpoint/{SLUG}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(payload.to_string()))
            .expect("a request"),
    )
    .await;
    let answer: Value = serde_json::from_str(raw.trim_start_matches("data: ").trim())
        .unwrap_or_else(|error| panic!("the façade answered {raw}: {error}"));
    let structured = &answer["result"]["structuredContent"]["schema"];
    match structured.get("document").and_then(Value::as_str) {
        Some(document) => document.to_owned(),
        // A JSON formalism is the structured result itself; a refusal carries none, and its
        // words are in the result's own content.
        None if structured.is_object() => {
            serde_json::to_string(structured).expect("the result serializes")
        }
        None => serde_json::to_string(&answer["result"]).expect("the result serializes"),
    }
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
