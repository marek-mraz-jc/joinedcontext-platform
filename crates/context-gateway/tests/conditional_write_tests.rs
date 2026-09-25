//! T-0152: what happens between the decision and the write (R45, GW16, GW19).
//!
//! GW16 checks the payload. This checks the other half: the entity as the broker holds it.
//! A grant with a `q` condition is only true of a stored state, and between reading that
//! state and writing over it the entity can move, so the write goes upstream carrying the
//! entity tag it was decided on. What the caller sends in `If-Match` is checked by the very
//! same read.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::conditional::matches;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "c9k2vt7xm4qz8ndrb6hs3wfjp5";
const PROJECT: &str = "banskabystrica";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const SENSOR: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:senzor-01";
/// The entity tag the broker publishes for the sensor as it is stored now.
const ETAG: &str = "\"7f3a\"";

/// One request the gateway made upstream.
#[derive(Debug, Clone)]
struct Call {
    method: Method,
    path: String,
    query: String,
    if_match: Option<String>,
}

type Calls = Arc<Mutex<Vec<Call>>>;

/// What the broker holds and what it publishes about it.
#[derive(Clone)]
struct Stored {
    /// The entity, or nothing when the broker holds none.
    entity: Option<Value>,
    /// The entity tag, or nothing for a broker that publishes none — which is Antares today.
    etag: Option<&'static str>,
    /// Whether the stored entity still satisfies the grant's own filter.
    condition_holds: bool,
    calls: Calls,
}

fn sensor() -> Value {
    json!({
        "id": SENSOR,
        "type": "AirQualityObserved",
        "temperature": { "type": "Property", "value": 19.5 },
        "location": {
            "type": "GeoProperty",
            "value": { "type": "Point", "coordinates": [19.15, 48.73] }
        }
    })
}

/// A broker that answers the three things a conditional write asks it: does the entity
/// exist, what is its tag, and does the grant's filter still match it.
async fn broker(stored: Stored) -> (String, Calls) {
    let calls = Arc::clone(&stored.calls);
    let app = Router::new()
        .fallback(any(
            |State(stored): State<Stored>, request: Request| async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let query = request.uri().query().unwrap_or_default().to_owned();
                stored.calls.lock().expect("the call log").push(Call {
                    method: method.clone(),
                    path: path.clone(),
                    query: query.clone(),
                    if_match: request
                        .headers()
                        .get("if-match")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                });

                if method == Method::GET && path == "/ngsi-ld/v1/entities" {
                    let found = match stored.condition_holds {
                        true => stored.entity.clone().into_iter().collect::<Vec<_>>(),
                        false => Vec::new(),
                    };
                    return axum::Json(Value::Array(found)).into_response();
                }
                if method == Method::GET {
                    let Some(entity) = stored.entity.clone() else {
                        return (
                            StatusCode::NOT_FOUND,
                            axum::Json(json!({
                                "type": "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound",
                                "title": "ResourceNotFound",
                                "status": 404,
                            })),
                        )
                            .into_response();
                    };
                    let mut headers = HeaderMap::new();
                    if let Some(etag) = stored.etag {
                        headers.insert("etag", etag.parse().expect("a header value"));
                    }
                    return (StatusCode::OK, headers, axum::Json(entity)).into_response();
                }
                StatusCode::NO_CONTENT.into_response()
            },
        ))
        .with_state(stored);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), calls)
}

/// A grant over the sensor's own attributes, with or without a condition on its state.
fn policy(condition: Option<&str>) -> PolicySpec {
    let filter = condition.map_or(String::new(), |q| format!("q: {q}\n"));
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [retrieveEntity, queryEntity, updateAttrs, mergeEntity, deleteEntity]\n\
         {filter}\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20   propertyNames: [temperature, location]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(condition: Option<&str>) -> Endpoint {
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
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![policy(condition)],
    }
}

/// One write through the gateway, and everything the broker was asked on the way.
async fn write(
    method: Method,
    if_match: Option<&str>,
    condition: Option<&str>,
    stored: Stored,
) -> (StatusCode, Vec<Call>) {
    let (upstream, calls) = broker(stored).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .serve([endpoint(condition)]),
    );

    let mut request = Request::builder()
        .method(method.clone())
        .uri(format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{SENSOR}/attrs"
        ))
        .header("content-type", "application/json");
    if let Some(tag) = if_match {
        request = request.header("if-match", tag);
    }
    let body = match method == Method::DELETE {
        true => Body::empty(),
        false => {
            Body::from(json!({ "temperature": { "type": "Property", "value": 21.0 } }).to_string())
        }
    };
    let uri = match method == Method::DELETE {
        true => format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{SENSOR}"),
        false => format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{SENSOR}/attrs"),
    };
    let response = router(gateway)
        .oneshot(request.uri(uri).body(body).expect("a request"))
        .await
        .expect("the gateway answers");

    let status = response.status();
    let made = calls.lock().expect("the call log").clone();
    (status, made)
}

fn holding(etag: Option<&'static str>, condition_holds: bool) -> Stored {
    Stored {
        entity: Some(sensor()),
        etag,
        condition_holds,
        calls: Arc::new(Mutex::new(Vec::new())),
    }
}

fn empty() -> Stored {
    Stored {
        entity: None,
        etag: None,
        condition_holds: false,
        calls: Arc::new(Mutex::new(Vec::new())),
    }
}

fn writes(calls: &[Call]) -> Vec<&Call> {
    calls
        .iter()
        .filter(|call| call.method != Method::GET)
        .collect()
}

#[tokio::test]
async fn a_write_with_no_condition_and_no_if_match_is_forwarded_without_a_read() {
    let (status, calls) = write(Method::PATCH, None, None, holding(Some(ETAG), true)).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        calls.len(),
        1,
        "an unconditional write costs one hop: {calls:?}"
    );
    assert_eq!(calls[0].method, Method::PATCH);
    assert!(calls[0].if_match.is_none(), "nothing to be conditional on");
}

#[tokio::test]
async fn an_if_match_that_agrees_with_the_stored_tag_is_forwarded_carrying_it() {
    let (status, calls) = write(Method::PATCH, Some(ETAG), None, holding(Some(ETAG), true)).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    let written = writes(&calls);
    assert_eq!(written.len(), 1, "the write happened once: {calls:?}");
    assert_eq!(
        written[0].if_match.as_deref(),
        Some(ETAG),
        "the write is conditional upstream too, so a change in between is the broker's 412"
    );
}

#[tokio::test]
async fn an_if_match_that_disagrees_is_412_and_the_write_never_reaches_the_broker() {
    let (status, calls) = write(
        Method::PATCH,
        Some("\"stale\""),
        None,
        holding(Some(ETAG), true),
    )
    .await;

    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(writes(&calls).is_empty(), "nothing was written: {calls:?}");
}

/// RFC 9110 section 13.1.1: `*` asks only that the entity exist.
#[tokio::test]
async fn if_match_star_passes_on_an_entity_that_exists_and_fails_on_one_that_does_not() {
    let (present, _) = write(Method::PATCH, Some("*"), None, holding(Some(ETAG), true)).await;
    assert_eq!(present, StatusCode::NO_CONTENT);

    let (absent, calls) = write(Method::PATCH, Some("*"), None, empty()).await;
    assert_eq!(absent, StatusCode::PRECONDITION_FAILED);
    assert!(writes(&calls).is_empty(), "nothing was written: {calls:?}");
}

/// A broker that publishes no entity tag cannot have its preconditions evaluated, and a
/// precondition the platform cannot evaluate must not be treated as met.
#[tokio::test]
async fn an_if_match_against_a_broker_that_publishes_no_etag_is_412() {
    let (status, calls) = write(Method::PATCH, Some(ETAG), None, holding(None, true)).await;

    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(writes(&calls).is_empty(), "nothing was written: {calls:?}");
}

#[tokio::test]
async fn a_state_dependent_grant_reads_first_and_writes_conditionally() {
    let (status, calls) = write(
        Method::PATCH,
        None,
        Some("temperature<100"),
        holding(Some(ETAG), true),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    let condition = calls
        .iter()
        .find(|call| call.method == Method::GET && call.path == "/ngsi-ld/v1/entities")
        .expect("the grant's filter was evaluated by the broker");
    assert!(
        condition.query.contains("temperature%3C100"),
        "the grant's own filter is what was asked: {}",
        condition.query
    );
    assert!(
        condition.query.contains("id=urn%3Angsi-ld") || condition.query.contains("id=urn:ngsi-ld"),
        "about this entity alone: {}",
        condition.query
    );
    assert_eq!(
        writes(&calls)[0].if_match.as_deref(),
        Some(ETAG),
        "the window between the read and the write is closed by the tag it was read at"
    );
}

/// The half GW16 cannot see: the payload sits inside the grant, the stored entity does not.
#[tokio::test]
async fn a_write_whose_stored_state_fails_the_condition_is_refused_and_never_forwarded() {
    let (status, calls) = write(
        Method::PATCH,
        None,
        Some("temperature<100"),
        holding(Some(ETAG), false),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an entity the caller may not touch is a miss, not a refusal (R20)"
    );
    assert!(writes(&calls).is_empty(), "nothing was written: {calls:?}");
}

/// GW19: a delete is a write, and gets the same treatment.
#[tokio::test]
async fn a_delete_is_conditional_too() {
    let (status, calls) = write(
        Method::DELETE,
        Some(ETAG),
        Some("temperature<100"),
        holding(Some(ETAG), true),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    let written = writes(&calls);
    assert_eq!(written[0].method, Method::DELETE);
    assert_eq!(written[0].if_match.as_deref(), Some(ETAG));
}

#[tokio::test]
async fn a_conditional_write_on_an_entity_the_broker_does_not_hold_is_a_miss() {
    let (status, calls) = write(Method::PATCH, None, Some("temperature<100"), empty()).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(writes(&calls).is_empty(), "nothing was written: {calls:?}");
}

#[test]
fn the_strong_comparison_is_the_one_rfc_9110_asks_for() {
    assert!(matches("*", Some(ETAG)), "any representation will do");
    assert!(matches("*", None), "even one whose tag is unknown");
    assert!(matches(ETAG, Some(ETAG)));
    assert!(
        matches("\"a\", \"7f3a\"", Some(ETAG)),
        "a list matches by member"
    );

    assert!(!matches(ETAG, None), "no tag satisfies nothing");
    assert!(!matches(ETAG, Some("\"other\"")));
    assert!(
        !matches("W/\"7f3a\"", Some("W/\"7f3a\"")),
        "a weak tag is not strongly equal to anything, itself included"
    );
    assert!(
        !matches("7f3a", Some("7f3a")),
        "an entity tag is quoted; an unquoted one is not one"
    );
}

// --- the grant that draws an area rather than a condition (T-0807) -------------------------

/// The district the grant draws, far from where the sensor is stored (19.15, 48.73).
const DISTRICT: &str = "georel=within;geometry=Polygon;coordinates=[[[21.0,48.0],[21.5,48.0],[21.5,48.5],[21.0,48.5],[21.0,48.0]]]";

/// A grant over the sensor, narrowed by whatever `narrowing` says — an area, a filter, or
/// nothing — and allowing exactly the operations named.
///
/// The area case is the one that used to disappear: a `geoQ` forwarded to the broker left
/// the constraint set with no area of its own, so nothing here knew the grant drew one.
fn grant(operations: &str, narrowing: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [{operations}]\n\
         {narrowing}\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20   propertyNames: [temperature, location]\n"
    ))
    .expect("the policy spec parses")
}

/// The grant of the two area tests: one district, no filter.
fn in_the_district(operations: &str) -> PolicySpec {
    grant(operations, &format!("geoQ: '{DISTRICT}'"))
}

/// One request through a gateway serving `policies`, and everything the broker was asked.
async fn through(
    policies: Vec<PolicySpec>,
    method: Method,
    uri: &str,
    body: Body,
    stored: Stored,
) -> (StatusCode, Vec<Call>) {
    let (upstream, calls) = broker(stored).await;
    let endpoint = Endpoint {
        roles: Default::default(),
        policies,
        ..endpoint(None)
    };
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/api/endpoint/{SLUG}{uri}"))
                .header("content-type", "application/json")
                .body(body)
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    let status = response.status();
    let made = calls.lock().expect("the call log").clone();
    (status, made)
}

/// The grant is an area and the payload carries no location, so GW16 has nothing to test:
/// the only thing that says where this entity is, is the entity as the broker holds it.
#[tokio::test]
async fn a_grant_that_is_only_an_area_reads_the_stored_entity_before_it_edits_one() {
    let (status, calls) = through(
        vec![in_the_district("retrieveEntity, queryEntity, updateAttrs")],
        Method::PATCH,
        &format!("/ngsi-ld/v1/entities/{SENSOR}/attrs"),
        Body::from(json!({ "temperature": { "type": "Property", "value": 21.0 } }).to_string()),
        holding(Some(ETAG), true),
    )
    .await;

    // A write to an entity the caller cannot see is a miss, not a refusal (R20).
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        writes(&calls).is_empty(),
        "the entity is stored outside the granted district, so nothing was written: {calls:?}"
    );
    assert!(
        calls.iter().any(|call| call.method == Method::GET),
        "the stored entity was read before the decision: {calls:?}"
    );
}

/// The same grant, the same district, an entity stored inside it: the write goes through and
/// carries the tag it was decided on.
#[tokio::test]
async fn a_grant_that_is_only_an_area_still_lets_a_write_inside_it_through() {
    let inside = Stored {
        entity: Some(json!({
            "id": SENSOR,
            "type": "AirQualityObserved",
            "temperature": { "type": "Property", "value": 19.5 },
            "location": {
                "type": "GeoProperty",
                "value": { "type": "Point", "coordinates": [21.2, 48.2] }
            }
        })),
        ..holding(Some(ETAG), true)
    };
    let (status, calls) = through(
        vec![in_the_district("retrieveEntity, queryEntity, updateAttrs")],
        Method::PATCH,
        &format!("/ngsi-ld/v1/entities/{SENSOR}/attrs"),
        Body::from(json!({ "temperature": { "type": "Property", "value": 21.0 } }).to_string()),
        inside,
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        writes(&calls)[0].if_match.as_deref(),
        Some(ETAG),
        "a state-dependent write is conditional upstream (R45)"
    );
}

/// A batch names its entities in the payload, so there is no stored entity to read and no
/// tag to carry. The payload alone cannot decide a grant that decides from stored state.
#[tokio::test]
async fn a_batch_write_under_a_state_dependent_grant_never_reaches_the_broker() {
    const ALLOWED: &str = "retrieveEntity, queryEntity, upsertBatch";
    for narrowing in ["q: temperature<100", &format!("geoQ: '{DISTRICT}'")] {
        let (status, calls) = through(
            vec![grant(ALLOWED, narrowing)],
            Method::POST,
            "/ngsi-ld/v1/entityOperations/upsert",
            // Inside the granted district, and a legal payload in every other way: what is
            // refused is the operation, not the body.
            Body::from(
                json!([{
                    "id": SENSOR,
                    "type": "AirQualityObserved",
                    "temperature": { "type": "Property", "value": 21.0 },
                    "location": {
                        "type": "GeoProperty",
                        "value": { "type": "Point", "coordinates": [21.2, 48.2] }
                    }
                }])
                .to_string(),
            ),
            holding(Some(ETAG), true),
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(calls.is_empty(), "the broker was never called: {calls:?}");
    }
}
