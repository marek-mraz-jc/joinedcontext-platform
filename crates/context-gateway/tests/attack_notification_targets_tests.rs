//! Attack vector: the URLs the gateway fetches for a caller (T-1698; R46, GW27).
//!
//! A subscription's notification endpoint is the one address a caller gets to choose that this
//! platform will later dial by itself, with no caller holding the other end. T-1302 gave it two
//! checks — the address is refused when the subscription is stored, and where it points is
//! checked again when a notification is delivered — and `notification_egress_tests.rs` plays the
//! plain forms of both: `127.0.0.1`, `10.1.2.3`, `169.254.169.254`, `[::1]`, `localhost`, and a
//! name that resolves inward after the fact.
//!
//! A plain form is not how this address is sent. The cases here are the disguises: the metadata
//! address behind user info, an IPv4 address mapped into IPv6, a host spelt in capitals or with
//! the trailing dot of a fully qualified name, unique-local and link-local IPv6, and the two
//! ranges that are neither loopback nor RFC 1918 but are still nobody's public host. The last
//! case is the delivery hop itself: a subscriber that answers a redirect does not get the
//! gateway to fetch what it points at.
//!
//! The JSON-LD `@context` of a request is the other URL a caller chooses, and it is fetched by
//! the broker rather than by the gateway: the gateway forwards `Link` to the broker as it
//! forwards the rest of the request, and AntaresBroker's loader
//! (`crates/antares-jsonld/src/loader.rs`) refuses the metadata address and the private ranges
//! there, resolver and redirect policy included. It is tested in that repository, which is why
//! nothing here fetches a context.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::egress::notifications::narrow_subscription;
use context_gateway::pdp::evaluator::Constraints;
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

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn endpoint() -> Endpoint {
    Endpoint {
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

fn granted() -> Constraints {
    Constraints {
        tenant: SPACE.to_owned(),
        types: set(&["AirQualityObserved"]),
        attrs: set(&["temperature"]),
        ..Constraints::default()
    }
}

fn subscription(uri: &str) -> Value {
    json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": uri, "accept": "application/json" } },
    })
}

/// Narrows a creation, which is where the address is judged.
fn created(uri: &str) -> Result<Value, Box<jc_core::ProblemDetails>> {
    let mut value = subscription(uri);
    narrow_subscription(&mut value, &granted(), &endpoint(), BASE, &[], true).map(|()| value)
}

/// R46, T-1302: the address is read as a URL, not as text. Every spelling below reaches a host
/// inside the platform, and each of them was a way past a check that reads the string.
#[tokio::test]
async fn a_disguised_internal_address_is_refused_when_the_subscription_is_stored() {
    for uri in [
        // User info in front of the authority: the host is what follows the last `@`.
        "http://alerts.example.sk@169.254.169.254/latest/meta-data/",
        "http://user:password@127.0.0.1/hook",
        // The same addresses mapped into IPv6, which a socket resolves back to IPv4.
        "http://[::ffff:169.254.169.254]/hook",
        "http://[::ffff:127.0.0.1]/hook",
        "http://[::ffff:10.1.2.3]/hook",
        // Host names are case-insensitive and may carry the root's trailing dot.
        "http://LOCALHOST:8080/hook",
        "http://LocalHost./hook",
        "http://127.0.0.1./hook",
        // IPv6 unique-local and link-local, which are private ranges with no RFC 1918 shape.
        "http://[fd00::1]/hook",
        "http://[fe80::1%25eth0]/hook",
        // Neither loopback nor RFC 1918, and still nobody's public host: shared address space
        // (RFC 6598), `0.0.0.0/8` and the unspecified address.
        "http://100.64.0.1/hook",
        "http://0.0.0.0/hook",
        "http://0.1.2.3/hook",
        "http://[::]/hook",
    ] {
        let problem = created(uri).expect_err("this address is inside the platform");
        assert_eq!(problem.status, 400, "{uri} was stored");
        assert!(
            !format!("{:?}", problem).contains("169.254.169.254"),
            "the refusal repeats the address back: {problem:?}"
        );
    }
}

/// The control: an ordinary public subscriber is stored, and stored routed back through the
/// gateway. Without it every case above would pass on a surface that refused every address.
#[tokio::test]
async fn a_public_subscriber_is_stored_and_routed_through_the_gateway() {
    let stored = created("https://alerts.example.sk/hook").expect("a public address");
    let uri = stored["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("a uri");
    assert!(uri.starts_with(BASE), "{uri}");
    assert!(
        uri.contains("to=https%3A%2F%2Falerts.example.sk%2Fhook"),
        "{uri}"
    );
}

/// What the subscriber's own webhook answered, and what it was asked.
type Seen = Arc<Mutex<Vec<String>>>;

#[derive(Clone)]
struct Redirecting {
    /// Where the webhook sends the gateway next.
    location: String,
    seen: Seen,
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

/// A webhook that answers `302` and points somewhere else, which is the cheapest way to ask a
/// platform to fetch an address it would never have accepted.
async fn redirecting(location: String) -> (String, Seen) {
    let state = Redirecting {
        location,
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let seen = Arc::clone(&state.seen);
    let app = Router::new()
        .fallback(any(
            |State(state): State<Redirecting>, request: Request| async move {
                state
                    .seen
                    .lock()
                    .expect("the hop log")
                    .push(request.uri().path().to_owned());
                (
                    StatusCode::FOUND,
                    [(axum::http::header::LOCATION, state.location.clone())],
                )
                    .into_response()
            },
        ))
        .with_state(state);
    (serve(app).await, seen)
}

/// The address the redirect points at: a second server that must never be reached.
async fn inside() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.uri().path().to_owned());
            StatusCode::NO_CONTENT
        }
    }));
    (serve(app).await, seen)
}

/// A broker holding one subscription, which is all the delivery path reads.
async fn broker(stored: Value) -> String {
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

/// The percent-encoding the gateway uses for a value it puts in a query string.
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

/// R46: a subscriber may answer a delivery however it likes, and the one answer that would make
/// the gateway fetch a second address is a redirect. The gateway hands the status back to the
/// broker and dials nothing: the address a redirect names was never judged when the subscription
/// was stored, and following it would be an SSRF with the platform's own network position.
#[tokio::test]
async fn a_redirect_from_the_subscriber_is_never_followed() {
    let (secret, reached) = inside().await;
    let (webhook, asked) = redirecting(format!("{secret}/latest/meta-data/")).await;
    let stored = json!({
        "id": SUBSCRIPTION,
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": {
            "attributes": ["temperature"],
            "endpoint": {
                "uri": format!(
                    "{BASE}/api/endpoint/{SLUG}/egress/notifications?to={}",
                    percent(&webhook)
                ),
                "accept": "application/json"
            }
        }
    });
    let upstream = broker(stored).await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .deliver_through(Some(BASE.to_owned()))
            // The sinks listen on loopback; an installation names its own in-cluster
            // subscribers the same way (T-1302).
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
        .expect("the gateway answers");

    assert_eq!(
        asked.lock().expect("the hop log").len(),
        1,
        "the subscriber was not delivered to"
    );
    assert!(
        reached.lock().expect("the hop log").is_empty(),
        "the gateway followed the subscriber's redirect: {:?}",
        reached.lock().expect("the hop log")
    );
    assert_eq!(
        response.status(),
        StatusCode::FOUND,
        "the subscriber's own answer is what the broker is told"
    );
}
