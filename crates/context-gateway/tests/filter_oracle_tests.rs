//! A filter is a read: filtering on an attribute a type may not serve tells the caller its value
//! (T-1862, owner's rule of 2026-09-18; MP-02, R9, EP-61).
//!
//! Before this, `q`, `attrs`, `orderBy` and `geoproperty` reached the broker exactly as the caller
//! wrote them and the answer was stripped afterwards. The rows that came back were the rows that
//! matched, so `q=age>30` over a type whose `age` this endpoint hides still said which entities
//! have an `age` over thirty — and bisecting the number reads the value itself, one comparison at
//! a time. The gateway now decides which types may be *considered* at all: a type stays in the
//! query only if every attribute the request filters or orders on is one it may serve, and a query
//! with no type left is answered as an attribute that does not exist would be answered.
//!
//! The fixture is the one the task names: a projection giving `User` `[age, name]` and `Vehicle`
//! `[weight, location]`, over a broker whose every entity carries every attribute, so a leak shows.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "4hq2vbn7kzxr3m5tjw6cyd9pfs";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";

const VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: User
      slots: [age, name, homeLocation, ageGroup]
    - name: Vehicle
      slots: [weight, location]
"#;

fn projection() -> Arc<ModelProjectionSpec> {
    let parsed = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(VIEW).expect("parses");
    parsed.validate().expect("valid");
    Arc::new(parsed.spec)
}

/// Every entity carries every attribute, so anything that leaks is visible in the answer.
fn everything() -> Value {
    json!([
        {
            "id": "urn:ngsi-ld:User:hel.fi:fleet:aino",
            "type": "User",
            "name": { "type": "Property", "value": "Aino" },
            "age": { "type": "Property", "value": 41 },
            "ageGroup": { "type": "Property", "value": "adult" },
            "weight": { "type": "Property", "value": 70 },
            "secretPin": { "type": "Property", "value": "1234" }
        },
        {
            "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
            "type": "Vehicle",
            "weight": { "type": "Property", "value": 12_000 },
            "age": { "type": "Property", "value": 7 },
            "secretPin": { "type": "Property", "value": "4321" }
        }
    ])
}

type Hops = Arc<Mutex<Vec<String>>>;

async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.uri().query().unwrap_or_default().to_owned());
            axum::Json(everything())
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
         operations: [queryEntity, retrieveEntity, queryBatch, queryTemporal]\n"
    ))
    .expect("the policy spec parses")
}

/// A grant that whitelists attributes rather than a projection: the other half of the same rule.
fn policy_granting(names: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Vehicle\n\
         \x20   propertyNames: [{names}]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(projected: bool, hidden: &[&str]) -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: projected.then(projection),
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["User".to_owned(), "Vehicle".to_owned()],
            json_schema: None,
            context: None,
        }],
        policies: vec![policy()],
    }
}

/// The same endpoint under a grant whose whitelist is the whole of the narrowing.
fn endpoint_granting(names: &str) -> Endpoint {
    Endpoint {
        roles: Default::default(),
        policies: vec![policy_granting(names)],
        ..endpoint(false, &[])
    }
}

/// One query, with the status, the answer and every query string the broker was asked.
async fn ask(endpoint: Endpoint, uri: &str) -> (StatusCode, Value, Vec<String>) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/endpoint/{SLUG}{uri}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let asked = hops.lock().expect("the hop log").clone();
    (status, body, asked)
}

/// The types one answer carries, in the order they came.
fn types(body: &Value) -> Vec<String> {
    body.as_array()
        .map(|entities| {
            entities
                .iter()
                .filter_map(|entity| entity["type"].as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// MP-02: `age` belongs to `User`, so a query filtering on it is a query about Users.
#[tokio::test]
async fn q_on_an_attribute_of_one_type_matches_that_type_only() {
    let (status, body, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=age%3E30",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(types(&body), vec!["User".to_owned()], "{body}");
    assert!(
        asked[0].contains("type=User") && !asked[0].contains("Vehicle"),
        "the broker is asked for Users only: {}",
        asked[0]
    );
}

/// The oracle itself: asking the same question with three thresholds must tell the caller
/// nothing, and must not even reach the broker.
#[tokio::test]
async fn bisecting_a_hidden_value_learns_nothing() {
    let mut answers = BTreeSet::new();
    for threshold in [0, 50, 100] {
        let (status, body, asked) = ask(
            endpoint(true, &[]),
            &format!("/ngsi-ld/v1/entities?type=Vehicle&q=age%3E{threshold}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!([]), "threshold {threshold}: {body}");
        assert!(
            asked.is_empty(),
            "the broker was asked at threshold {threshold}: {asked:?}"
        );
        answers.insert(body.to_string());
    }
    assert_eq!(
        answers.len(),
        1,
        "three thresholds, three identical answers, or the answer is the value"
    );
}

/// EP-61: an attribute the endpoint hides is the same oracle, with no projection in sight.
#[tokio::test]
async fn one_type_endpoint_with_a_hidden_attribute_is_not_an_oracle() {
    let (status, body, asked) = ask(
        endpoint(false, &["secretPin"]),
        "/ngsi-ld/v1/entities?type=Vehicle&q=secretPin%3D%3D%221234%22",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(asked.is_empty(), "{asked:?}");
}

/// A request naming no type is every granted type, and it is pruned the same way.
#[tokio::test]
async fn no_type_in_the_request_is_pruned_the_same() {
    let (status, body, asked) = ask(endpoint(true, &[]), "/ngsi-ld/v1/entities?q=age%3E30").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(types(&body), vec!["User".to_owned()], "{body}");
    assert!(!asked[0].contains("Vehicle"), "{}", asked[0]);
}

/// Existence and non-existence are references too: `!age` over a Vehicle would answer "every
/// Vehicle", and that answer is only interesting because `age` is there to be missing.
#[tokio::test]
async fn negation_and_non_existence_are_references() {
    for q in ["%21age", "age"] {
        let (status, body, _) = ask(
            endpoint(true, &[]),
            &format!("/ngsi-ld/v1/entities?type=User,Vehicle&q={q}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{q}: {body}");
        assert_eq!(types(&body), vec!["User".to_owned()], "{q}: {body}");
    }
}

/// Strict on purpose: an `|` branch still decides whether a row comes back, so a type that may
/// not serve one side of it leaves the query whole.
#[tokio::test]
async fn an_or_branch_with_a_foreign_attribute_drops_the_type() {
    let (status, body, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=age%3E30%7Cweight%3E100",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "neither type may serve both names: {body}");
    assert!(asked.is_empty(), "{asked:?}");
}

/// The same for an `and` of two types' attributes: no single type can satisfy it.
#[tokio::test]
async fn an_and_of_two_types_attributes_matches_nothing() {
    let (status, body, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=age%3E30%3Bweight%3E100",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(asked.is_empty(), "{asked:?}");
}

/// Parentheses, value lists, ranges and patterns are read for their attribute names, not their
/// values: `age==18,21` references `age` and nothing called `21`.
#[tokio::test]
async fn parentheses_value_lists_ranges_and_patterns_are_parsed() {
    let (status, body, _) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=(age%3D%3D18,21%7Cage%3D%3D30..40);name~%3D%22%5EA%22",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(types(&body), vec!["User".to_owned()], "{body}");

    // The same shape with a Vehicle's attribute inside the parentheses leaves no type at all.
    let (status, body, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=(age%3D%3D18,21%7Cweight%3D%3D30..40)",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(asked.is_empty(), "{asked:?}");
}

/// A path is judged by its head: `age.observedAt` and `age[value]` are about `age`.
#[tokio::test]
async fn a_dotted_or_bracketed_path_is_its_head() {
    for q in [
        "age.observedAt%3E2026-01-01T00:00:00Z",
        "age%5Bvalue%5D%3E3",
    ] {
        let (status, body, _) = ask(
            endpoint(true, &[]),
            &format!("/ngsi-ld/v1/entities?type=User,Vehicle&q={q}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{q}: {body}");
        assert_eq!(types(&body), vec!["User".to_owned()], "{q}: {body}");
    }
}

/// A name that contains another is a different name: `ageGroup` is not `age`, and a type that may
/// serve one is not thereby allowed to be filtered on the other.
#[tokio::test]
async fn a_name_that_contains_another_is_not_confused() {
    // `ageGroup` is a User's slot, so it behaves like `age`: Users only.
    let (status, body, _) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=ageGroup%3D%3D%22adult%22",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(types(&body), vec!["User".to_owned()], "{body}");

    // `weightage` is nobody's slot: it does not pass as `weight`.
    let (status, body, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=weightage%3E1",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(asked.is_empty(), "{asked:?}");
}

/// An expanded IRI and the short name it expands to are one name.
#[tokio::test]
async fn an_expanded_iri_and_a_short_name_are_one_name() {
    for q in ["https%3A%2F%2Fschema.org%2Fage%3E30", "%61ge%3E30"] {
        let (status, body, _) = ask(
            endpoint(true, &[]),
            &format!("/ngsi-ld/v1/entities?type=User,Vehicle&q={q}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{q}: {body}");
        assert_eq!(types(&body), vec!["User".to_owned()], "{q}: {body}");
    }
}

/// NGSI-LD names are case sensitive, so `Age` is an unknown attribute rather than `age`.
#[tokio::test]
async fn attribute_names_are_case_sensitive() {
    let (status, body, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=Age%3E30",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "no type serves `Age`: {body}");
    assert!(asked.is_empty(), "{asked:?}");
}

/// CIM 009's `attrs` selects the entities that carry one of the names, so it is a reference too.
#[tokio::test]
async fn attrs_is_a_selector_too() {
    let (status, body, _) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&attrs=age",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        types(&body),
        vec!["User".to_owned()],
        "a Vehicle stripped to id and type still says it exists: {body}"
    );
}

/// `orderBy` decides which rows come first, which over a page decides which come back at all.
#[tokio::test]
async fn ordering_on_a_foreign_attribute_drops_the_type() {
    let (status, body, _) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&orderBy=%21age",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(types(&body), vec!["User".to_owned()], "{body}");
}

/// A filter on what every entity carries is not a reference to anything a grant could withhold.
#[tokio::test]
async fn filtering_on_the_structure_narrows_nothing() {
    let (status, body, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities?type=User,Vehicle&q=createdAt%3E2026-01-01T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        types(&body),
        vec!["User".to_owned(), "Vehicle".to_owned()],
        "{body}"
    );
    assert!(
        asked[0].contains("User") && asked[0].contains("Vehicle"),
        "{}",
        asked[0]
    );
}

/// An addressed read answers as an entity that is not there would: `404`, not an empty entity.
#[tokio::test]
async fn an_addressed_read_filtered_on_a_foreign_attribute_is_not_found() {
    let (status, _, asked) = ask(
        endpoint(true, &[]),
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01?type=Vehicle&attrs=age",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(asked.is_empty(), "{asked:?}");
}

/// R9: a grant's own attribute whitelist is an oracle the same way a projection is — and it is the
/// whitelist a filter is judged against, never the `attrs` the caller asked for.
#[tokio::test]
async fn a_grants_whitelist_is_judged_and_the_callers_own_attrs_are_not() {
    // `secretPin` is outside the grant: filtering on it matches nothing and asks nobody.
    let (status, body, asked) = ask(
        endpoint_granting("weight, location"),
        "/ngsi-ld/v1/entities?type=Vehicle&q=secretPin%3D%3D%224321%22",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(asked.is_empty(), "{asked:?}");

    // And a caller who narrows their own request to one granted attribute may still filter on
    // another granted one: `attrs` is a selection, not a permission.
    let (status, body, asked) = ask(
        endpoint_granting("weight, location"),
        "/ngsi-ld/v1/entities?type=Vehicle&attrs=weight&q=location%21%3D%22nowhere%22",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!asked.is_empty(), "the broker is asked: {asked:?}");
}

/// The same broker, recording the body as well: a batch query writes its selector there, so the
/// hop log has to carry it to prove what the broker was allowed to see (T-2259).
async fn broker_recording_bodies() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let query = request.uri().query().unwrap_or_default().to_owned();
            let body = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .unwrap_or_default();
            recorder
                .lock()
                .expect("the hop log")
                .push(format!("{query} {}", String::from_utf8_lossy(&body)));
            axum::Json(everything())
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

/// One batch query, with the status, the answer and every request the broker saw.
async fn post_query(endpoint: Endpoint, body: Value) -> (StatusCode, Value, Vec<String>) {
    post_query_at(endpoint, "entityOperations/query", body).await
}

/// The same, at either of the two POST query forms (CIM 009 clauses 5.6.9 and 5.6.12).
async fn post_query_at(
    endpoint: Endpoint,
    operation: &str,
    body: Value,
) -> (StatusCode, Value, Vec<String>) {
    let (upstream, hops) = broker_recording_bodies().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/{operation}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let answered = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let asked = hops.lock().expect("the hop log").clone();
    (status, answered, asked)
}

/// R9, MP-02, T-2259: the selector of a batch query lives in its body (CIM 009 clause 5.6.9), so
/// the rule that takes a type out of a query it may not be filtered on reads the body too. Three
/// thresholds on a hidden attribute answer the same thing and the broker is never asked.
#[tokio::test]
async fn a_filter_in_a_batch_query_body_cannot_be_bisected() {
    let mut answers = BTreeSet::new();
    for threshold in ["1000", "6000", "20000"] {
        let (status, body, asked) = post_query(
            endpoint(true, &["secretPin"]),
            json!({
                "type": "Query",
                "entities": [{ "type": "User" }, { "type": "Vehicle" }],
                "q": format!("secretPin>{threshold}"),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            asked.is_empty(),
            "the broker was asked to filter on a hidden attribute: {asked:?}"
        );
        answers.insert(body.to_string());
    }
    assert_eq!(
        answers.len(),
        1,
        "three thresholds gave three different answers, which is the value itself: {answers:?}"
    );
}

/// AG-85, R9, T-2259: the temporal batch query is the same read with its selector in a body (CIM 009
/// clause 5.6.12), so it is narrowed by the same rule as the entity batch. Without it the history
/// of a hidden attribute could be bisected through the one POST form nobody narrowed.
#[tokio::test]
async fn a_filter_in_a_temporal_batch_query_body_cannot_be_bisected() {
    let mut answers = BTreeSet::new();
    for threshold in ["1000", "6000", "20000"] {
        let (status, body, asked) = post_query_at(
            endpoint(true, &["secretPin"]),
            "temporal/entityOperations/query",
            json!({
                "type": "Query",
                "entities": [{ "type": "User" }, { "type": "Vehicle" }],
                "q": format!("secretPin>{threshold}"),
                "temporalQ": { "timerel": "after", "timeAt": "2026-01-01T00:00:00Z" },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            asked.is_empty(),
            "the broker was asked for the history of a hidden attribute: {asked:?}"
        );
        answers.insert(body.to_string());
    }
    assert_eq!(
        answers.len(),
        1,
        "three thresholds gave three different answers, which is the value itself: {answers:?}"
    );
}

/// AG-85, MP-02: a type the filter may not select on leaves the body of the temporal batch before
/// the broker sees it, exactly as it leaves the entity batch's.
#[tokio::test]
async fn a_type_outside_the_projection_is_dropped_from_the_temporal_body() {
    let (status, body, asked) = post_query_at(
        endpoint(true, &[]),
        "temporal/entityOperations/query",
        json!({
            "type": "Query",
            "entities": [{ "type": "User" }, { "type": "Vehicle" }],
            "q": "age>30",
            "temporalQ": { "timerel": "after", "timeAt": "2026-01-01T00:00:00Z" },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hop = asked.first().expect("the broker was asked once").clone();
    assert!(
        !hop.contains("Vehicle"),
        "the broker was asked for the history of Vehicles by an attribute they may not serve: {hop}"
    );
}

/// MP-02: `age` belongs to `User` in this projection, so a body filtering on it is a query about
/// Users — the Vehicle leaves the selector before the broker sees it.
#[tokio::test]
async fn a_type_outside_the_projection_is_dropped_from_the_body() {
    let (status, body, asked) = post_query(
        endpoint(true, &[]),
        json!({
            "type": "Query",
            "entities": [{ "type": "User" }, { "type": "Vehicle" }],
            "q": "age>30",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hop = asked.first().expect("the broker was asked once").clone();
    assert!(
        !hop.contains("Vehicle"),
        "the broker was asked to filter Vehicles by an attribute they may not serve: {hop}"
    );
    assert_eq!(types(&body), vec!["User".to_owned()], "{body}");
}

/// R9, T-2259: `attrs` in the body is a selector as well as a projection — an entity carrying
/// none of the names is not returned — so a name this endpoint hides empties the query there
/// exactly as it does in a URL, without the broker being asked.
#[tokio::test]
async fn an_attrs_selector_in_the_body_that_names_a_hidden_attribute_asks_for_nothing() {
    let (status, body, asked) = post_query(
        endpoint(true, &["secretPin"]),
        json!({
            "type": "Query",
            "entities": [{ "type": "User" }],
            "attrs": ["name", "secretPin"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(
        asked.is_empty(),
        "the broker was asked for an attribute this endpoint hides: {asked:?}"
    );
}

/// MP-02, T-2259: a body whose filter belongs to one type and whose selector names another asks
/// for nothing — the filter keeps the types that may serve it, the selector keeps the types the
/// caller named, and there is no type in both.
#[tokio::test]
async fn a_body_whose_filter_and_selector_disagree_asks_for_nothing() {
    let (status, body, asked) = post_query(
        endpoint(true, &[]),
        json!({
            "type": "Query",
            "entities": [{ "type": "User" }],
            "q": "weight>100",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(
        asked.is_empty(),
        "the broker was asked for the one type the caller did not name: {asked:?}"
    );
}

/// T-1862 rule 4: an answer the filter rule narrowed says which types it left out, and why.
///
/// Every named type is one this caller may read — the rule took it out of *this* query, not out of
/// the grant — so naming it tells them nothing they could not read another way, and it is the
/// difference between an empty list a developer fixes and one they bisect until it answers.
#[tokio::test]
async fn an_answer_says_which_types_the_filter_left_out() {
    let (upstream, _) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .serve([endpoint(true, &[])]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=User,Vehicle&q=age%3E30"
                ))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    let warning = response
        .headers()
        .get("ngsild-warning")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        warning.contains("Vehicle") && warning.contains("not queried"),
        "the answer does not say the Vehicle was left out: {warning:?}"
    );
    assert!(
        !warning.contains("User"),
        "the type that was queried is named as left out: {warning}"
    );
}

/// The same, in the channel a model reads: a tool result carries the warning, because a header is
/// no use to somebody calling `query_entities` (AG-13).
#[tokio::test]
async fn a_tool_result_carries_the_same_warning() {
    let (upstream, _) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([Endpoint {
            roles: Default::default(),
            representations: vec![Representation::NgsiLd, Representation::Mcp],
            ..endpoint(true, &[])
        }]),
    );
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "query_entities",
            // One type per call is what the tool schema takes; `age` is a `User` slot, so the
            // `Vehicle` is the type this query may not select on and the answer is empty.
            "arguments": { "type": "Vehicle", "q": "age>30" }
        }
    });
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/mcp"))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(call.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let answer: Value = serde_json::from_slice(&bytes).expect("a JSON-RPC answer");
    let warnings = answer["result"]["structuredContent"]["warnings"].to_string();
    assert!(
        warnings.contains("Vehicle"),
        "the tool result does not say the Vehicle was left out: {answer}"
    );
}
