//! Attack vector: notifications as an amplifier (T-1699; R46, PL-23).
//!
//! A subscription is work this platform does later, on somebody else's schedule: the broker
//! matches, the gateway projects and dials, and the subscriber decides how long that takes. The
//! amplification is in the last step — a host that accepts the connection and never answers costs
//! the attacker one subscription and the gateway a task and a connection for as long as it likes.
//!
//! `notification_egress_tests.rs` owns the rest of this vector and proves it: the delivery target
//! is read from the stored subscription and never from the request
//! (`the_target_comes_from_the_stored_subscription_and_not_from_the_request`), a delivery carries
//! only the attributes the subscription was narrowed to, an endpoint's hidden attribute is
//! stripped even when the subscription names it, an entity that no longer matches or lies outside
//! the granted area is not delivered, and a subscription this gateway did not route is not
//! delivered at all. `attack_notification_targets_tests.rs` owns where a delivery may go.
//!
//! What is left, and what this file plays, is the clock.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "d7m2xq9vkt4zc6wrb8shj5nfp3";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const BASE: &str = "https://gw.banskabystrica.sk";
const SUBSCRIPTION: &str = "urn:ngsi-ld:Subscription:banskabystrica.sk:ovzdusie:senzory";

fn endpoint() -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: DOMAIN.to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        policies: Vec::new(),
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

/// A subscriber that accepts the delivery and answers nothing, ever.
async fn silent() -> (String, Arc<Mutex<usize>>) {
    let reached = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&reached);
    let app = Router::new().fallback(any(move |_request: Request| {
        let counter = Arc::clone(&counter);
        async move {
            *counter.lock().expect("the hop count") += 1;
            std::future::pending::<()>().await;
            StatusCode::NO_CONTENT.into_response()
        }
    }));
    (serve(app).await, reached)
}

/// A broker holding one subscription, which is all the delivery path reads of it.
async fn broker(target: &str) -> String {
    let stored = json!({
        "id": SUBSCRIPTION,
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": {
            "attributes": ["temperature"],
            "endpoint": {
                "uri": format!(
                    "{BASE}/api/endpoint/{SLUG}/egress/notifications?to={}",
                    percent(target)
                ),
                "accept": "application/json"
            }
        }
    });
    let app = Router::new().fallback(any(move |request: Request| {
        let stored = stored.clone();
        async move {
            match request
                .uri()
                .path()
                .starts_with("/ngsi-ld/v1/subscriptions/")
            {
                true => axum::Json(stored).into_response(),
                false => axum::Json(Value::Array(Vec::new())).into_response(),
            }
        }
    }));
    serve(app).await
}

fn percent(raw: &str) -> String {
    raw.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// R46: the delivery is time-bounded, so one silent subscriber cannot hold a gateway task.
///
/// The clock is the test's, not the wall's (`start_paused`): the runtime goes idle waiting for
/// a subscriber that never answers, the timer advances to the gateway's own deadline, and the
/// answer arrives without anybody waiting fifteen seconds. A gateway that never gives up would
/// hang here instead, which is exactly the failure the case is about.
#[tokio::test(start_paused = true)]
async fn a_subscriber_that_never_answers_does_not_hold_the_delivery_open() {
    let (webhook, reached) = silent().await;
    let upstream = broker(&webhook).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .deliver_through(Some(BASE.to_owned()))
            .deliver_privately_to(vec!["127.0.0.1".to_owned()])
            .serve([endpoint()]),
    );

    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/endpoint/{SLUG}/egress/notifications?to={}",
                    percent(&webhook)
                ))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "id": "urn:ngsi-ld:Notification:1",
                        "type": "Notification",
                        "subscriptionId": SUBSCRIPTION,
                        "notifiedAt": "2026-09-20T08:00:00Z",
                        "data": [{
                            "id": "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:s1",
                            "type": "AirQualityObserved",
                            "temperature": { "type": "Property", "value": 19.5 }
                        }]
                    })
                    .to_string(),
                ))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers rather than hanging on the subscriber");

    assert_eq!(
        *reached.lock().expect("the hop count"),
        1,
        "the subscriber was never dialled, so the case proves nothing"
    );
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let problem: Value = serde_json::from_slice(&bytes).expect("a problem document");
    assert_eq!(problem["status"], 504);
    let detail = problem["detail"].as_str().unwrap_or_default();
    assert!(
        !detail.contains("127.0.0.1"),
        "the refusal carries the subscriber's address, which can hold a credential: {problem}"
    );
}
