//! T-2740: a write through an Endpoint keeps the relationships of the space's model (DM-70).
//!
//! The model's generated JSON Schema states each stored end (`x-ngsi-ld-relationship`). A write
//! that names a target of the wrong type, gives a single end several targets, leaves a required
//! end empty or names a target the writer cannot read in the space is a 400 `BadRequestData`
//! with `slot`, `rule` and `object`, before the broker is asked to write. A batch is answered
//! per entity (207). A target the writer may not read is answered as missing, word for word
//! (DM-71).

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::query::decode;
use context_gateway::relationships::RelationshipRules;
use context_gateway::resolver::{DeclaredTypes, Endpoint};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "r4m8qz2xk7vbn3tdw6hcy5pajf";
const SPACE: &str = "skoly";
const DOMAIN: &str = "banskabystrica.sk";
const SCHOOL: &str = "urn:ngsi-ld:School:banskabystrica.sk:skoly:s1";
const OTHER_SCHOOL: &str = "urn:ngsi-ld:School:banskabystrica.sk:skoly:s2";
const NO_SCHOOL: &str = "urn:ngsi-ld:School:banskabystrica.sk:skoly:s9";
const COURSE: &str = "urn:ngsi-ld:Course:banskabystrica.sk:skoly:c1";
const OTHER_COURSE: &str = "urn:ngsi-ld:Course:banskabystrica.sk:skoly:c2";
const BAD_REQUEST_DATA: &str = "https://uri.etsi.org/ngsi-ld/errors/BadRequestData";

type Calls = Arc<Mutex<Vec<String>>>;

/// A broker holding two schools and two courses: it answers a read by id and takes every write.
async fn broker() -> (String, Calls) {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let held: Vec<Value> = [SCHOOL, OTHER_SCHOOL, COURSE, OTHER_COURSE]
        .iter()
        .map(|id| {
            let kind = id.split(':').nth(2).unwrap_or_default();
            json!({ "id": id, "type": kind })
        })
        .collect();
    let app = Router::new()
        .fallback(any(
            |State((calls, held)): State<(Calls, Arc<Vec<Value>>)>, request: Request| async move {
                let method = request.method().clone();
                let query = request.uri().query().unwrap_or_default().to_owned();
                calls
                    .lock()
                    .expect("the call log")
                    .push(format!("{method} {}?{query}", request.uri().path()));
                if method != Method::GET {
                    return StatusCode::NO_CONTENT.into_response();
                }
                let pairs: Vec<(String, String)> = query
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(key, value)| (decode(key), decode(value)))
                    .collect();
                let wanted = |name: &str| -> Vec<String> {
                    pairs
                        .iter()
                        .filter(|(key, _)| key == name)
                        .flat_map(|(_, value)| value.split(',').map(str::to_owned))
                        .collect()
                };
                let (ids, types) = (wanted("id"), wanted("type"));
                let found: Vec<Value> = held
                    .iter()
                    .filter(|entity| ids.iter().any(|id| entity["id"] == id.as_str()))
                    .filter(|entity| {
                        types.is_empty() || types.iter().any(|kind| entity["type"] == kind.as_str())
                    })
                    .cloned()
                    .collect();
                axum::Json(Value::Array(found)).into_response()
            },
        ))
        .with_state((Arc::clone(&calls), Arc::new(held)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), calls)
}

/// A User belongs to exactly one School (required) and takes any number of Courses.
fn rules() -> RelationshipRules {
    let end = |target: &str, cardinality: &str| {
        json!({ "target": target, "inverse": "users", "cardinality": cardinality,
                "onDelete": "restrict", "unique": false })
    };
    RelationshipRules::from_schema(&json!({ "definitions": {
        "User": {
            "required": ["school"],
            "properties": {
                "school": { "type": "string", "x-ngsi-ld-kind": "Relationship",
                    "x-ngsi-ld-relationship": end("School", "many-to-one") },
                "courses": { "type": "array", "items": { "type": "string" },
                    "x-ngsi-ld-kind": "Relationship",
                    "x-ngsi-ld-relationship": end("Course", "many-to-many") },
                "name": { "type": "string" }
            }
        },
        "School": { "properties": { "name": { "type": "string" } } },
        "Course": { "properties": { "name": { "type": "string" } } }
    } }))
}

fn policy(operations: &str, types: &[&str]) -> PolicySpec {
    let entities: String = types
        .iter()
        .map(|kind| format!("\x20     - type: {kind}\n"))
        .collect();
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [{operations}]\n\
         information:\n\
         \x20 - entities:\n{entities}"
    ))
    .expect("the policy spec parses")
}

/// The writer writes Users and reads the `readable` classes.
fn endpoint(readable: &[&str]) -> Endpoint {
    Endpoint {
        declared_types: Some(DeclaredTypes {
            model: "bb-skoly".into(),
            classes: ["User", "School", "Course"]
                .iter()
                .map(|class| (*class).to_owned())
                .collect(),
            units: Default::default(),
            relationships: rules(),
        }),
        catalog: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: SPACE.to_owned(),
        project: SPACE.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policies: vec![
            policy(
                "createEntity, upsertBatch, appendAttrs, deleteAttrs",
                &["User"],
            ),
            policy("queryEntity, retrieveEntity", readable),
        ],
    }
}

fn user(local: &str, school: Option<Value>, courses: &[&str]) -> Value {
    let mut user = json!({
        "id": format!("urn:ngsi-ld:User:{DOMAIN}:{SPACE}:{local}"),
        "type": "User",
        "name": { "type": "Property", "value": local }
    });
    if let Some(school) = school {
        user["school"] = school;
    }
    if !courses.is_empty() {
        user["courses"] = json!({ "type": "Relationship", "object": courses });
    }
    user
}

fn at(urn: &str) -> Option<Value> {
    Some(json!({ "type": "Relationship", "object": urn }))
}

struct Answer {
    status: StatusCode,
    body: Value,
    calls: Vec<String>,
}

impl Answer {
    fn writes(&self) -> Vec<&String> {
        self.calls
            .iter()
            .filter(|call| !call.starts_with("GET "))
            .collect()
    }
}

async fn send(readable: &[&str], method: Method, uri: &str, body: Option<Value>) -> Answer {
    let (upstream, calls) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .serve([endpoint(readable)]),
    );
    let request = Request::builder()
        .method(method)
        .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{uri}"))
        .header("content-type", "application/ld+json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .expect("a request");
    let response = router(gateway)
        .oneshot(request)
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let calls = calls.lock().expect("the call log").clone();
    Answer {
        status,
        body,
        calls,
    }
}

const READS_ALL: &[&str] = &["User", "School", "Course"];

async fn create(readable: &[&str], entity: Value) -> Answer {
    send(readable, Method::POST, "/entities", Some(entity)).await
}

fn assert_refused(answer: &Answer, rule: &str, slot: &str, object: Option<&str>) {
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.body);
    assert_eq!(answer.body["type"], BAD_REQUEST_DATA, "{}", answer.body);
    assert_eq!(answer.body["rule"], rule, "{}", answer.body);
    assert_eq!(answer.body["slot"], slot, "{}", answer.body);
    assert_eq!(
        answer.body.get("object").and_then(Value::as_str),
        object,
        "{}",
        answer.body
    );
    assert!(
        answer.writes().is_empty(),
        "the broker was asked to write: {:?}",
        answer.calls
    );
}

#[tokio::test]
async fn a_user_at_a_school_on_two_courses_is_written() {
    let answer = create(READS_ALL, user("u1", at(SCHOOL), &[COURSE, OTHER_COURSE])).await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert_eq!(answer.writes().len(), 1, "{:?}", answer.calls);
}

#[tokio::test]
async fn a_target_of_another_type_is_refused_naming_it() {
    let answer = create(READS_ALL, user("u1", at(COURSE), &[])).await;
    assert_refused(&answer, "target-wrong-type", "school", Some(COURSE));
    assert!(
        answer.calls.is_empty(),
        "nothing is read: {:?}",
        answer.calls
    );
}

#[tokio::test]
async fn a_single_end_given_two_targets_is_refused() {
    let two = Some(json!({ "type": "Relationship", "object": [SCHOOL, OTHER_SCHOOL] }));
    let answer = create(READS_ALL, user("u1", two, &[])).await;
    assert_refused(
        &answer,
        "single-end-many-targets",
        "school",
        Some(OTHER_SCHOOL),
    );
}

#[tokio::test]
async fn a_required_end_left_out_of_a_new_entity_is_refused() {
    let answer = create(READS_ALL, user("u1", None, &[COURSE])).await;
    assert_refused(&answer, "required-end-missing", "school", None);
}

#[tokio::test]
async fn a_required_end_emptied_by_a_partial_write_is_refused() {
    let id = format!("urn:ngsi-ld:User:{DOMAIN}:{SPACE}:u1");
    let answer = send(
        READS_ALL,
        Method::POST,
        &format!("/entities/{id}/attrs"),
        Some(json!({ "school": { "type": "Relationship", "object": "urn:ngsi-ld:null" } })),
    )
    .await;
    assert_refused(&answer, "required-end-missing", "school", None);
}

#[tokio::test]
async fn a_partial_write_may_leave_a_required_end_as_it_is() {
    let id = format!("urn:ngsi-ld:User:{DOMAIN}:{SPACE}:u1");
    let answer = send(
        READS_ALL,
        Method::POST,
        &format!("/entities/{id}/attrs"),
        Some(json!({ "courses": { "type": "Relationship", "object": [COURSE] } })),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert_eq!(answer.writes().len(), 1, "{:?}", answer.calls);
}

#[tokio::test]
async fn deleting_a_required_end_is_refused_and_an_optional_one_is_not() {
    let id = format!("urn:ngsi-ld:User:{DOMAIN}:{SPACE}:u1");
    let required = send(
        READS_ALL,
        Method::DELETE,
        &format!("/entities/{id}/attrs/school"),
        None,
    )
    .await;
    assert_refused(&required, "required-end-missing", "school", None);

    let optional = send(
        READS_ALL,
        Method::DELETE,
        &format!("/entities/{id}/attrs/courses"),
        None,
    )
    .await;
    assert_eq!(optional.status, StatusCode::NO_CONTENT, "{}", optional.body);
    assert_eq!(optional.writes().len(), 1, "{:?}", optional.calls);
}

#[tokio::test]
async fn a_target_the_space_does_not_hold_is_refused_naming_it() {
    let answer = create(READS_ALL, user("u1", at(NO_SCHOOL), &[])).await;
    assert_refused(&answer, "target-missing", "school", Some(NO_SCHOOL));
    assert_eq!(
        answer.calls.len(),
        1,
        "one existence read: {:?}",
        answer.calls
    );
}

#[tokio::test]
async fn a_target_the_writer_may_not_read_is_answered_as_one_that_is_not_there() {
    let hidden = create(&["User", "Course"], user("u1", at(SCHOOL), &[])).await;
    let absent = create(&["User", "Course"], user("u1", at(NO_SCHOOL), &[])).await;
    assert_refused(&hidden, "target-missing", "school", Some(SCHOOL));
    assert_refused(&absent, "target-missing", "school", Some(NO_SCHOOL));
    let mut unmasked = hidden.body.clone();
    unmasked["object"] = json!(NO_SCHOOL);
    unmasked["detail"] = json!(hidden.body["detail"]
        .as_str()
        .unwrap_or_default()
        .replace(SCHOOL, NO_SCHOOL));
    assert_eq!(unmasked, absent.body, "the refusal tells the two apart");
}

#[tokio::test]
async fn a_batch_writes_the_valid_entities_and_answers_the_others_per_entity() {
    let answer = send(
        READS_ALL,
        Method::POST,
        "/entityOperations/upsert",
        Some(json!([
            user("ok", at(SCHOOL), &[COURSE]),
            user("gone", at(NO_SCHOOL), &[]),
            user("wrong", at(COURSE), &[]),
            user("none", None, &[])
        ])),
    )
    .await;
    assert_eq!(answer.status, StatusCode::MULTI_STATUS, "{}", answer.body);
    let ok = format!("urn:ngsi-ld:User:{DOMAIN}:{SPACE}:ok");
    assert_eq!(answer.body["success"], json!([ok]), "{}", answer.body);
    let errors = answer.body["errors"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let rule_of = |local: &str| {
        let id = format!("urn:ngsi-ld:User:{DOMAIN}:{SPACE}:{local}");
        errors
            .iter()
            .find(|error| error["entityId"] == id.as_str())
            .map(|error| error["error"]["rule"].clone())
    };
    assert_eq!(rule_of("gone"), Some(json!("target-missing")), "{errors:?}");
    assert_eq!(
        rule_of("wrong"),
        Some(json!("target-wrong-type")),
        "{errors:?}"
    );
    assert_eq!(
        rule_of("none"),
        Some(json!("required-end-missing")),
        "{errors:?}"
    );
    assert_eq!(answer.writes().len(), 1, "{:?}", answer.calls);
}

#[tokio::test]
async fn a_batch_where_every_entity_breaks_a_relationship_writes_nothing() {
    let answer = send(
        READS_ALL,
        Method::POST,
        "/entityOperations/upsert",
        Some(json!([user("gone", at(NO_SCHOOL), &[])])),
    )
    .await;
    assert_eq!(answer.status, StatusCode::MULTI_STATUS, "{}", answer.body);
    assert_eq!(answer.body["success"], json!([]), "{}", answer.body);
    assert!(answer.writes().is_empty(), "{:?}", answer.calls);
}

#[tokio::test]
async fn an_upsert_that_updates_may_leave_a_required_end_out() {
    let answer = send(
        READS_ALL,
        Method::POST,
        "/entityOperations/upsert?options=update",
        Some(json!([user("u1", None, &[COURSE])])),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert_eq!(answer.writes().len(), 1, "{:?}", answer.calls);
}
