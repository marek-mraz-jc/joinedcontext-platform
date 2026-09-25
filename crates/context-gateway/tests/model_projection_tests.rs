//! T-0563: an endpoint that references a `ModelProjection` reads the projected subset of
//! the model and nothing wider (MP-02, MP-03).
//!
//! The grant says who may read; the projection says what the endpoint is about; the
//! gateway intersects the two before any representation. What is worth a test: an
//! attribute outside the projection never reaches the wire, a type outside it is refused
//! before the broker is asked, the residual filter is conjoined so a caller cannot widen
//! it, a grant narrower than the projection wins, and the schema surface describes the
//! projected model only.

mod common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::handlers::schema;
use context_gateway::pdp::evaluator::Subject;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const PROJECT: &str = "helsinki";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";

const PARTNER_VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: Vehicle
      slots: [name, speed]
    - name: User
      slots: [name, age]
  filter:
    q: category=="public"
"#;

fn projection() -> Arc<ModelProjectionSpec> {
    let parsed = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(PARTNER_VIEW).expect("parses");
    parsed.validate().expect("valid");
    Arc::new(parsed.spec)
}

fn vehicle() -> Value {
    json!({
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
        "type": "Vehicle",
        "name": { "type": "Property", "value": "Bus 01" },
        "speed": { "type": "Property", "value": 32 },
        "odometer": { "type": "Property", "value": 120_000 },
        "maintenanceNote": { "type": "Property", "value": "brake pads next week" }
    })
}

/// The same fleet, answered as the broker holds it: every entity carries every attribute of the
/// model, which is what makes a projection's per-type narrowing visible (T-1862).
fn mixed_fleet() -> Value {
    json!([
        {
            "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
            "type": "Vehicle",
            "name": { "type": "Property", "value": "Bus 01" },
            "speed": { "type": "Property", "value": 32 },
            "age": { "type": "Property", "value": 7 },
            "odometer": { "type": "Property", "value": 120_000 }
        },
        {
            "id": "urn:ngsi-ld:User:hel.fi:fleet:driver-01",
            "type": "User",
            "name": { "type": "Property", "value": "Aino" },
            "age": { "type": "Property", "value": 41 },
            "speed": { "type": "Property", "value": 3 },
            "odometer": { "type": "Property", "value": 5 }
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
            axum::Json(json!([vehicle()]))
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

async fn broker_answering(body: Value) -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let answer = Arc::new(body);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        let answer = Arc::clone(&answer);
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.uri().query().unwrap_or_default().to_owned());
            axum::Json((*answer).clone())
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

fn policy(information: &str) -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         {information}"
    ))
    .expect("the policy spec parses")
}

fn endpoint(policy: PolicySpec, projection: Option<Arc<ModelProjectionSpec>>) -> Endpoint {
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
        projection,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned(), "User".to_owned(), "Depot".to_owned()],
            json_schema: None,
            context: None,
        }],
        policies: vec![policy],
    }
}

async fn query(endpoint: Endpoint, uri: &str) -> (StatusCode, Value, Vec<String>) {
    let (upstream, hops) = broker().await;
    through(endpoint, uri, upstream, hops).await
}

/// The same, against a broker answering a body of this test's choosing.
async fn query_answering(
    endpoint: Endpoint,
    uri: &str,
    body: Value,
) -> (StatusCode, Value, Vec<String>) {
    let (upstream, hops) = broker_answering(body).await;
    through(endpoint, uri, upstream, hops).await
}

async fn through(
    endpoint: Endpoint,
    uri: &str,
    upstream: String,
    hops: Hops,
) -> (StatusCode, Value, Vec<String>) {
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

/// MP-02: the whole-entity grant stays a whole-entity grant on the space; on this endpoint
/// the projection keeps `name` and `speed` of a Vehicle and nothing else.
#[tokio::test]
async fn a_query_through_the_projection_returns_only_the_projected_attributes() {
    let (status, body, asked) = query(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entity = &body[0];
    assert_eq!(entity["name"]["value"], json!("Bus 01"));
    assert_eq!(entity["speed"]["value"], json!(32));
    assert!(
        entity.get("odometer").is_none(),
        "outside the projection: {entity}"
    );
    assert!(
        entity.get("maintenanceNote").is_none(),
        "outside the projection: {entity}"
    );
    assert_eq!(
        entity["type"],
        json!("Vehicle"),
        "identity is never projected away"
    );
    assert!(
        asked[0].contains("category"),
        "the residual filter reached the broker: {}",
        asked[0]
    );
}

/// A caller asking for an attribute the projection keeps gets the entity without the ones it
/// does not; asking for a type outside the projection is refused before the broker is asked.
///
/// `attrs` naming an attribute this type may not serve is a selector on it, and since T-1862 that
/// takes the type out of the query altogether rather than answering with the entities that have
/// it — see `filter_oracle_tests::attrs_is_a_selector_too`.
#[tokio::test]
async fn an_attribute_outside_is_dropped_and_a_type_outside_is_refused() {
    let (status, body, _) = query(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle&attrs=name,speed",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["name"]["value"], json!("Bus 01"));
    assert!(body[0].get("odometer").is_none());

    let (status, _, asked) = query(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities?type=Depot",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        asked.is_empty(),
        "the broker was never asked for a type the projection lacks"
    );
}

/// T-0805: an attribute no grant names is answered with nothing, never with the whole
/// entity, and the broker is not asked.
#[tokio::test]
async fn an_attribute_outside_the_grant_serves_nothing_and_never_the_whole_entity() {
    let named =
        policy("information:\n  - entities: [{ type: Vehicle }]\n    propertyNames: [name]\n");
    let (status, body, asked) = query(
        endpoint(named, None),
        "/ngsi-ld/v1/entities?type=Vehicle&attrs=maintenanceNote",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "nothing granted was asked for");
    assert!(asked.is_empty(), "the broker was never asked: {asked:?}");
}

/// T-0805 edge cases: an addressed read for an ungranted attribute is 404 and never a hop; a
/// grant over the whole entity is not narrowed by `attrs`, the caller's selection stands.
#[tokio::test]
async fn an_addressed_read_for_an_ungranted_attribute_is_not_found_and_a_whole_grant_stays() {
    let named =
        policy("information:\n  - entities: [{ type: Vehicle }]\n    propertyNames: [name]\n");
    let (status, _, asked) = query(
        endpoint(named, None),
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01?attrs=maintenanceNote",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(asked.is_empty(), "the broker was never asked: {asked:?}");

    let (status, body, asked) = query(
        endpoint(policy(""), None),
        "/ngsi-ld/v1/entities?type=Vehicle&attrs=maintenanceNote",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body[0]["maintenanceNote"]["value"],
        json!("brake pads next week")
    );
    assert!(asked[0].contains("attrs=maintenanceNote"), "{asked:?}");
}

/// T-0812: a policy whitelist that shares no slot with the projection leaves identity only;
/// it does not switch the projection off.
#[tokio::test]
async fn a_policy_whitelist_disjoint_from_the_projection_serves_identity_only() {
    let odometer =
        policy("information:\n  - entities: [{ type: Vehicle }]\n    propertyNames: [odometer]\n");
    let (status, body, _) = query(
        endpoint(odometer, Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entity = &body[0];
    assert_eq!(entity["type"], json!("Vehicle"));
    for name in ["name", "speed", "odometer", "maintenanceNote"] {
        assert!(entity.get(name).is_none(), "{name} served: {entity}");
    }
}

/// GW10: the projection's `q` is conjoined with the caller's, so a caller's own filter
/// cannot widen what the endpoint is about.
#[tokio::test]
async fn the_residual_filter_is_intersected_and_a_caller_cannot_widen_it() {
    // On a slot the projection publishes: a caller may filter on `speed`, and their filter is
    // conjoined with the endpoint's own rather than replacing it. (A caller filtering on
    // `category`, which this projection keeps for itself, is the oracle T-1862 closed: the type
    // leaves the query and the broker is never asked.)
    let (status, _, asked) = query(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle&q=speed%3E10",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sent = &asked[0];
    assert!(
        sent.contains("speed"),
        "the caller's filter is kept: {sent}"
    );
    assert!(
        sent.contains("public"),
        "the projection's filter is kept beside it: {sent}"
    );
}

/// A grant narrower than the projection wins: the projection cannot hand out `speed` when
/// the policy names only `name`.
#[tokio::test]
async fn a_policy_grant_narrower_than_the_projection_wins() {
    let narrower =
        policy("information:\n  - entities: [{ type: Vehicle }]\n    propertyNames: [name]\n");
    let (status, body, _) = query(
        endpoint(narrower, Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["name"]["value"], json!("Bus 01"));
    assert!(
        body[0].get("speed").is_none(),
        "the grant did not give speed: {body}"
    );
}

/// MP-03: the schema surface describes the projected classes and slots only, and the
/// document (so its ETag) changes when the projection does.
#[test]
fn the_schema_surface_serves_the_projected_model() {
    let now = context_gateway::pdp::now();
    let subject = Subject::anonymous();

    let unprojected = endpoint(policy(""), None);
    let visible = schema::visible(&subject, &unprojected, now);
    let types: BTreeSet<String> = schema::visible_types(&unprojected, &visible)
        .into_iter()
        .collect();
    assert!(types.contains("Depot"), "{types:?}");

    let projected = endpoint(policy(""), Some(projection()));
    let visible = schema::visible(&subject, &projected, now);
    let types: BTreeSet<String> = schema::visible_types(&projected, &visible)
        .into_iter()
        .collect();
    assert_eq!(
        types,
        BTreeSet::from(["Vehicle".to_owned(), "User".to_owned()]),
        "exactly the projected classes"
    );
    let index_before = schema::index(
        &unprojected,
        &schema::visible(&subject, &unprojected, now),
        |b| format!("{:x}", b.len()),
    );
    let index_after = schema::index(&projected, &visible, |b| format!("{:x}", b.len()));
    assert_ne!(
        index_before, index_after,
        "the projection changes what is published"
    );
}

/// MP-02, T-1862: a projection says which slots belong to which class, so two classes sharing an
/// endpoint do not share their slots.
///
/// The partner view gives `Vehicle` `[name, speed]` and `User` `[name, age]`. Asked for both, the
/// gateway can only tell the broker one `attrs` list — their union — and the answer used to be
/// stripped by that union, so a Vehicle that carries `age` was served with it. That is the whole
/// of the leak: the schema surface tells this caller a Vehicle has no `age` while the wire hands
/// them one.
#[tokio::test]
async fn one_types_slots_are_not_served_on_another() {
    let (status, body, asked) = query_answering(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle,User",
        mixed_fleet(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let entities = body.as_array().expect("a list of entities");
    assert_eq!(entities.len(), 2, "both types are granted: {body}");
    let vehicle = &entities[0];
    let user = &entities[1];

    assert_eq!(vehicle["type"], json!("Vehicle"));
    assert_eq!(vehicle["speed"]["value"], json!(32), "its own slot stays");
    assert_eq!(vehicle["name"]["value"], json!("Bus 01"));
    assert!(
        vehicle.get("age").is_none(),
        "`age` is a User's slot, not a Vehicle's: {vehicle}"
    );

    assert_eq!(user["type"], json!("User"));
    assert_eq!(user["age"]["value"], json!(41), "its own slot stays");
    assert!(
        user.get("speed").is_none(),
        "`speed` is a Vehicle's slot, not a User's: {user}"
    );

    // Neither keeps what the projection names for nobody.
    assert!(
        vehicle.get("odometer").is_none() && user.get("odometer").is_none(),
        "{body}"
    );
    // The broker is still asked for the union, because CIM 009 has one `attrs` list.
    assert!(asked[0].contains("attrs="), "{}", asked[0]);
}

/// The same rule on a single entity a retrieve addresses by id: a request that names no type
/// gets every granted type's slots joined, and the entity is cut by its own type all the same.
#[tokio::test]
async fn a_retrieve_that_names_no_type_is_cut_by_the_entity_s_own_type() {
    let (status, body, _) = query_answering(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities/urn:ngsi-ld:Vehicle:hel.fi:fleet:bus-01",
        mixed_fleet()[0].clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["speed"]["value"], json!(32));
    assert!(body.get("age").is_none(), "{body}");
}

/// An entity carrying two types keeps the union of the two classes' slots, and nothing else:
/// each of them is a type this caller may read it as.
#[tokio::test]
async fn an_entity_of_two_types_keeps_both_their_slots() {
    let both = json!([{
        "id": "urn:ngsi-ld:Vehicle:hel.fi:fleet:pool-car",
        "type": ["Vehicle", "User"],
        "name": { "type": "Property", "value": "Pool car" },
        "speed": { "type": "Property", "value": 12 },
        "age": { "type": "Property", "value": 3 },
        "odometer": { "type": "Property", "value": 9 }
    }]);
    let (status, body, _) = query_answering(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle,User",
        both,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entity = &body[0];
    assert_eq!(entity["speed"]["value"], json!(12));
    assert_eq!(entity["age"]["value"], json!(3));
    assert!(entity.get("odometer").is_none(), "{entity}");
}

/// A type the projection does not name keeps its identity and nothing else, however it reached
/// the answer: a broker that ignores `type`, a federated source, a registration answering for a
/// neighbour (T-2131 is the wider sweep; this is the projection's own half of it).
#[tokio::test]
async fn an_unprojected_type_in_the_answer_keeps_nothing_but_its_identity() {
    let depot = json!([{
        "id": "urn:ngsi-ld:Depot:hel.fi:fleet:north",
        "type": "Depot",
        "name": { "type": "Property", "value": "North depot" },
        "speed": { "type": "Property", "value": 0 }
    }]);
    let (status, body, _) = query_answering(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle,User",
        depot,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Dropped whole or cut to its identity — either is a truthful answer, and neither may carry
    // an attribute of a type this endpoint does not publish.
    let raw = body.to_string();
    assert!(
        !raw.contains("North depot") && !raw.contains("\"speed\""),
        "{body}"
    );
}

/// The other half of the residual filter: the projection keeps `category` for itself, so a caller
/// who filters on it is asking a question this endpoint does not answer (T-1862). The type leaves
/// the query, the broker is never asked, and the answer is the one an absent attribute gives.
#[tokio::test]
async fn a_caller_cannot_filter_on_the_projections_own_attribute() {
    let (status, body, asked) = query(
        endpoint(policy(""), Some(projection())),
        "/ngsi-ld/v1/entities?type=Vehicle&q=category%3D%3D%22internal%22",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}
