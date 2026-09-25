//! A grant on a type is a grant on that type, however the entity is addressed (EP-26, MP-02).
//!
//! The gateway narrows a query with `?type=`, but a retrieve by id carries no type at all: it
//! used to send the granted types upstream and trust the broker to honour them. The broker is
//! not the authority on what a caller may read, so what comes back is judged here — and an id
//! whose URN already names an ungranted type is answered before the broker is asked.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const DOMAIN: &str = "hel.fi";
const VEHICLE: &str = "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01";
const DEPOT: &str = "urn:ngsi-ld:Depot:hel.fi:fleet:north";

type Hops = Arc<Mutex<Vec<String>>>;

fn entity(id: &str, kind: &str) -> Value {
    json!({
        "id": id,
        "type": kind,
        "name": { "type": "Property", "value": format!("{kind} north") },
    })
}

/// A broker that answers with whatever entity the path names, ignoring `type` exactly as a
/// broker is free to: the gateway must not depend on it.
async fn broker(answer: Value) -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        let answer = answer.clone();
        async move {
            recorder.lock().expect("the log").push(format!(
                "{}?{}",
                request.uri().path(),
                request.uri().query().unwrap_or_default()
            ));
            // A null answer is the broker's "no such entity", which it reports with a status
            // and not a body: the gateway must not be able to tell that apart from a refusal.
            if answer.is_null() {
                return (StatusCode::NOT_FOUND, axum::Json(json!({}))).into_response();
            }
            axum::Json(answer).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("the stub serves") });
    (format!("http://{address}"), hops)
}

/// One endpoint whose policy grants `Vehicle` alone, with no projection: the grant's type list
/// is the only thing narrowing the answer.
fn endpoint(granted_types: &[&str]) -> Endpoint {
    let entities = granted_types
        .iter()
        .map(|kind| format!("{{ type: {kind} }}"))
        .collect::<Vec<_>>()
        .join(", ");
    let information = if granted_types.is_empty() {
        String::new()
    } else {
        format!("information:\n  - entities: [{entities}]\n")
    };
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: fleet\nassigner: did:web:hel.fi\nassignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, queryBatch, retrieveTemporal, queryTemporal]\n\
         {information}"
    ))
    .expect("the policy parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "fleet".to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![
            Representation::NgsiLd,
            Representation::Json,
            Representation::OgcFeatures,
            Representation::Sta,
        ],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned(), "Depot".to_owned(), "Tram".to_owned()],
            json_schema: None,
            context: Some(json!({ "@context": { "@vocab": "https://hel.fi/schema/" } })),
        }],
        policies: vec![policy],
    }
}

async fn ask(
    answer: Value,
    granted_types: &[&str],
    uri: &str,
) -> (StatusCode, String, Vec<String>) {
    let (upstream, hops) = broker(answer).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .serve([endpoint(granted_types)]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .uri(format!("/api/endpoint/{SLUG}{uri}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let log = hops.lock().expect("the log").clone();
    (status, String::from_utf8_lossy(&bytes).into_owned(), log)
}

/// EP-26: the Depot is not granted, so a retrieve of it by id is not found — whatever the
/// broker chose to answer.
#[tokio::test]
async fn a_retrieve_of_an_ungranted_type_by_id_is_not_found() {
    let (status, body, _) = ask(
        entity(DEPOT, "Depot"),
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{DEPOT}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "body was {body}");
    assert!(!body.contains("Depot north"), "the Depot leaked: {body}");
}

/// EP-26: the temporal tree is the same read, so it answers the same way.
#[tokio::test]
async fn a_temporal_retrieve_of_an_ungranted_type_by_id_is_not_found() {
    let (status, body, _) = ask(
        entity(DEPOT, "Depot"),
        &["Vehicle"],
        &format!("/ngsi-ld/v1/temporal/entities/{DEPOT}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "body was {body}");
    assert!(!body.contains("Depot north"), "the Depot leaked: {body}");
}

/// T-2987, CIM 009 6.19.3.1: one entity's history takes no `type`, and a broker holding to it
/// refused every history read under a type grant with 400. The grant's type is judged on the
/// answer instead, so it reaches the broker without one.
#[tokio::test]
async fn a_temporal_retrieve_by_id_asks_the_broker_without_the_granted_type() {
    let (status, body, log) = ask(
        entity(VEHICLE, "Vehicle"),
        &["Vehicle"],
        &format!("/ngsi-ld/v1/temporal/entities/{VEHICLE}?attrs=name"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body was {body}");
    assert!(
        body.contains("Vehicle north"),
        "the Vehicle is granted: {body}"
    );
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(
        !log[0].contains("type="),
        "the broker was sent a type: {log:?}"
    );
    assert!(log[0].contains("attrs=name"), "{log:?}");
}

/// T-2987: without `type` upstream, a broker answering an entity of another type for a URN that
/// names a granted one is still refused, with the gateway's own miss.
#[tokio::test]
async fn a_temporal_retrieve_by_id_still_refuses_what_the_broker_answers_of_another_type() {
    let (status, body, log) = ask(
        entity(VEHICLE, "Depot"),
        &["Vehicle"],
        &format!("/ngsi-ld/v1/temporal/entities/{VEHICLE}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "body was {body}");
    assert!(!body.contains("Depot north"), "the Depot leaked: {body}");
    assert_eq!(log.len(), 1, "the broker was asked: {log:?}");
}

/// T-2987: the query form keeps the grant's type, which is how it selects (CIM 009 6.18.3.2).
#[tokio::test]
async fn a_temporal_query_still_sends_the_granted_type() {
    let (_, _, log) = ask(
        json!([entity(VEHICLE, "Vehicle")]),
        &["Vehicle"],
        "/ngsi-ld/v1/temporal/entities?attrs=name",
    )
    .await;

    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("type=Vehicle"), "{log:?}");
}

/// T-2987: a SensorThings Datastream reads the same history through the same function, so its
/// Observations reach the broker without `type` as well.
#[tokio::test]
async fn sensorthings_observations_ask_the_broker_without_the_granted_type() {
    let (status, body, log) = ask(
        entity(VEHICLE, "Vehicle"),
        &["Vehicle"],
        &format!("/sta/v1.1/Datastreams('{VEHICLE}/name')/Observations"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body was {body}");
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(
        log[0].starts_with("/ngsi-ld/v1/temporal/entities/"),
        "{log:?}"
    );
    assert!(
        !log[0].contains("type="),
        "the broker was sent a type: {log:?}"
    );
}

/// MP-02: a list leaves the ungranted entity out instead of paging it.
#[tokio::test]
async fn a_query_that_the_broker_answers_too_widely_drops_the_ungranted_type() {
    let (status, body, _) = ask(
        json!([entity(VEHICLE, "Vehicle"), entity(DEPOT, "Depot")]),
        &["Vehicle"],
        "/ngsi-ld/v1/entities?type=Vehicle",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body was {body}");
    assert!(
        body.contains("Vehicle north"),
        "the Vehicle is granted: {body}"
    );
    assert!(!body.contains("Depot north"), "the Depot leaked: {body}");
}

/// EP-26, R20: an id whose URN names an ungranted type is refused without asking the broker,
/// so an ungranted type cannot be probed through the gateway's upstream traffic either.
#[tokio::test]
async fn no_broker_call_is_made_for_a_urn_of_an_ungranted_type() {
    let (status, _, log) = ask(
        entity(DEPOT, "Depot"),
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{DEPOT}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(log.is_empty(), "the broker was asked anyway: {log:?}");
}

/// R20: the refusal is the one an unknown id gets, so the caller learns nothing from the
/// difference between "no such entity" and "not yours".
#[tokio::test]
async fn the_refusal_equals_an_unknown_id_byte_for_byte() {
    let unknown = "urn:ngsi-ld:Vehicle:hel.fi:fleet:nothing-here";
    let (ungranted_status, ungranted_body, _) = ask(
        entity(DEPOT, "Depot"),
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{DEPOT}"),
    )
    .await;
    let (unknown_status, unknown_body, _) = ask(
        Value::Null,
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{unknown}"),
    )
    .await;

    assert_eq!(ungranted_status, unknown_status);
    assert_eq!(ungranted_body, unknown_body);
}

/// An entity that carries two types, one of them granted, is a granted entity.
#[tokio::test]
async fn an_entity_with_two_types_one_granted_is_served() {
    let mut both = entity(VEHICLE, "Vehicle");
    both["type"] = json!(["Depot", "Vehicle"]);
    let (status, body, _) = ask(
        both,
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{VEHICLE}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body was {body}");
    assert!(body.contains("Vehicle north"), "body was {body}");
}

/// An entity whose every type is ungranted is not found, even in a list of types.
#[tokio::test]
async fn an_entity_whose_every_type_is_ungranted_is_not_found() {
    let mut neither = entity(VEHICLE, "Vehicle");
    neither["type"] = json!(["Depot", "Tram"]);
    let (status, body, _) = ask(
        neither,
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{VEHICLE}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "body was {body}");
}

/// A grant that names no type at all is a grant over every type the endpoint serves: the
/// behaviour there is unchanged.
#[tokio::test]
async fn a_grant_over_every_type_is_unchanged() {
    let (status, body, _) = ask(
        entity(DEPOT, "Depot"),
        &[],
        &format!("/ngsi-ld/v1/entities/{DEPOT}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body was {body}");
    assert!(body.contains("Depot north"), "body was {body}");
}

/// A broker that answers a JSON-LD expanded type must be judged by the same rule: the term is
/// read out of the IRI before it is compared with the grant.
#[tokio::test]
async fn an_expanded_type_iri_is_compacted_before_it_is_judged() {
    let mut expanded = entity(VEHICLE, "Depot");
    expanded["type"] = json!("https://hel.fi/schema/Depot");
    let (status, body, _) = ask(
        expanded,
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{VEHICLE}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "body was {body}");
    assert!(!body.contains("Depot north"), "the Depot leaked: {body}");

    let mut granted = entity(VEHICLE, "Vehicle");
    granted["type"] = json!("https://hel.fi/schema/Vehicle");
    let (status, body, _) = ask(
        granted,
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{VEHICLE}"),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "an expanded granted type is served: {body}"
    );
}

/// An id outside the URN scheme of ADR 001 is refused by the scheme itself, before any grant is
/// consulted, so no read reaches the broker with an id whose type cannot be read (EP-26).
#[tokio::test]
async fn an_id_outside_the_urn_scheme_is_refused_before_the_broker() {
    let opaque = "urn:example:north";
    let (status, body, log) = ask(
        entity(opaque, "Vehicle"),
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{opaque}"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "body was {body}");
    assert!(log.is_empty(), "the broker was asked anyway: {log:?}");
}

/// An answer with no `type` at all is not an entity the gateway can judge, so it is not served
/// when the grant names types.
#[tokio::test]
async fn an_answer_without_a_type_is_not_served_under_a_type_grant() {
    let (status, body, _) = ask(
        json!({ "id": VEHICLE, "name": { "type": "Property", "value": "nameless" } }),
        &["Vehicle"],
        &format!("/ngsi-ld/v1/entities/{VEHICLE}"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "body was {body}");
}
