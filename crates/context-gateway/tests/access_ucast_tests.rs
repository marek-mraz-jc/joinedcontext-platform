//! The access surface as a UCAST condition tree (T-0165, EP-58, EP-59).
//!
//! What a client needs to filter before it asks: one entry per type it may see, the operations
//! it may call, the attributes it may project, and the conditions the gateway would add anyway.
//! And nothing about anything else.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const GRANT_AST: &str = "application/vnd.joinedcontext.grant-ast+json";
const POLYGON: &str = "georel=within;geometry=Polygon;coordinates=[[[19.1,48.7],[19.2,48.7],[19.2,48.8],[19.1,48.8],[19.1,48.7]]]";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

fn endpoint(policies: Vec<PolicySpec>) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "ovzdusie".to_owned(),
        project: "ovzdusie".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        policies,
    }
}

async fn tree(policies: Vec<PolicySpec>) -> Value {
    let app = router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint(policies)]),
    ));
    let request = Request::builder()
        .uri(format!("/api/endpoint/{SLUG}/access"))
        .header(header::ACCEPT, GRANT_AST)
        .body(Body::empty())
        .expect("a request");
    let response = app.oneshot(request).await.expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(GRANT_AST),
        "its own media type (EP-58)"
    );
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    serde_json::from_slice(&body).expect("the grant AST is JSON")
}

fn full_grant() -> PolicySpec {
    policy(&format!(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10, pm25]
q: "pm10>=0"
scopeQ: "/geo/SK/BB"
geoQ: "{POLYGON}"
temporalQ: "timerel=after;timeAt=P-1D"
"#
    ))
}

/// EP-58: operations, projection and the condition tree, keyed by entity type.
#[tokio::test]
async fn the_residual_comes_back_as_a_condition_tree() {
    let document = tree(vec![full_grant()]).await;
    let entry = &document["AirQualityObserved"];

    assert_eq!(
        entry["operations"],
        json!(["queryEntity", "retrieveEntity"])
    );
    assert_eq!(entry["project"], json!(["pm10", "pm25"]));
    assert_eq!(
        entry["q"],
        json!("pm10>=0"),
        "R56 leaves the gateway no NGSI-LD grammar, so q travels verbatim rather than as a branch"
    );

    let where_ = &entry["where"];
    assert_eq!(where_["type"], json!("compound"));
    assert_eq!(where_["operator"], json!("and"));
    let branches = where_["value"].as_array().expect("branches");
    assert_eq!(branches.len(), 3, "scope, geography and time");

    let by_operator = |operator: &str| {
        branches
            .iter()
            .find(|branch| branch["operator"] == json!(operator))
            .cloned()
            .unwrap_or(Value::Null)
    };

    let scope = by_operator("scope_under");
    assert_eq!(scope["type"], json!("field"));
    assert_eq!(scope["field"], json!("scope"));
    assert_eq!(scope["value"], json!("/geo/SK/BB"));

    let geo = by_operator("geo_within");
    assert_eq!(geo["field"], json!("location"));
    assert_eq!(geo["value"]["type"], json!("Polygon"));
    assert_eq!(geo["value"]["coordinates"][0][0], json!([19.1, 48.7]));

    let time = by_operator("gte");
    assert_eq!(time["field"], json!("observedAt"));
    assert_eq!(
        time["value"],
        json!({ "relative": "P-1D" }),
        "a moving window stays relative, or the client compiles a boundary that is already stale"
    );
}

/// EP-58: a closed window is one `time_between` leaf, not two.
#[tokio::test]
async fn a_closed_window_is_one_leaf() {
    let document = tree(vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryTemporal]
information:
  - entities:
      - type: AirQualityObserved
temporalQ: "timerel=between;timeAt=2026-01-01T00:00:00Z;endTimeAt=2026-02-01T00:00:00Z"
"#,
    )])
    .await;
    let leaf = &document["AirQualityObserved"]["where"];
    assert_eq!(leaf["operator"], json!("time_between"));
    assert_eq!(
        leaf["value"],
        json!(["2026-01-01T00:00:00Z", "2026-02-01T00:00:00Z"])
    );
}

/// EP-58: no whitelist reaches every attribute, and an unnarrowed grant carries no `where`.
#[tokio::test]
async fn an_unconstrained_grant_projects_everything_and_filters_nothing() {
    let document = tree(vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
    )])
    .await;
    let entry = &document["AirQualityObserved"];
    assert_eq!(entry["project"], json!("*"));
    assert!(entry.get("where").is_none(), "nothing to narrow");
    assert!(entry.get("q").is_none());
}

/// Two grants on one type are two ways in, so the caller may see what satisfies either (R7).
#[tokio::test]
async fn two_grants_on_one_type_are_a_union() {
    let narrow = |scope: &str, q: &str| {
        policy(&format!(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
    propertyNames: [pm10]
q: "{q}"
scopeQ: "{scope}"
"#
        ))
    };
    let document = tree(vec![
        narrow("/geo/SK/BB", "pm10>=0"),
        narrow("/geo/SK/ZV", "pm25>=0"),
    ])
    .await;
    let entry = &document["AirQualityObserved"];

    assert_eq!(entry["where"]["operator"], json!("or"));
    let branches = entry["where"]["value"].as_array().expect("two branches");
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0]["value"], json!("/geo/SK/BB"));
    assert_eq!(branches[1]["value"], json!("/geo/SK/ZV"));
    assert_eq!(
        entry["q"],
        json!("(pm10>=0)|(pm25>=0)"),
        "NGSI-LD writes OR as | and every operand is parenthesized"
    );
}

/// A grant that narrows nothing subsumes one that does: the caller may see the type outright.
#[tokio::test]
async fn an_open_grant_subsumes_a_narrow_one() {
    let document = tree(vec![
        full_grant(),
        policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
"#,
        ),
    ])
    .await;
    let entry = &document["AirQualityObserved"];
    assert!(
        entry.get("where").is_none() && entry.get("q").is_none(),
        "repeating the narrow grant would describe a smaller right than the caller holds"
    );
    assert_eq!(entry["project"], json!("*"));
}

/// GW8: a prohibition takes its operations off the grant, and a type with none left goes away.
#[tokio::test]
async fn a_prohibition_takes_operations_off_and_can_remove_the_type() {
    let prohibit = |operations: &str| {
        policy(&format!(
            r#"contextSpaceRef: ovzdusie
effect: prohibition
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [{operations}]
information:
  - entities:
      - type: AirQualityObserved
"#
        ))
    };

    let partial = tree(vec![full_grant(), prohibit("retrieveEntity")]).await;
    assert_eq!(
        partial["AirQualityObserved"]["operations"],
        json!(["queryEntity"]),
        "what is taken back is gone from the list"
    );

    let total = tree(vec![full_grant(), prohibit("queryEntity, retrieveEntity")]).await;
    assert_eq!(
        total,
        json!({}),
        "a type the caller may do nothing with has no place in a document about what it may do"
    );
}

/// EP-59, R20: a type granted to somebody else is absent, not listed as denied.
#[tokio::test]
async fn another_roles_type_is_absent() {
    let document = tree(vec![
        full_grant(),
        policy(
            r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: steward }
operations: [createEntity]
information:
  - entities:
      - type: InternalIncident
"#,
        ),
    ])
    .await;
    assert!(
        !document.to_string().contains("InternalIncident"),
        "the grant AST must not become a directory of what exists"
    );
    assert_eq!(document.as_object().expect("an object").len(), 1);
}

/// An unreadable geometry is still a constraint the caller is under, so it must not vanish.
#[tokio::test]
async fn a_geometry_that_cannot_be_read_is_still_reported() {
    let document = tree(vec![policy(
        r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryEntity]
information:
  - entities:
      - type: AirQualityObserved
geoQ: "georel=near;maxDistance==2000"
"#,
    )])
    .await;
    let leaf = &document["AirQualityObserved"]["where"];
    assert_eq!(leaf["operator"], json!("geo_within"));
    assert_eq!(
        leaf["value"]["geoQ"],
        json!("georel=near;maxDistance==2000"),
        "leaving it out would describe a wider grant than the one in force"
    );
}
