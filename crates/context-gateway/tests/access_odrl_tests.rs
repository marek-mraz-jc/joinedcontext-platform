//! The access surface as a W3C ODRL 2.2 policy (T-0164, EP-57, R52, R26, MIM3-R10).
//!
//! Two questions. Does the document say the same grants the AuthZEN one says, in the words a
//! data-space connector negotiates in — and does it survive the journey back, which is what
//! R26 asks for and what an agreement compiled from an offer depends on.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use context_gateway::app::{router, Gateway};
use context_gateway::handlers::access_odrl::{read, Grant};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const PATTERN: &str = r"^urn:ngsi-ld:AirQualityObserved:banskabystrica\.sk:ovzdusie:.*$";
const POLYGON: &str = "georel=within;geometry=Polygon;coordinates=[[[19.1,48.7],[19.2,48.7],[19.2,48.8],[19.1,48.8],[19.1,48.7]]]";

fn policy(yaml: &str) -> PolicySpec {
    serde_norway::from_str(yaml).expect("the policy spec parses")
}

/// One public grant with every residual dimension in it, and one prohibition over it.
fn endpoint() -> Endpoint {
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
        catalog: None,
        policies: vec![
            policy(&format!(
                r#"contextSpaceRef: ovzdusie
assigner: did:web:banskabystrica.sk
assignee: {{ kind: role, id: public }}
operations: [queryEntity, retrieveEntity]
information:
  - entities:
      - type: AirQualityObserved
        idPattern: '{PATTERN}'
    propertyNames: [pm10, pm25]
    relationshipNames: [refDevice]
q: "pm10>=0"
scopeQ: "/geo/SK/BB"
geoQ: "{POLYGON}"
temporalQ: "timerel=after;timeAt=P-1D"
"#
            )),
            // Somebody else's grant: the anonymous caller must not learn this type exists.
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
            policy(
                r#"contextSpaceRef: ovzdusie
effect: prohibition
assigner: did:web:banskabystrica.sk
assignee: { kind: role, id: public }
operations: [queryTemporal]
information:
  - entities:
      - type: AirQualityObserved
"#,
            ),
        ],
    }
}

fn app() -> axum::Router {
    router(Arc::new(
        Gateway::new(
            Broker::new("http://127.0.0.1:1"),
            Box::new(PolicyPdp),
            "banskabystrica.sk",
        )
        .serve([endpoint()]),
    ))
}

async fn fetch(accept: &str) -> (StatusCode, String, String) {
    let request = Request::builder()
        .uri(format!("/api/endpoint/{SLUG}/access"))
        .header(header::ACCEPT, accept)
        .body(Body::empty())
        .expect("a request");
    let response = app().oneshot(request).await.expect("the gateway answers");
    let status = response.status();
    let media = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("a readable body");
    (status, media, String::from_utf8_lossy(&body).into_owned())
}

async fn document() -> Value {
    let (status, media, body) = fetch("application/odrl+json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "application/odrl+json", "its own media type (EP-57)");
    serde_json::from_str(&body).expect("the ODRL document is JSON")
}

/// EP-57: the grants as an ODRL 2.2 Set in the `ngsi-ld:` profile.
#[tokio::test]
async fn the_grants_come_back_as_an_odrl_set() {
    let document = document().await;

    assert_eq!(
        document["@context"],
        json!([
            "http://www.w3.org/ns/odrl.jsonld",
            "https://joinedcontext.com/odrl/ngsi-ld/v1/context.jsonld"
        ]),
        "ODRL's own vocabulary, then the profile that names the rest (R52)"
    );
    assert_eq!(document["@type"], json!("Set"));
    assert_eq!(document["assigner"], json!("did:web:banskabystrica.sk"));
    assert_eq!(document["assignee"], json!("public"));

    let permission = document["permission"].as_array().expect("permission[]");
    assert_eq!(permission.len(), 1, "one granted type");
    let rule = &permission[0];
    assert_eq!(
        rule["action"],
        json!(["ngsi-ld:queryEntity", "ngsi-ld:retrieveEntity"]),
        "CIM 009 operations as profile actions"
    );
    assert_eq!(rule["target"]["@type"], json!("ngsi-ld:EntityType"));
    assert_eq!(rule["target"]["uid"], json!("AirQualityObserved"));

    let refinement = rule["target"]["refinement"]
        .as_array()
        .expect("refinement[]");
    let attrs = refinement
        .iter()
        .find(|item| item["leftOperand"] == json!("ngsi-ld:attrs"))
        .expect("the attribute whitelist is a target refinement (EP-57)");
    assert_eq!(attrs["operator"], json!("isAnyOf"));
    assert_eq!(attrs["rightOperand"], json!(["pm10", "pm25", "refDevice"]));

    let prohibition = document["prohibition"].as_array().expect("prohibition[]");
    assert_eq!(prohibition.len(), 1);
    assert_eq!(prohibition[0]["action"], json!(["ngsi-ld:queryTemporal"]));
}

/// EP-57: scope, geography and time are constraints, and each keeps enough of itself to act on.
#[tokio::test]
async fn scope_geography_and_time_become_constraints() {
    let document = document().await;
    let constraints = document["permission"][0]["constraint"]
        .as_array()
        .expect("constraint[]")
        .clone();
    let by_left = |left: &str| {
        constraints
            .iter()
            .find(|item| item["leftOperand"] == json!(left))
            .cloned()
            .unwrap_or(Value::Null)
    };

    assert_eq!(by_left("ngsi-ld:q")["rightOperand"], json!("pm10>=0"));

    let scope = by_left("ngsi-ld:scopeQ");
    assert_eq!(scope["operator"], json!("isPartOf"));
    assert_eq!(scope["rightOperand"], json!("/geo/SK/BB"));

    let geo = by_left("ngsi-ld:geoQ");
    assert_eq!(geo["operator"], json!("ngsi-ld:within"));
    assert_eq!(geo["rightOperand"]["@type"], json!("geojson:Polygon"));
    assert_eq!(
        geo["rightOperand"]["coordinates"][0][0],
        json!([19.1, 48.7]),
        "real GeoJSON, so a partner can act on the area rather than parse our filter"
    );

    let time = constraints
        .iter()
        .find(|item| item["leftOperand"] == json!("dateTime"))
        .expect("the window is a dateTime constraint");
    assert_eq!(time["operator"], json!("gteq"));
    assert_eq!(
        time["rightOperand"],
        json!("P-1D"),
        "a moving window stays a duration; resolving it here would hand out a stale boundary"
    );
}

/// R26: what goes out comes back. The six names the requirement lists survive unchanged.
#[tokio::test]
async fn the_document_reads_back_into_the_grant_it_was_written_from() {
    let document = document().await;
    let grants = read(&document);
    assert_eq!(grants.len(), 2, "one permission and one prohibition");

    let granted = grants
        .iter()
        .find(|grant| !grant.prohibition)
        .expect("the permission");
    assert_eq!(
        granted,
        &Grant {
            prohibition: false,
            assigner: Some("did:web:banskabystrica.sk".to_owned()),
            operations: vec!["queryEntity".to_owned(), "retrieveEntity".to_owned()],
            entity_type: "AirQualityObserved".to_owned(),
            id: None,
            id_pattern: Some(PATTERN.to_owned()),
            attributes: vec!["pm10".to_owned(), "pm25".to_owned(), "refDevice".to_owned()],
            q: Some("pm10>=0".to_owned()),
            scope_q: Some("/geo/SK/BB".to_owned()),
            geo_q: Some(POLYGON.to_owned()),
            temporal_q: Some("timerel=after;timeAt=P-1D".to_owned()),
        },
        "operations, entities, propertyNames, relationshipNames, q and scopeQ are R26's list"
    );

    let taken_back = grants
        .iter()
        .find(|grant| grant.prohibition)
        .expect("the prohibition");
    assert_eq!(taken_back.operations, vec!["queryTemporal".to_owned()]);
    assert_eq!(taken_back.entity_type, "AirQualityObserved");
}

/// EP-59, R20: the other role's type is nowhere in any representation of this caller's grants.
#[tokio::test]
async fn a_type_the_caller_may_not_touch_is_absent() {
    let document = document().await;
    assert!(
        !document.to_string().contains("InternalIncident"),
        "the ODRL document must not become a directory of what exists"
    );
    let (_, _, turtle) = fetch("text/turtle").await;
    assert!(!turtle.contains("InternalIncident"), "nor the Turtle one");
}

/// EP-57: the same policy as RDF, from the same document, so the two cannot drift.
#[tokio::test]
async fn the_same_policy_comes_back_as_turtle() {
    let (status, media, turtle) = fetch("text/turtle").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media, "text/turtle");

    for expected in [
        "@prefix odrl: <http://www.w3.org/ns/odrl/2/> .",
        "@prefix ngsi-ld: <https://joinedcontext.com/odrl/ngsi-ld/v1#> .",
        "a odrl:Set",
        "odrl:assigner <did:web:banskabystrica.sk>",
        "odrl:action ngsi-ld:queryEntity, ngsi-ld:retrieveEntity",
        "odrl:uid \"AirQualityObserved\"",
        "odrl:leftOperand ngsi-ld:scopeQ ; odrl:operator odrl:isPartOf",
        "odrl:permission",
        "odrl:prohibition",
    ] {
        assert!(
            turtle.contains(expected),
            "missing from the Turtle:\n{expected}\n\n{turtle}"
        );
    }
    assert!(
        turtle.trim_end().ends_with('.'),
        "a Turtle document ends its statement"
    );
    assert_eq!(
        turtle.matches('"').count() % 2,
        0,
        "every literal is closed, so the escaping did not lose a quote"
    );
    // A role is not an IRI, so it must not be written as one.
    assert!(turtle.contains("odrl:assignee \"public\""));
}

/// EP-59: two reads of unchanged grants carry one identifier, so a cache can key on it.
#[tokio::test]
async fn the_uid_is_the_digest_of_the_grants() {
    let first = document().await;
    let second = document().await;
    let uid = first["uid"].as_str().expect("a uid");
    assert_eq!(uid, second["uid"].as_str().expect("a uid"));
    assert!(
        uid.contains(&format!("/api/endpoint/{SLUG}/access#sha256:")),
        "the uid names the document it identifies: {uid}"
    );
}

/// EP-56: the default is still the AuthZEN document, and a caller that names it gets it.
#[tokio::test]
async fn the_default_representation_is_unchanged() {
    for accept in ["*/*", "application/json", "application/json, text/html"] {
        let (status, media, body) = fetch(accept).await;
        assert_eq!(status, StatusCode::OK, "for {accept}");
        assert_eq!(media, "application/json", "for {accept}");
        let document: Value = serde_json::from_str(&body).expect("JSON");
        assert!(document["permissions"].is_array(), "for {accept}");
    }
}
