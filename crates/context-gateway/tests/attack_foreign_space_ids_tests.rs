//! Attack vector: a tenant header or an entity id of another space (T-1697; EP-01, PF-84, GW16).
//!
//! `tenancy_middleware_tests.rs` and `edge_gateway_tenancy_strip_tests.rs` prove the header half
//! on a read: whatever the caller spells `NGSILD-Tenant`, the broker is told the space the
//! endpoint pins. `write_guard_tests.rs` proves the identifier half as a function: an id whose
//! `{orgDomain}:{space}` segments are not this endpoint's is refused.
//!
//! What neither plays is the attack itself — the foreign id sent through each write route of a
//! live router, where the thing to prove is not only the status but that the broker was never
//! asked. A write refused after the hop is a write that happened. So every case here asserts the
//! refusal, its reason, and an empty hop log; the two control cases assert that the same routes
//! carry an id of this space through, so the file fails if the surface simply stopped writing.

mod common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";

/// An id of this endpoint's own space, which every control case writes.
const OURS: &str = "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7";

/// The same local id in another space of the same organization.
const OTHER_SPACE: &str = "urn:ngsi-ld:Device:banskabystrica.sk:doprava:lamps-7";

/// The same local id in the same-named space of another organization.
const OTHER_ORG: &str = "urn:ngsi-ld:Device:kosice.sk:ovzdusie:lamps-7";

/// A space whose name has this one as a prefix: a segment is a whole name, never a prefix.
const LOOKALIKE: &str = "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie-archiv:lamps-7";

/// The same organization spelt in another case, which is a different domain.
const OTHER_CASE: &str = "urn:ngsi-ld:Device:BANSKABYSTRICA.SK:ovzdusie:lamps-7";

/// One hop as the broker saw it: what was asked, and which tenant it was asked for.
type Hops = Arc<Mutex<Vec<(String, String)>>>;

async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let tenant = tenant_of(request.headers());
            recorder.lock().expect("the hop log").push((
                format!("{} {}", request.method(), request.uri().path()),
                tenant,
            ));
            StatusCode::NO_CONTENT
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

/// The tenant the broker was told, or `<none>`: a missing header is the broker's default tenant
/// and a forged one would be a different space, so the two are never folded together.
fn tenant_of(headers: &HeaderMap) -> String {
    headers
        .get("ngsild-tenant")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>")
        .to_owned()
}

/// Anonymous callers may write devices of this space, and read them back.
fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [createEntity, updateEntity, updateAttrs, deleteEntity, upsertBatch, \
         deleteBatch, createSubscription, queryEntity, retrieveEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Device\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
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
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policies: vec![policy()],
    }
}

/// One request through the whole router, with the tenant header the attack sends on it.
async fn send_with(
    method: Method,
    uri: &str,
    body: Option<Value>,
    tenant: Option<&str>,
) -> (StatusCode, Value, Vec<(String, String)>) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            // A subscription is only stored when the gateway knows an address to route its
            // delivery back through (R46); without one the surface answers 501 and the tenancy
            // of a subscription would never be reached.
            .deliver_through(Some("https://platform.example.sk".to_owned()))
            .seal_subscribers_with(common::delivery_key())
            .serve([endpoint()]),
    );
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{uri}"));
    if body.is_some() {
        request = request.header("content-type", "application/ld+json");
    }
    if let Some(tenant) = tenant {
        request = request.header("NGSILD-Tenant", tenant);
    }
    let response = router(gateway)
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let problem = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let asked = hops.lock().expect("the hop log").clone();
    (status, problem, asked)
}

async fn send(
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, Vec<(String, String)>) {
    send_with(method, uri, body, None).await
}

fn device(id: &str) -> Value {
    json!({
        "id": id,
        "type": "Device",
        "status": { "type": "Property", "value": "on" }
    })
}

/// The refusal a foreign id earns: a bad request that names the id, never a policy decision and
/// never a silence the caller has to guess at (PF-42, GW16).
fn assert_refused(status: StatusCode, problem: &Value, asked: &[(String, String)], id: &str) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {problem}");
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains(id) && detail.contains(SPACE),
        "{id}: the refusal does not say which id was refused and which space it was refused for: {problem}"
    );
    assert!(asked.is_empty(), "{id}: the broker was asked: {asked:?}");
}

#[tokio::test]
async fn an_entity_created_under_another_space_or_organization_never_reaches_the_broker() {
    for id in [OTHER_SPACE, OTHER_ORG, LOOKALIKE, OTHER_CASE] {
        let (status, problem, asked) = send(Method::POST, "/entities", Some(device(id))).await;
        assert_refused(status, &problem, &asked, id);
    }
}

#[tokio::test]
async fn one_foreign_id_refuses_a_whole_batch_and_nothing_of_it_is_written() {
    // GW18 answers a batch per entity where the grant decides it; a foreign id is not a grant
    // decision but a malformed write, and a batch half applied is the one outcome nobody asked
    // for, so the whole call is refused before the broker sees any of it.
    let (status, problem, asked) = send(
        Method::POST,
        "/entityOperations/upsert",
        Some(json!([device(OURS), device(OTHER_SPACE)])),
    )
    .await;
    assert_refused(status, &problem, &asked, OTHER_SPACE);

    let (status, problem, asked) = send(
        Method::POST,
        "/entityOperations/delete",
        Some(json!([OURS, OTHER_ORG])),
    )
    .await;
    assert_refused(status, &problem, &asked, OTHER_ORG);
}

#[tokio::test]
async fn an_addressed_write_to_a_foreign_id_never_reaches_the_broker() {
    for id in [OTHER_SPACE, OTHER_ORG, LOOKALIKE] {
        let (status, problem, asked) = send(
            Method::PATCH,
            &format!("/entities/{id}/attrs"),
            Some(json!({ "status": { "type": "Property", "value": "off" } })),
        )
        .await;
        assert_refused(status, &problem, &asked, id);

        let (status, problem, asked) = send(Method::DELETE, &format!("/entities/{id}"), None).await;
        assert_refused(status, &problem, &asked, id);
    }
}

#[tokio::test]
async fn a_body_that_names_another_space_than_the_path_is_refused() {
    // The path is inside this space and the body is not: two identifiers, and the write is
    // refused rather than one of them being picked (GW17, PF-44).
    let (status, problem, asked) = send(
        Method::PATCH,
        &format!("/entities/{OURS}/attrs"),
        Some(device(OTHER_SPACE)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}

#[tokio::test]
async fn a_forged_tenant_header_on_a_write_is_replaced_by_the_endpoints_own_space() {
    // The header is the other half of the same attack: the id is this space's, so the write is
    // legal, and the broker must still be told this space and not the one the caller named.
    for forged in ["doprava", "OVZDUSIE", ""] {
        let (status, _, asked) =
            send_with(Method::POST, "/entities", Some(device(OURS)), Some(forged)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{forged}");
        assert_eq!(
            asked,
            vec![("POST /ngsi-ld/v1/entities".to_owned(), SPACE.to_owned())],
            "the forged tenant {forged:?} reached the broker"
        );
    }
}

#[tokio::test]
async fn a_subscription_watching_another_space_is_stored_in_this_one() {
    // A subscription is a standing read, and its `entities` carry ids like any other selector.
    // Whatever space they name, the broker is told this endpoint's tenant, so the subscription
    // can only ever match what this space holds.
    let (status, _, asked) = send(
        Method::POST,
        "/subscriptions",
        Some(json!({
            "id": "urn:ngsi-ld:Subscription:banskabystrica.sk:ovzdusie:watch",
            "type": "Subscription",
            "entities": [{ "id": OTHER_SPACE, "type": "Device" }],
            "notification": { "endpoint": { "uri": "https://alerts.example.sk/hook" } }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(
        asked[0].1, SPACE,
        "the subscription was stored in {:?}",
        asked[0].1
    );
}

/// The control: the same routes carry an id of this space through to the broker, with the tenant
/// pinned. Without it every case above would pass on a surface that refused everything.
#[tokio::test]
async fn an_id_of_this_space_is_written_through_every_route_it_was_refused_on() {
    for (method, uri, body) in [
        (
            Method::POST,
            "/entities".to_owned(),
            Some(json!(device(OURS))),
        ),
        (
            Method::POST,
            "/entityOperations/upsert".to_owned(),
            Some(json!([device(OURS)])),
        ),
        (
            Method::POST,
            "/entityOperations/delete".to_owned(),
            Some(json!([OURS])),
        ),
        (
            Method::PATCH,
            format!("/entities/{OURS}/attrs"),
            Some(json!({ "status": { "type": "Property", "value": "off" } })),
        ),
        (Method::DELETE, format!("/entities/{OURS}"), None),
    ] {
        let (status, problem, asked) = send(method.clone(), &uri, body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{method} {uri}: {problem}");
        assert_eq!(asked.len(), 1, "{method} {uri}: {asked:?}");
        assert_eq!(asked[0].1, SPACE, "{method} {uri}: {asked:?}");
    }
}
