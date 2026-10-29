//! Attack vector: a percent-spelled attribute name in a single-attribute path (SEC1 F-007).
//!
//! The broker decodes the `{attr}` segment of `…/entities/{id}/attrs/{attr}` before it writes
//! (Antares reads it through axum's `Path`), so `attrs/%6fwner` writes `owner`. The gateway's
//! grant, quarantine, personal-data and relationship guards judge the name the path names, so
//! they have to judge the decoded name too, or a spelling the broker reads as `owner` passes a
//! Policy whose `propertyNames` leaves `owner` out.

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

/// Anonymous callers may change the `status` of this space's devices and nothing else.
fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [updateAttrs, replaceAttrs, deleteAttrs]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Device\n\
         \x20   propertyNames: [status]\n"
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

async fn send(method: Method, attribute: &str) -> (StatusCode, Vec<String>, String) {
    let (upstream, hops) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
    );
    let request = Request::builder().method(method.clone()).uri(format!(
        "/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{OURS}/attrs/{attribute}"
    ));
    let request = if method == Method::DELETE {
        request
            .header("jc-reason", "retire the attribute")
            .body(Body::empty())
    } else {
        request
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "type": "Property", "value": "mallory" }).to_string(),
            ))
    };
    let response = router(gateway)
        .oneshot(request.expect("a request"))
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .expect("the answer body");
    let asked = hops.lock().expect("the hop log").clone();
    (status, asked, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn a_percent_spelled_attribute_outside_the_grant_never_reaches_the_broker() {
    for method in [Method::PATCH, Method::PUT] {
        for spelling in ["owner", "%6fwner", "%6Fwner", "%6f%77%6e%65%72"] {
            let (status, asked, body) = send(method.clone(), spelling).await;
            assert!(
                asked.is_empty(),
                "{method} attrs/{spelling}: the broker was asked {asked:?}"
            );
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} attrs/{spelling}: answered {status} {body}"
            );
        }
    }
}

/// The quarantine mark is the gateway's (DM-90): a `DELETE` of it is refused however it is
/// spelled, before the broker is asked.
#[tokio::test]
async fn a_percent_spelled_quarantine_mark_is_refused() {
    for method in [Method::PATCH, Method::DELETE] {
        for spelling in ["quarantinedAt", "%71uarantinedAt", "%71uarantined%41t"] {
            let (status, asked, body) = send(method.clone(), spelling).await;
            assert!(asked.is_empty(), "{method} attrs/{spelling}: {asked:?}");
            assert!(
                status.is_client_error(),
                "{method} attrs/{spelling}: answered {status} {body}"
            );
        }
    }
}

/// The control: a percent-spelled name the grant covers is judged as that name and forwarded,
/// so the refusal above is the grant's and not a blanket refusal of `%`.
#[tokio::test]
async fn a_percent_spelled_attribute_inside_the_grant_is_forwarded() {
    for method in [Method::PATCH, Method::PUT, Method::DELETE] {
        let (status, asked, body) = send(method.clone(), "%73tatus").await;
        assert!(status.is_success(), "{method}: answered {status} {body}");
        assert_eq!(asked.len(), 1, "{method}: {asked:?}");
    }
}
