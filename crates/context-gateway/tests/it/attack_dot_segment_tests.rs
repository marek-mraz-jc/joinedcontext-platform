//! Attack vector: a dot segment in an Endpoint's NGSI-LD path (T-3588 path 1; EP-01, GW16).
//!
//! The gateway decides on the path it received: `operations::operation_of` reads
//! `PATCH …/entities/{id}/attrs/..` as Update Attributes of one attribute named `..`, and the
//! Policy is evaluated for that. A hop that resolves dot segments (RFC 3986 clause 5.2.4: a
//! proxy, or a broker other than Antares behind a space) runs `PATCH …/entities/{id}/attrs`
//! instead, every attribute of the body, and `GET …/entities/..` climbs to the view root. Antares
//! refuses such a name itself (`antares_model::has_dot_segment`), but the decision belongs where
//! the Policy is: a segment that is `.` or `..`, however it is percent-spelled, names no entity,
//! attribute, type or subscription, so the gateway refuses it before any broker is asked.

use crate::common;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "3ecozggnnhjlp5miouhia53mr2";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const OURS: &str = "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:lamps-7";

type Hops = Arc<Mutex<Vec<String>>>;

async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder.lock().expect("the hop log").push(format!(
                "{} {}",
                request.method(),
                request.uri().path()
            ));
            StatusCode::NO_CONTENT
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, common::plain_tree(app)).await;
    });
    (format!("http://{address}"), hops)
}

/// Anonymous callers may read and write devices of this space, and change their attributes.
fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [updateAttrs, replaceAttrs, deleteAttrs, retrieveEntity, queryEntity, \
         deleteEntity, retrieveEntityTypeInfo, retrieveSubscription, retrieveTemporal]\n\
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
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy()],
    }
}

async fn send(method: Method, path: &str) -> (StatusCode, Vec<String>) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
    );
    let body = json!({ "status": { "type": "Property", "value": "off" },
                       "owner": { "type": "Property", "value": "mallory" } });
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1{path}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let asked = hops.lock().expect("the hop log").clone();
    (status, asked)
}

/// Every spelling of a dot segment in every position the operation table reads as a name.
fn attacks() -> Vec<(Method, String)> {
    let mut out = Vec::new();
    for dots in ["..", ".", "%2e%2e", "%2E%2E", ".%2e", "%2e"] {
        out.push((Method::PATCH, format!("/entities/{OURS}/attrs/{dots}")));
        out.push((Method::PUT, format!("/entities/{OURS}/attrs/{dots}")));
        out.push((Method::DELETE, format!("/entities/{OURS}/attrs/{dots}")));
        out.push((Method::GET, format!("/entities/{dots}")));
        out.push((Method::DELETE, format!("/entities/{dots}")));
        out.push((Method::GET, format!("/types/{dots}")));
        out.push((Method::GET, format!("/subscriptions/{dots}")));
        out.push((Method::GET, format!("/temporal/entities/{dots}")));
    }
    out
}

#[tokio::test]
async fn a_dot_segment_never_reaches_the_broker() {
    for (method, path) in attacks() {
        let (status, asked) = send(method.clone(), &path).await;
        assert!(
            asked.is_empty(),
            "{method} {path}: the broker was asked {asked:?}"
        );
        assert!(
            status.is_client_error(),
            "{method} {path}: answered {status}, not a refusal"
        );
    }
}

/// The control: the same requests with a real name pass, so the refusal above is the dot
/// segment's and not the harness's.
#[tokio::test]
async fn a_real_name_in_the_same_place_is_forwarded() {
    let (status, asked) = send(Method::PATCH, &format!("/entities/{OURS}/attrs/status")).await;
    assert!(status.is_success(), "answered {status}");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].ends_with("/attrs/status"), "{asked:?}");
    let (status, asked) = send(Method::GET, &format!("/entities/{OURS}")).await;
    assert!(status.is_success(), "answered {status}");
    assert_eq!(asked.len(), 1, "{asked:?}");
}

/// A name that merely contains dots is a name: only a whole `.` or `..` segment is refused.
#[tokio::test]
async fn a_name_with_dots_inside_is_not_a_dot_segment() {
    let (status, asked) = send(
        Method::GET,
        "/entities/urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:v1..2",
    )
    .await;
    assert!(status.is_success(), "answered {status}");
    assert_eq!(asked.len(), 1, "{asked:?}");
}

/// Path 1 of T-3588 by name: the broker's admin, registration and peer doors are no operation an
/// Endpoint offers, so a token of any Endpoint reaches none of them (S16 of the gateway map).
#[tokio::test]
async fn the_brokers_admin_registration_and_peer_doors_are_no_endpoint_operation() {
    for (method, path) in [
        (Method::GET, "/q/tenants"),
        (Method::DELETE, "/q/tenants/ovzdusie"),
        (Method::GET, "/q/dead-letters"),
        (Method::POST, "/csourceRegistrations"),
        (Method::GET, "/csourceRegistrations"),
        (Method::POST, "/csourceSubscriptions"),
        (Method::POST, "/ex/v1/remote-notify"),
        (Method::GET, "/jsonldContexts"),
        (Method::DELETE, "/jsonldContexts/x"),
        (Method::GET, "/../../q/tenants"),
    ] {
        let (status, asked) = send(method.clone(), path).await;
        assert!(
            asked.is_empty(),
            "{method} {path}: the broker was asked {asked:?}"
        );
        assert!(
            status.is_client_error(),
            "{method} {path}: answered {status}"
        );
    }
}
