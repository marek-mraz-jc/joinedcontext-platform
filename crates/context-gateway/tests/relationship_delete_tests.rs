//! T-2858: the relationship rules the broker does not hold, run by the gateway (DM-70, DM-71).
//!
//! The owner decided on 2026-09-25 that relationship integrity stays out of the broker, so the
//! gateway reads the space and then writes: a one-to-one target another source stores is
//! refused (`target-taken`), and a delete runs the rule of every relationship that points at
//! what it deletes: `restrict` answers 409 with the count, `cascade` and `set-null` change the
//! pointing entities first and the entity itself last. The broker here is a small in-memory
//! store that answers the computed end's query (`type` + `q={slot}=="{urn}"`) and applies the
//! writes, so each test reads the store after the request.

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
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k7vbn3tdw6hcy5pajfr4m8qz2x";
const SPACE: &str = "skoly";
const DOMAIN: &str = "banskabystrica.sk";
const BAD_REQUEST_DATA: &str = "https://uri.etsi.org/ngsi-ld/errors/BadRequestData";

fn urn(kind: &str, local: &str) -> String {
    format!("urn:ngsi-ld:{kind}:{DOMAIN}:{SPACE}:{local}")
}

/// What the in-memory broker holds, and every call it was made.
#[derive(Default)]
struct Store {
    entities: BTreeMap<String, Value>,
    calls: Vec<String>,
    /// Writes to these paths are refused with 500, to stop a delete part way.
    failing: BTreeSet<String>,
}

type Shared = Arc<Mutex<Store>>;

/// The targets one attribute value names, in the forms the store holds.
fn objects(value: &Value) -> Vec<String> {
    match value {
        Value::Array(instances) => instances.iter().flat_map(objects).collect(),
        Value::Object(member) => match member.get("object") {
            Some(Value::String(one)) => vec![one.clone()],
            Some(Value::Array(many)) => many
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

async fn serve(State(store): State<Shared>, request: Request) -> axum::response::Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let mut store = store.lock().expect("the store");
    store.calls.push(format!("{method} {path}?{query}"));
    let pairs: Vec<(String, String)> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect();
    let param = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    if method != Method::GET && store.failing.contains(&decode(&path)) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let segments: Vec<String> = path
        .trim_start_matches("/ngsi-ld/v1/")
        .split('/')
        .map(decode)
        .collect();
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (method.clone(), segments.as_slice()) {
        (Method::GET, ["entities"]) => {
            let ids: Vec<String> = param("id")
                .map(|ids| ids.split(',').map(str::to_owned).collect())
                .unwrap_or_default();
            let kinds: Vec<String> = param("type")
                .map(|kinds| kinds.split(',').map(str::to_owned).collect())
                .unwrap_or_default();
            // `q={slot}=="{urn}"`: the computed end's query.
            let pointing = param("q").and_then(|q| {
                let (slot, object) = q.split_once("==")?;
                Some((slot.to_owned(), object.trim_matches('"').to_owned()))
            });
            let limit: usize = param("limit").and_then(|l| l.parse().ok()).unwrap_or(20);
            let offset: usize = param("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
            let found: Vec<Value> = store
                .entities
                .values()
                .filter(|entity| ids.is_empty() || ids.iter().any(|id| entity["id"] == id.as_str()))
                .filter(|entity| {
                    kinds.is_empty() || kinds.iter().any(|kind| entity["type"] == kind.as_str())
                })
                .filter(|entity| match &pointing {
                    Some((slot, object)) => entity
                        .get(slot)
                        .is_some_and(|value| objects(value).contains(object)),
                    None => true,
                })
                .skip(offset)
                .take(limit)
                .cloned()
                .collect();
            axum::Json(Value::Array(found)).into_response()
        }
        (Method::POST, ["entities"]) => {
            let entity: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let id = entity["id"].as_str().unwrap_or_default().to_owned();
            store.entities.insert(id, entity);
            StatusCode::CREATED.into_response()
        }
        (Method::POST, ["entityOperations", "create" | "upsert"]) => {
            let batch: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let mut ids = Vec::new();
            for entity in batch.as_array().into_iter().flatten() {
                let id = entity["id"].as_str().unwrap_or_default().to_owned();
                ids.push(Value::String(id.clone()));
                store.entities.insert(id, entity.clone());
            }
            (StatusCode::CREATED, axum::Json(Value::Array(ids))).into_response()
        }
        (Method::POST, ["entityOperations", "delete"]) => {
            let batch: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            for id in batch
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                store.entities.remove(id);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        (Method::DELETE, ["entities", id]) => match store.entities.remove(*id) {
            Some(_) => StatusCode::NO_CONTENT.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        (Method::DELETE, ["entities", id, "attrs", slot]) => {
            let dataset = param("datasetId");
            let Some(entity) = store.entities.get_mut(*id) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let Some(object) = entity.as_object_mut() else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let removed = match (dataset, object.get_mut(*slot)) {
                (None, Some(_)) => object.remove(*slot).is_some(),
                (Some(dataset), Some(Value::Array(instances))) => {
                    let before = instances.len();
                    instances.retain(|instance| instance["datasetId"] != dataset.as_str());
                    before != instances.len()
                }
                _ => false,
            };
            if removed {
                StatusCode::NO_CONTENT.into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
        (Method::PATCH, ["entities", id, "attrs", slot]) => {
            let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let Some(entity) = store.entities.get_mut(*id) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            entity[*slot] = value;
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn broker(held: Vec<Value>, failing: &[&str]) -> (String, Shared) {
    let store: Shared = Arc::new(Mutex::new(Store {
        entities: held
            .into_iter()
            .map(|entity| (entity["id"].as_str().unwrap_or_default().to_owned(), entity))
            .collect(),
        calls: Vec::new(),
        failing: failing.iter().map(|path| (*path).to_owned()).collect(),
    }));
    let app = Router::new()
        .fallback(any(serve))
        .with_state(Arc::clone(&store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), store)
}

/// A School sits in a Building (restrict); a Teacher has one Desk (one-to-one, set-null), works
/// at a School (required, cascade) and teaches Courses (many-to-many, cascade); a Lesson is
/// given by a Teacher (cascade). Deleting a School reaches Lessons two levels down.
fn rules() -> RelationshipRules {
    let end = |target: &str, cardinality: &str, on_delete: &str| {
        json!({ "target": target, "inverse": "x", "cardinality": cardinality,
                "onDelete": on_delete, "unique": cardinality == "one-to-one" })
    };
    let single = |target: &str, cardinality: &str, on_delete: &str| {
        json!({ "type": "string", "x-ngsi-ld-kind": "Relationship",
                "x-ngsi-ld-relationship": end(target, cardinality, on_delete) })
    };
    RelationshipRules::from_schema(&json!({ "definitions": {
        "School": { "properties": {
            "building": single("Building", "many-to-one", "restrict"),
            "name": { "type": "string" } } },
        "Teacher": {
            "required": ["school"],
            "properties": {
                "desk": single("Desk", "one-to-one", "set-null"),
                "school": single("School", "many-to-one", "cascade"),
                "courses": { "type": "array", "items": { "type": "string" },
                    "x-ngsi-ld-kind": "Relationship",
                    "x-ngsi-ld-relationship": end("Course", "many-to-many", "cascade") },
                "name": { "type": "string" } } },
        "Lesson": { "properties": {
            "teacher": single("Teacher", "many-to-one", "cascade"),
            "name": { "type": "string" } } },
        "Building": { "properties": { "name": { "type": "string" } } },
        "Desk": { "properties": { "name": { "type": "string" } } },
        "Course": { "properties": { "name": { "type": "string" } } }
    } }))
}

const CLASSES: &[&str] = &["School", "Teacher", "Lesson", "Building", "Desk", "Course"];

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

/// The caller writes and deletes the `writable` classes and reads every class.
fn endpoint(writable: &[&str]) -> Endpoint {
    Endpoint {
        declared_types: Some(DeclaredTypes {
            model: "bb-skoly".into(),
            classes: CLASSES.iter().map(|class| (*class).to_owned()).collect(),
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
                "createEntity, createBatch, upsertBatch, appendAttrs, updateAttrs, deleteAttrs, \
                 deleteEntity, deleteBatch",
                writable,
            ),
            policy("queryEntity, retrieveEntity", CLASSES),
        ],
    }
}

fn link(object: &str) -> Value {
    json!({ "type": "Relationship", "object": object })
}

fn entity(kind: &str, local: &str, links: &[(&str, Value)]) -> Value {
    let mut entity = json!({ "id": urn(kind, local), "type": kind,
        "name": { "type": "Property", "value": local } });
    for (slot, value) in links {
        entity[*slot] = value.clone();
    }
    entity
}

/// Two schools in one building, a teacher at each with a desk and courses, two lessons of the
/// first teacher.
fn campus() -> Vec<Value> {
    let course = |local: &str, dataset: &str| {
        json!({ "type": "Relationship", "object": urn("Course", local),
                "datasetId": format!("urn:ngsi-ld:Dataset:{dataset}") })
    };
    vec![
        entity("Building", "b1", &[]),
        entity(
            "School",
            "s1",
            &[("building", link(&urn("Building", "b1")))],
        ),
        entity(
            "School",
            "s2",
            &[("building", link(&urn("Building", "b1")))],
        ),
        entity("Desk", "d1", &[]),
        entity("Desk", "d2", &[]),
        entity("Course", "c1", &[]),
        entity("Course", "c2", &[]),
        entity(
            "Teacher",
            "t1",
            &[
                ("school", link(&urn("School", "s1"))),
                ("desk", link(&urn("Desk", "d1"))),
                ("courses", json!([course("c1", "c1"), course("c2", "c2")])),
            ],
        ),
        entity("Teacher", "t2", &[("school", link(&urn("School", "s2")))]),
        entity("Lesson", "l1", &[("teacher", link(&urn("Teacher", "t1")))]),
        entity("Lesson", "l2", &[("teacher", link(&urn("Teacher", "t1")))]),
    ]
}

struct Answer {
    status: StatusCode,
    body: Value,
    store: Shared,
}

impl Answer {
    fn held(&self, id: &str) -> Option<Value> {
        self.store
            .lock()
            .expect("the store")
            .entities
            .get(id)
            .cloned()
    }

    fn writes(&self) -> Vec<String> {
        self.store
            .lock()
            .expect("the store")
            .calls
            .iter()
            .filter(|call| !call.starts_with("GET "))
            .cloned()
            .collect()
    }
}

async fn send(
    writable: &[&str],
    failing: &[&str],
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> Answer {
    let (upstream, store) = broker(campus(), failing).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .serve([endpoint(writable)]),
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
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        store,
    }
}

async fn delete(writable: &[&str], id: &str) -> Answer {
    send(
        writable,
        &[],
        Method::DELETE,
        &format!("/entities/{id}"),
        None,
    )
    .await
}

// --- target-taken (DM-70) ---------------------------------------------------------------------

#[tokio::test]
async fn a_desk_another_teacher_has_is_refused_and_nothing_is_written() {
    let teacher = entity(
        "Teacher",
        "t3",
        &[
            ("school", link(&urn("School", "s2"))),
            ("desk", link(&urn("Desk", "d1"))),
        ],
    );
    let answer = send(CLASSES, &[], Method::POST, "/entities", Some(teacher)).await;

    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.body);
    assert_eq!(answer.body["type"], BAD_REQUEST_DATA);
    assert_eq!(answer.body["rule"], "target-taken");
    assert_eq!(answer.body["slot"], "desk");
    assert_eq!(answer.body["object"], urn("Desk", "d1"));
    let detail = answer.body["detail"].as_str().unwrap_or_default();
    assert!(
        !detail.contains(&urn("Teacher", "t1")),
        "the refusal names the entity that holds the desk: {detail}"
    );
    assert!(answer.writes().is_empty(), "{:?}", answer.writes());
}

#[tokio::test]
async fn a_teacher_rewriting_its_own_desk_and_a_free_desk_are_written() {
    let own = entity(
        "Teacher",
        "t1",
        &[
            ("school", link(&urn("School", "s1"))),
            ("desk", link(&urn("Desk", "d1"))),
        ],
    );
    let answer = send(
        CLASSES,
        &[],
        Method::POST,
        "/entityOperations/upsert",
        Some(json!([own])),
    )
    .await;
    assert!(answer.status.is_success(), "{}", answer.body);

    let free = entity(
        "Teacher",
        "t3",
        &[
            ("school", link(&urn("School", "s2"))),
            ("desk", link(&urn("Desk", "d2"))),
        ],
    );
    let answer = send(CLASSES, &[], Method::POST, "/entities", Some(free)).await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
}

#[tokio::test]
async fn two_teachers_of_one_batch_taking_one_desk_divide_the_batch() {
    let teacher = |local: &str| {
        entity(
            "Teacher",
            local,
            &[
                ("school", link(&urn("School", "s2"))),
                ("desk", link(&urn("Desk", "d2"))),
            ],
        )
    };
    let answer = send(
        CLASSES,
        &[],
        Method::POST,
        "/entityOperations/create",
        Some(json!([teacher("t3"), teacher("t4")])),
    )
    .await;

    assert_eq!(answer.status, StatusCode::MULTI_STATUS, "{}", answer.body);
    assert_eq!(answer.body["success"], json!([urn("Teacher", "t3")]));
    let errors = answer.body["errors"].as_array().expect("the errors");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["entityId"], urn("Teacher", "t4"));
    assert_eq!(errors[0]["error"]["rule"], "target-taken");
    assert!(answer.held(&urn("Teacher", "t4")).is_none());
}

// --- delete rules (DM-71) ---------------------------------------------------------------------

#[tokio::test]
async fn restrict_refuses_with_the_count_and_the_readable_ids_and_changes_nothing() {
    let answer = delete(CLASSES, &urn("Building", "b1")).await;

    assert_eq!(answer.status, StatusCode::CONFLICT, "{}", answer.body);
    assert_eq!(answer.body["rule"], "restrict");
    assert_eq!(answer.body["slot"], "building");
    assert_eq!(answer.body["count"], 2);
    assert_eq!(
        answer.body["ids"],
        json!([urn("School", "s1"), urn("School", "s2")])
    );
    assert!(answer.writes().is_empty(), "{:?}", answer.writes());
    assert!(answer.held(&urn("Building", "b1")).is_some());
}

#[tokio::test]
async fn a_cascade_two_levels_deep_deletes_from_the_farthest_in() {
    let answer = delete(CLASSES, &urn("School", "s1")).await;

    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    for gone in [
        urn("School", "s1"),
        urn("Teacher", "t1"),
        urn("Lesson", "l1"),
        urn("Lesson", "l2"),
    ] {
        assert!(answer.held(&gone).is_none(), "{gone} is still held");
    }
    for kept in [urn("School", "s2"), urn("Teacher", "t2"), urn("Desk", "d1")] {
        assert!(answer.held(&kept).is_some(), "{kept} went too");
    }
    // The lessons go before their teacher, and the teacher before the school: a stop part way
    // leaves nothing pointing at an entity already gone.
    let deletes: Vec<String> = answer
        .writes()
        .into_iter()
        .filter(|call| call.starts_with("DELETE"))
        .map(|call| decode(&call))
        .collect();
    let at = |local: &str| {
        deletes
            .iter()
            .position(|call| call.contains(local))
            .unwrap_or_else(|| panic!("no delete of {local}: {deletes:?}"))
    };
    assert!(
        at(":l1") < at(":t1") && at(":l2") < at(":t1"),
        "{deletes:?}"
    );
    assert!(at(":t1") < at(":s1"), "{deletes:?}");
}

#[tokio::test]
async fn set_null_removes_the_one_to_one_end_and_keeps_the_teacher() {
    let answer = delete(CLASSES, &urn("Desk", "d1")).await;

    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert!(answer.held(&urn("Desk", "d1")).is_none());
    let teacher = answer
        .held(&urn("Teacher", "t1"))
        .expect("the teacher stays");
    assert!(teacher.get("desk").is_none(), "{teacher}");
    assert!(teacher.get("school").is_some(), "{teacher}");
}

#[tokio::test]
async fn a_many_to_many_cascade_removes_only_the_one_link() {
    let answer = delete(CLASSES, &urn("Course", "c1")).await;

    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    let teacher = answer
        .held(&urn("Teacher", "t1"))
        .expect("the teacher stays");
    let courses: Vec<&str> = teacher["courses"]
        .as_array()
        .expect("the other course is kept")
        .iter()
        .filter_map(|course| course["object"].as_str())
        .collect();
    assert_eq!(courses, vec![urn("Course", "c2")]);
    assert!(answer.held(&urn("Course", "c2")).is_some());
}

#[tokio::test]
async fn a_cascade_into_entities_the_caller_may_not_delete_is_refused_without_ids() {
    // The caller deletes Schools and Teachers, not Lessons.
    let answer = delete(&["School", "Teacher"], &urn("School", "s1")).await;

    assert_eq!(answer.status, StatusCode::CONFLICT, "{}", answer.body);
    assert_eq!(answer.body["rule"], "restrict");
    assert_eq!(answer.body["slot"], "teacher");
    assert_eq!(answer.body["count"], 2);
    assert_eq!(answer.body["ids"], json!([]));
    assert!(answer.writes().is_empty(), "{:?}", answer.writes());
}

#[tokio::test]
async fn a_batch_delete_refuses_the_kept_id_and_deletes_the_rest() {
    let answer = send(
        CLASSES,
        &[],
        Method::POST,
        "/entityOperations/delete",
        Some(json!([urn("Building", "b1"), urn("Desk", "d1")])),
    )
    .await;

    assert_eq!(answer.status, StatusCode::MULTI_STATUS, "{}", answer.body);
    let errors = answer.body["errors"].as_array().expect("the errors");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["entityId"], urn("Building", "b1"));
    assert_eq!(errors[0]["error"]["rule"], "restrict");
    assert!(answer.held(&urn("Building", "b1")).is_some());
    assert!(answer.held(&urn("Desk", "d1")).is_none());
    let teacher = answer
        .held(&urn("Teacher", "t1"))
        .expect("the teacher stays");
    assert!(
        teacher.get("desk").is_none(),
        "set-null ran for the kept part"
    );
}

#[tokio::test]
async fn a_change_the_broker_refuses_stops_the_delete_with_the_entity_in_place() {
    // The lessons go first, l2 then l1; l1 is refused, so the teacher and the school are never
    // touched and the answer counts the one change already made.
    let l1 = format!("/ngsi-ld/v1/entities/{}", urn("Lesson", "l1"));
    let answer = send(
        CLASSES,
        &[l1.as_str()],
        Method::DELETE,
        &format!("/entities/{}", urn("School", "s1")),
        None,
    )
    .await;

    assert_eq!(answer.status, StatusCode::BAD_GATEWAY, "{}", answer.body);
    assert_eq!(answer.body["changed"], 1, "{}", answer.body);
    let detail = answer.body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("is not deleted"), "{detail}");
    assert!(answer.held(&urn("School", "s1")).is_some());
    assert!(answer.held(&urn("Teacher", "t1")).is_some());
    assert!(answer.held(&urn("Lesson", "l1")).is_some());
    assert!(answer.held(&urn("Lesson", "l2")).is_none());
}

#[tokio::test]
async fn a_delete_nothing_points_at_goes_straight_to_the_broker() {
    let answer = delete(CLASSES, &urn("Lesson", "l1")).await;

    assert_eq!(answer.status, StatusCode::NO_CONTENT, "{}", answer.body);
    assert_eq!(
        answer.writes(),
        vec![format!(
            "DELETE /ngsi-ld/v1/entities/{}?",
            urn("Lesson", "l1")
        )]
    );
}
