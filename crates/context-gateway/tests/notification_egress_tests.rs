//! T-0156: a notification leaves through the gateway, not around it (R46, GW27).
//!
//! Everything else the platform sends is an answer to a request that was projected on the
//! way in. A notification is sent later and to somebody else, so the projection has to
//! happen on the way out — and it has to happen from the subscription as it was *stored*,
//! because at delivery time there is no token and no caller to decide from.

mod common;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::{Service, ServiceExt};

const SLUG: &str = "n4t8xq2vhm6zc9wrb5sdj3kfp7";
const PROJECT: &str = "banskabystrica";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const PUBLIC_URL: &str = "https://gw.banskabystrica.sk";

/// The in-cluster Service a deployment points `JC_GATEWAY_EGRESS_URL` at, so a delivery
/// never leaves the cluster and reaches the gateway from the broker rather than the edge.
const EGRESS: &str = "http://context-gateway.jc-context-gateway.svc.cluster.local:8080";
const SUBSCRIPTION: &str = "urn:ngsi-ld:Subscription:ovzdusie:senzory";
const SENSOR: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:senzor-01";
const OTHER: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:senzor-02";
/// The attribute the grant does not name, which no delivery may ever carry.
const UNGRANTED: &str = "operatorPhone";

/// What one side of the egress path recorded.
type Seen = Arc<Mutex<Vec<Value>>>;
/// The headers one delivery arrived with.
type Headers = Arc<Mutex<Vec<(String, String)>>>;

#[derive(Clone)]
struct Sink {
    seen: Seen,
    headers: Headers,
}

/// The webhook the subscriber asked for: an ordinary HTTP server that records what arrives.
async fn sink() -> (String, Seen, Headers) {
    let state = Sink {
        seen: Arc::new(Mutex::new(Vec::new())),
        headers: Arc::new(Mutex::new(Vec::new())),
    };
    let (seen, headers) = (Arc::clone(&state.seen), Arc::clone(&state.headers));
    (serve(recorder(state)).await, seen, headers)
}

/// The same webhook behind TLS, and the CA the gateway has to be given to reach it (T-0426).
///
/// The certificate is minted per run rather than committed: a test key in the repository is
/// a finding for the secret scanner and an expiry date waiting to happen.
async fn tls_sink() -> (String, Seen, Vec<u8>) {
    let authority_key = rcgen::KeyPair::generate().expect("a CA key");
    let mut authority = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    authority.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    authority.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    authority
        .distinguished_name
        .push(rcgen::DnType::CommonName, "joinedcontext egress test CA");
    let certificate_authority = authority
        .self_signed(&authority_key)
        .expect("a self-signed CA");
    let issuer = rcgen::Issuer::new(authority, authority_key);

    // The address, not a name: the delivery connects to 127.0.0.1 and nothing in the test
    // depends on how this machine resolves `localhost`.
    let key = rcgen::KeyPair::generate().expect("a server key");
    let certificate = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("the SAN")
        .signed_by(&key, &issuer)
        .expect("a signed certificate");

    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![
                certificate.der().clone(),
                certificate_authority.der().clone(),
            ],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .expect("the certificate matches the key");

    let state = Sink {
        seen: Arc::new(Mutex::new(Vec::new())),
        headers: Arc::new(Mutex::new(Vec::new())),
    };
    let seen = Arc::clone(&state.seen);
    let app = recorder(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (acceptor, app) = (acceptor.clone(), app.clone());
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |request: hyper::Request<_>| {
                    let mut app = app.clone();
                    async move { app.call(request.map(Body::new)).await }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (
        format!("https://{address}"),
        seen,
        certificate_authority.pem().into_bytes(),
    )
}

/// The server both sinks are: it records the body and the headers of everything it is sent.
fn recorder(state: Sink) -> Router {
    Router::new()
        .fallback(any(
            |State(sink): State<Sink>, request: Request| async move {
                sink.headers.lock().expect("the header log").extend(
                    request.headers().iter().filter_map(|(name, value)| {
                        Some((name.to_string(), value.to_str().ok()?.to_owned()))
                    }),
                );
                let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                    .await
                    .expect("a body");
                sink.seen
                    .lock()
                    .expect("the delivery log")
                    .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
                StatusCode::NO_CONTENT
            },
        ))
        .with_state(state)
}

#[derive(Clone)]
struct BrokerState {
    /// The subscription the broker holds, as the gateway would have stored it.
    stored: Option<Value>,
    /// The entity ids the narrowed filter still matches.
    matching: Vec<String>,
    /// The subscriptions the gateway forwarded, as it forwarded them.
    forwarded: Seen,
}

/// A broker that stores subscriptions and answers the one query the egress path makes.
async fn broker(state: BrokerState) -> (String, Seen) {
    let forwarded = Arc::clone(&state.forwarded);
    let app = Router::new()
        .fallback(any(
            |State(state): State<BrokerState>, request: Request| async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let query = request.uri().query().unwrap_or_default().to_owned();

                if method == Method::GET && path == "/ngsi-ld/v1/entities" {
                    let matched: Vec<Value> = state
                        .matching
                        .iter()
                        .filter(|id| query.contains(&percent(id)))
                        .map(|id| json!({ "id": id, "type": "AirQualityObserved" }))
                        .collect();
                    return axum::Json(Value::Array(matched)).into_response();
                }
                if method == Method::GET && path.starts_with("/ngsi-ld/v1/subscriptions/") {
                    return match state.stored.clone() {
                        Some(stored) => axum::Json(stored).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    };
                }
                if method == Method::POST || method == Method::PATCH {
                    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                        .await
                        .expect("a body");
                    state
                        .forwarded
                        .lock()
                        .expect("the forwarding log")
                        .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
                    return StatusCode::CREATED.into_response();
                }
                StatusCode::NO_CONTENT.into_response()
            },
        ))
        .with_state(state);
    (serve(app).await, forwarded)
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

/// The percent-encoding the gateway uses for a value it puts in a query string.
fn percent(raw: &str) -> String {
    raw.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, createSubscription, updateSubscription]\n\
         q: temperature<100\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: AirQualityObserved\n\
         \x20   propertyNames: [temperature, location]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint(hidden: &[&str]) -> Endpoint {
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
        hidden_attributes: hidden.iter().map(|name| (*name).to_owned()).collect(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policies: vec![policy()],
    }
}

/// The same gateway, trusting `authority` on top of the public roots, handing the broker
/// `egress` as the base of a rewritten endpoint, and serving `policy`.
fn gateway_with(
    upstream: String,
    hidden: &[&str],
    authority: Option<&[u8]>,
    egress: Option<&str>,
    policy: PolicySpec,
) -> Arc<Gateway> {
    let broker = match authority {
        None => Broker::new(upstream),
        Some(pem) => Broker::trusting(upstream, pem).expect("the test CA is usable"),
    };
    let realm = common::Realm::new();
    Arc::new(
        Gateway::new(broker, Box::new(PolicyPdp), DOMAIN)
            .authenticate(
                Arc::new(realm.verifier()),
                ServiceAccounts::new(),
                Some(PUBLIC_URL.to_owned()),
            )
            .deliver_through(egress.map(str::to_owned))
            // The sinks listen on this address; the installation would name its own (T-1302).
            .deliver_privately_to(vec!["127.0.0.1".to_owned()])
            .serve([Endpoint {
                roles: Default::default(),
                policies: vec![policy],
                ..endpoint(hidden)
            }]),
    )
}

/// The subscription the gateway would have stored: narrowed, and routed back through itself.
fn stored(to: &str, attributes: Value, q: &str) -> Value {
    json!({
        "id": SUBSCRIPTION,
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "q": q,
        "notification": {
            "attributes": attributes,
            "endpoint": {
                "uri": format!(
                    "{PUBLIC_URL}/api/endpoint/{SLUG}/egress/notifications?to={}",
                    percent(to)
                ),
                "accept": "application/json"
            }
        }
    })
}

fn notification(entities: Vec<Value>) -> Value {
    json!({
        "id": "urn:ngsi-ld:Notification:1",
        "type": "Notification",
        "subscriptionId": SUBSCRIPTION,
        "notifiedAt": "2026-09-07T08:00:00Z",
        "data": entities
    })
}

fn sensor(id: &str) -> Value {
    json!({
        "id": id,
        "type": "AirQualityObserved",
        "temperature": { "type": "Property", "value": 19.5 },
        UNGRANTED: { "type": "Property", "value": "+421 900 000 000" }
    })
}

/// Creates a subscription through the gateway and hands back what the broker was told.
async fn create(subscription: Value) -> (StatusCode, Vec<Value>) {
    create_under(subscription, policy(), None).await
}

/// The same, under a policy the test chose and through the egress base it named.
async fn create_under(
    subscription: Value,
    policy: PolicySpec,
    egress: Option<&str>,
) -> (StatusCode, Vec<Value>) {
    let (upstream, forwarded) = broker(BrokerState {
        stored: None,
        matching: Vec::new(),
        forwarded: Arc::new(Mutex::new(Vec::new())),
    })
    .await;

    let response = router(gateway_with(upstream, &[], None, egress, policy))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/subscriptions"))
                .header("content-type", "application/json")
                .body(Body::from(subscription.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    let status = response.status();
    let seen = forwarded.lock().expect("the forwarding log").clone();
    (status, seen)
}

/// Delivers one notification through the egress path and hands back what the sink saw.
async fn deliver(
    subscription: Option<Value>,
    matching: Vec<String>,
    entities: Vec<Value>,
    hidden: &[&str],
    to: &str,
) -> (StatusCode, Vec<Value>) {
    deliver_via(subscription, matching, entities, hidden, to, None).await
}

/// The same, against a gateway that hands the broker `egress` as the base of a rewritten
/// endpoint. What was stored under the old base still has to arrive (R46).
async fn deliver_via(
    subscription: Option<Value>,
    matching: Vec<String>,
    entities: Vec<Value>,
    hidden: &[&str],
    to: &str,
    egress: Option<&str>,
) -> (StatusCode, Vec<Value>) {
    let (upstream, _) = broker(BrokerState {
        stored: subscription,
        matching,
        forwarded: Arc::new(Mutex::new(Vec::new())),
    })
    .await;

    let response = router(gateway_with(upstream, hidden, None, egress, policy()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/endpoint/{SLUG}/egress/notifications?to={}",
                    percent(to)
                ))
                .header("content-type", "application/json")
                .header("x-auth-token", "the receiverInfo the broker attached")
                .body(Body::from(notification(entities).to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    (response.status(), Vec::new())
}

#[tokio::test]
async fn a_created_subscription_is_narrowed_and_its_delivery_routed_back_through_the_gateway() {
    let (status, forwarded) = create(json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }, { "type": "Device" }],
        "q": "temperature>10",
        "notification": {
            "endpoint": { "uri": "http://mesto.internal/hooks/ovzdusie" }
        }
    }))
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let stored = forwarded.first().expect("the broker was told something");

    let uri = stored["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("a rewritten uri");
    assert!(
        uri.starts_with(&format!(
            "{PUBLIC_URL}/api/endpoint/{SLUG}/egress/notifications?to="
        )),
        "delivery goes through the gateway: {uri}"
    );
    assert!(
        uri.ends_with(&percent("http://mesto.internal/hooks/ovzdusie")),
        "and the subscriber's own endpoint travels with it: {uri}"
    );

    assert_eq!(
        stored["entities"],
        json!([{ "type": "AirQualityObserved" }]),
        "a type outside the grant is not watched"
    );
    assert_eq!(
        stored["q"].as_str().expect("a folded filter"),
        "(temperature>10);((temperature<100))",
        "the subscriber's filter and the grant's, conjoined"
    );
    assert_eq!(
        stored["notification"]["attributes"],
        json!(["location", "temperature"]),
        "a subscription that named no attributes is stored asking for the granted ones"
    );
}

#[tokio::test]
async fn a_subscription_that_watches_nothing_granted_is_refused() {
    let (status, forwarded) = create(json!({
        "type": "Subscription",
        "entities": [{ "type": "Device" }],
        "notification": { "endpoint": { "uri": "http://mesto.internal/hooks/x" } }
    }))
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(forwarded.is_empty(), "nothing was stored: {forwarded:?}");
}

#[tokio::test]
async fn a_subscription_asking_for_an_ungranted_attribute_keeps_only_the_granted_ones() {
    let (status, forwarded) = create(json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": {
            "attributes": ["temperature", UNGRANTED],
            "endpoint": { "uri": "http://mesto.internal/hooks/x" }
        }
    }))
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        forwarded[0]["notification"]["attributes"],
        json!(["temperature"])
    );
}

/// The gateway's dispatcher speaks HTTP, so a subscription it could never deliver is
/// refused at creation rather than stored and silently dropped later.
/// T-0426 changed this test rather than adding one: an `https://` endpoint used to be the
/// `501` this asserted, because the gateway had no TLS client to deliver with.
#[tokio::test]
async fn an_endpoint_behind_tls_is_accepted_at_creation_and_routed_like_any_other() {
    let (status, forwarded) = create(json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": "https://mesto.example/hooks/x" } }
    }))
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let uri = forwarded[0]["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("the stored subscription carries a rewritten endpoint");
    assert_eq!(
        uri,
        format!(
            "{PUBLIC_URL}/api/endpoint/{SLUG}/egress/notifications?to={}",
            percent("https://mesto.example/hooks/x")
        )
    );
}

#[tokio::test]
async fn an_endpoint_that_is_neither_http_nor_https_is_refused_at_creation() {
    let (status, forwarded) = create(json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": "mqtt://mesto.example:1883/hooks" } }
    }))
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(forwarded.is_empty(), "nothing was stored: {forwarded:?}");
}

#[tokio::test]
async fn a_delivered_notification_carries_only_the_attributes_the_subscription_was_narrowed_to() {
    let (webhook, seen, headers) = sink().await;
    let (status, _) = deliver(
        Some(stored(
            &webhook,
            json!(["location", "temperature"]),
            "((temperature<100))",
        )),
        vec![SENSOR.to_owned()],
        vec![sensor(SENSOR)],
        &[],
        &webhook,
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    let delivered = seen.lock().expect("the delivery log").clone();
    assert_eq!(delivered.len(), 1, "one delivery: {delivered:?}");

    let entity = &delivered[0]["data"][0];
    assert_eq!(entity["temperature"]["value"], json!(19.5));
    assert!(
        entity.get(UNGRANTED).is_none(),
        "the phone number is not granted and never leaves: {entity}"
    );
    assert_eq!(entity["id"], json!(SENSOR), "still an NGSI-LD entity");

    let arrived = headers.lock().expect("the header log").clone();
    assert!(
        arrived
            .iter()
            .any(|(name, value)| name == "x-auth-token" && value.contains("receiverInfo")),
        "the authorization material the broker attached reaches the receiver: {arrived:?}"
    );
}

#[tokio::test]
async fn an_endpoints_hidden_attribute_is_stripped_even_when_the_subscription_names_it() {
    let (webhook, seen, _) = sink().await;
    let (status, _) = deliver(
        Some(stored(
            &webhook,
            json!(["location", "temperature"]),
            "((temperature<100))",
        )),
        vec![SENSOR.to_owned()],
        vec![sensor(SENSOR)],
        &["temperature"],
        &webhook,
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    let delivered = seen.lock().expect("the delivery log").clone();
    assert!(
        delivered[0]["data"][0].get("temperature").is_none(),
        "what the endpoint publishes nothing of is not published by a notification either"
    );
}

/// A notification says what changed. Whether the entity still satisfies the condition the
/// grant was narrowed with is a question about now, and only the broker can answer it.
#[tokio::test]
async fn an_entity_that_no_longer_matches_the_condition_is_not_delivered() {
    let (webhook, seen, _) = sink().await;
    let (status, _) = deliver(
        Some(stored(
            &webhook,
            json!(["temperature"]),
            "((temperature<100))",
        )),
        vec![SENSOR.to_owned()],
        vec![sensor(SENSOR), sensor(OTHER)],
        &[],
        &webhook,
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    let delivered = seen.lock().expect("the delivery log").clone();
    let ids: Vec<&Value> = delivered[0]["data"]
        .as_array()
        .expect("the entities")
        .iter()
        .map(|entity| &entity["id"])
        .collect();
    assert_eq!(ids, vec![&json!(SENSOR)], "only the one that still matches");
}

#[tokio::test]
async fn a_notification_whose_entities_all_stopped_matching_dispatches_nothing() {
    let (webhook, seen, _) = sink().await;
    let (status, _) = deliver(
        Some(stored(
            &webhook,
            json!(["temperature"]),
            "((temperature<100))",
        )),
        Vec::new(),
        vec![sensor(SENSOR)],
        &[],
        &webhook,
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        seen.lock().expect("the delivery log").is_empty(),
        "nothing left the platform"
    );
}

/// A subscription created straight on the broker was never narrowed, so the gateway has no
/// projection to apply and refuses to be its delivery agent.
#[tokio::test]
async fn a_subscription_the_gateway_did_not_route_is_not_delivered() {
    let (webhook, seen, _) = sink().await;
    let unrouted = json!({
        "id": SUBSCRIPTION,
        "type": "Subscription",
        "notification": { "endpoint": { "uri": webhook.clone() } }
    });
    let (status, _) = deliver(
        Some(unrouted),
        vec![SENSOR.to_owned()],
        vec![sensor(SENSOR)],
        &[],
        &webhook,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(seen.lock().expect("the delivery log").is_empty());
}

#[tokio::test]
async fn a_delivery_naming_a_subscription_the_broker_does_not_hold_is_refused() {
    let (webhook, seen, _) = sink().await;
    let (status, _) = deliver(
        None,
        vec![SENSOR.to_owned()],
        vec![sensor(SENSOR)],
        &[],
        &webhook,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(seen.lock().expect("the delivery log").is_empty());
}

/// The target is read out of the stored subscription, never out of the request: a forged
/// delivery cannot make the gateway post anywhere the subscription does not name.
#[tokio::test]
async fn the_target_comes_from_the_stored_subscription_and_not_from_the_request() {
    let (webhook, seen, _) = sink().await;
    let (elsewhere, forged, _) = sink().await;
    let (status, _) = deliver(
        Some(stored(
            &webhook,
            json!(["temperature"]),
            "((temperature<100))",
        )),
        vec![SENSOR.to_owned()],
        vec![sensor(SENSOR)],
        &[],
        &elsewhere,
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        seen.lock().expect("the delivery log").len(),
        1,
        "the subscription's own endpoint got it"
    );
    assert!(
        forged.lock().expect("the delivery log").is_empty(),
        "the one the request named did not"
    );
}

/// The area a geo-conditioned grant draws: the city, as a policy would put it on a map.
const CITY: &str = "georel=within;geometry=Polygon;coordinates=\
[[[19.0,48.6],[19.4,48.6],[19.4,48.9],[19.0,48.9],[19.0,48.6]]]";

/// The same grant, with a geographic condition on it (GW11).
fn geo_policy() -> PolicySpec {
    PolicySpec {
        geo_q: Some(CITY.to_owned()),
        ..policy()
    }
}

/// A sensor that says where it is, which is what a geo grant is decided on.
fn placed(id: &str, longitude: f64, latitude: f64) -> Value {
    let mut entity = sensor(id);
    entity["location"] = json!({
        "type": "GeoProperty",
        "value": { "type": "Point", "coordinates": [longitude, latitude] }
    });
    entity
}

/// The stored subscription of a caller whose grants drew `areas`.
fn stored_in(to: &str, attributes: Value, q: &str, areas: &[&str]) -> Value {
    let mut subscription = stored(to, attributes, q);
    let uri = subscription["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("the stored endpoint")
        .to_owned();
    subscription["notification"]["endpoint"]["uri"] = Value::String(
        areas
            .iter()
            .fold(uri, |uri, area| format!("{uri}&area={}", percent(area))),
    );
    subscription
}

#[tokio::test]
async fn a_subscriber_behind_tls_is_delivered_to_over_tls() {
    let (webhook, seen, authority) = tls_sink().await;
    let (upstream, _) = broker(BrokerState {
        stored: Some(stored(
            &webhook,
            json!(["temperature"]),
            "((temperature<100))",
        )),
        matching: vec![SENSOR.to_owned()],
        forwarded: Arc::new(Mutex::new(Vec::new())),
    })
    .await;

    let response = router(gateway_with(
        upstream,
        &[],
        Some(&authority),
        None,
        policy(),
    ))
    .oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/endpoint/{SLUG}/egress/notifications"))
            .header("content-type", "application/json")
            .body(Body::from(notification(vec![sensor(SENSOR)]).to_string()))
            .expect("a request"),
    )
    .await
    .expect("the gateway answers");

    assert!(
        response.status().is_success(),
        "the delivery failed: {}",
        response.status()
    );
    let delivered = seen.lock().expect("the delivery log").clone();
    assert_eq!(delivered.len(), 1, "delivered over TLS: {delivered:?}");
    assert_eq!(delivered[0]["data"][0]["id"], json!(SENSOR));
}

#[tokio::test]
async fn a_grant_with_a_geographic_condition_stores_the_subscription_with_its_area() {
    let (status, forwarded) = create_under(
        json!({
            "type": "Subscription",
            "entities": [{ "type": "AirQualityObserved" }],
            "notification": { "endpoint": { "uri": "http://mesto.example/hooks/x" } }
        }),
        geo_policy(),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "the subscription is stored");
    let uri = forwarded[0]["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("the stored subscription carries a rewritten endpoint");
    assert!(
        uri.contains(&format!("&area={}", percent(CITY))),
        "the granted area travels with the delivery: {uri}"
    );
}

#[tokio::test]
async fn an_entity_outside_the_granted_area_is_not_delivered() {
    let (webhook, seen, _) = sink().await;
    let inside = placed(SENSOR, 19.15, 48.73);
    let outside = placed(OTHER, 21.24, 48.72);
    let (upstream, _) = broker(BrokerState {
        stored: Some(stored_in(
            &webhook,
            json!(["location", "temperature"]),
            "((temperature<100))",
            &[CITY],
        )),
        matching: vec![SENSOR.to_owned(), OTHER.to_owned()],
        forwarded: Arc::new(Mutex::new(Vec::new())),
    })
    .await;

    let response = router(gateway_with(upstream, &[], None, None, geo_policy()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/egress/notifications"))
                .header("content-type", "application/json")
                .body(Body::from(notification(vec![inside, outside]).to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    assert!(response.status().is_success(), "{}", response.status());
    let delivered = seen.lock().expect("the delivery log").clone();
    assert_eq!(delivered.len(), 1, "one delivery: {delivered:?}");
    let entities = delivered[0]["data"].as_array().expect("the entities");
    assert_eq!(
        entities.len(),
        1,
        "only the one inside the area: {entities:?}"
    );
    assert_eq!(entities[0]["id"], json!(SENSOR));
}

#[tokio::test]
async fn an_entity_the_gateway_cannot_place_is_not_delivered_under_a_geo_grant() {
    let (webhook, seen, _) = sink().await;
    let (upstream, _) = broker(BrokerState {
        stored: Some(stored_in(
            &webhook,
            json!(["location", "temperature"]),
            "((temperature<100))",
            &[CITY],
        )),
        matching: vec![SENSOR.to_owned()],
        forwarded: Arc::new(Mutex::new(Vec::new())),
    })
    .await;

    let response = router(gateway_with(upstream, &[], None, None, geo_policy()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/egress/notifications"))
                .header("content-type", "application/json")
                // No `location`: an entity that cannot be placed cannot be shown to be
                // inside the grant, which is the answer a spatial read gives too.
                .body(Body::from(notification(vec![sensor(SENSOR)]).to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        seen.lock().expect("the delivery log").is_empty(),
        "nothing left the platform"
    );
}

/// Where the broker delivers is not where a caller's token is audience-bound (R46, PF-45).
#[tokio::test]
async fn a_deployment_that_names_an_egress_url_has_the_broker_deliver_there() {
    let (status, forwarded) = create_under(
        json!({
            "type": "Subscription",
            "entities": [{ "type": "AirQualityObserved" }],
            "notification": { "endpoint": { "uri": "http://mesto.internal/hooks/ovzdusie" } }
        }),
        policy(),
        Some(EGRESS),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let uri = forwarded[0]["notification"]["endpoint"]["uri"]
        .as_str()
        .expect("a rewritten uri");
    assert!(
        uri.starts_with(&format!(
            "{EGRESS}/api/endpoint/{SLUG}/egress/notifications?to="
        )),
        "the broker is handed the in-cluster address: {uri}"
    );
    assert!(
        !uri.starts_with(PUBLIC_URL),
        "and not the public one, which would send the delivery out through the edge: {uri}"
    );
    assert!(
        uri.ends_with(&percent("http://mesto.internal/hooks/ovzdusie")),
        "the subscriber's own endpoint still travels with it: {uri}"
    );
}

/// A deployment that adds the variable does not orphan what it already stored: the target is
/// read back out of the stored subscription by its path, not by the base in front of it.
#[tokio::test]
async fn a_subscription_stored_under_the_public_url_is_delivered_after_the_egress_url_arrives() {
    let (webhook, seen, _) = sink().await;
    let (status, _) = deliver_via(
        Some(stored(
            &webhook,
            json!(["temperature"]),
            "((temperature<100))",
        )),
        vec![SENSOR.to_owned()],
        vec![sensor(SENSOR)],
        &[],
        &webhook,
        Some(EGRESS),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        seen.lock().expect("the delivery log").len(),
        1,
        "a subscription written under the old base is still delivered"
    );
}

/// A subscription whose endpoint points inside the platform's own networks: `status` for it.
async fn created_with_endpoint(uri: &str) -> (StatusCode, Vec<Value>) {
    create(json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": uri } }
    }))
    .await
}

#[tokio::test]
async fn a_subscription_with_a_private_or_metadata_notification_endpoint_is_refused() {
    for uri in [
        "http://169.254.169.254/latest/meta-data/",
        "http://10.43.0.10:8080/admin",
        "http://192.168.1.1/",
        "https://172.16.0.5/hook",
        "http://100.64.0.1/",
        "http://0.0.0.0:8080/",
        "http://[::1]:8080/",
        "http://[fd00::1]/",
        "http://[fe80::1]/",
        "http://[::ffff:10.0.0.1]/",
        "http://user:pass@10.0.0.1/",
    ] {
        let (status, forwarded) = created_with_endpoint(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(
            forwarded.is_empty(),
            "nothing was stored for {uri}: {forwarded:?}"
        );
    }
}

#[tokio::test]
async fn a_subscription_with_a_loopback_notification_endpoint_is_refused() {
    for uri in [
        "http://127.0.0.2:8080/",
        "http://localhost:8080/",
        "http://api.localhost/",
        "http://LOCALHOST./",
    ] {
        let (status, forwarded) = created_with_endpoint(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(
            forwarded.is_empty(),
            "nothing was stored for {uri}: {forwarded:?}"
        );
    }
}

#[tokio::test]
async fn a_subscription_with_a_public_notification_endpoint_is_accepted() {
    let (status, forwarded) = created_with_endpoint("https://example.com/webhook").await;
    assert!(status.is_success(), "{status}");
    assert_eq!(forwarded.len(), 1, "the broker got the subscription");
}

#[tokio::test]
async fn a_host_the_installation_names_is_let_through_although_it_is_private() {
    // The test gateway names 127.0.0.1, as an installation names its in-cluster subscriber.
    let (status, forwarded) = created_with_endpoint("http://127.0.0.1:9000/hook").await;
    assert!(status.is_success(), "{status}");
    assert_eq!(forwarded.len(), 1);
}

#[tokio::test]
async fn a_stored_endpoint_whose_name_resolves_inside_the_platform_is_not_delivered_to() {
    let (webhook, seen, _) = sink().await;
    // The same sink, reached by a name rather than by the address the installation named.
    let by_name = webhook.replacen("127.0.0.1", "localhost", 1);
    let (status, _) = deliver(
        Some(stored(
            &by_name,
            json!(["temperature"]),
            "((temperature<100))",
        )),
        vec![SENSOR.to_owned()],
        vec![sensor(SENSOR)],
        &[],
        &by_name,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        seen.lock().expect("the delivery log").is_empty(),
        "nothing left the gateway"
    );
}

/// MP-02, T-1862: the projection of the endpoint the subscription belongs to, applied per type at
/// delivery.
///
/// A subscription carries one attribute list, so a grant over two classes with different slots is
/// stored as the union of both. Applying that union to every entity delivers a `Device`'s
/// `temperature` to a subscriber who may only read an `AirQualityObserved`'s — the leak the read
/// path closed in T-1862, on the path where the answer arrives later.
const TWO_CLASS_VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata: { name: sensors, namespace: banskabystrica }
spec:
  contextSpaceRef: ovzdusie
  dataModelRef: { kind: DataModel, name: ovzdusie, version: "1" }
  classes:
    - name: AirQualityObserved
      slots: [temperature, location]
    - name: Device
      slots: [battery, location]
"#;

/// One delivery through an endpoint that carries the two-class projection, with what the sink saw.
async fn deliver_projected(
    stored_subscription: Value,
    entities: Vec<Value>,
    hidden: &[&str],
) -> (StatusCode, Value) {
    let (webhook, seen, _) = sink().await;
    let subscription = {
        let mut subscription = stored_subscription;
        subscription["notification"]["endpoint"]["uri"] = json!(format!(
            "{PUBLIC_URL}/api/endpoint/{SLUG}/egress/notifications?to={}",
            percent(&webhook)
        ));
        subscription
    };
    let matching: Vec<String> = entities
        .iter()
        .filter_map(|entity| entity["id"].as_str().map(str::to_owned))
        .collect();
    let (upstream, _) = broker(BrokerState {
        stored: Some(subscription),
        matching,
        forwarded: Arc::new(Mutex::new(Vec::new())),
    })
    .await;

    let projection =
        jc_core::envelope::ResourceEnvelope::<jc_core::kinds::ModelProjectionSpec>::from_yaml(
            TWO_CLASS_VIEW,
        )
        .expect("the projection parses");
    let realm = common::Realm::new();
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
            .authenticate(
                Arc::new(realm.verifier()),
                ServiceAccounts::new(),
                Some(PUBLIC_URL.to_owned()),
            )
            .deliver_privately_to(vec!["127.0.0.1".to_owned()])
            .serve([Endpoint {
                roles: Default::default(),
                projection: Some(Arc::new(projection.spec)),
                ..endpoint(hidden)
            }]),
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
                .body(Body::from(notification(entities).to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let delivered = seen.lock().expect("the delivery log").clone();
    (status, delivered.first().cloned().unwrap_or(Value::Null))
}

#[tokio::test]
async fn a_delivered_entity_keeps_the_slots_of_its_own_type() {
    let (status, delivered) = deliver_projected(
        json!({
            "id": SUBSCRIPTION,
            "type": "Subscription",
            "entities": [{ "type": "AirQualityObserved" }, { "type": "Device" }],
            "notification": {
                // The union of both classes' slots, which is what one subscription can carry.
                "attributes": ["battery", "location", "temperature"],
                "endpoint": { "uri": "replaced by the helper", "accept": "application/json" }
            }
        }),
        vec![
            json!({
                "id": SENSOR,
                "type": "AirQualityObserved",
                "temperature": { "type": "Property", "value": 19.5 },
                "battery": { "type": "Property", "value": 42 }
            }),
            json!({
                "id": "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:device-01",
                "type": "Device",
                "temperature": { "type": "Property", "value": 31.0 },
                "battery": { "type": "Property", "value": 88 }
            }),
            // A type the projection does not name is not this endpoint's to deliver at all.
            json!({
                "id": "urn:ngsi-ld:Camera:banskabystrica.sk:ovzdusie:cam-01",
                "type": "Camera",
                "location": { "type": "GeoProperty", "value": { "type": "Point", "coordinates": [19.1, 48.7] } }
            }),
        ],
        &[],
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{delivered}");
    let data = delivered["data"]
        .as_array()
        .unwrap_or_else(|| panic!("a delivery carries its data: {delivered}"));
    assert_eq!(data.len(), 2, "the Camera was delivered too: {delivered}");

    let sensor = &data[0];
    assert_eq!(sensor["temperature"]["value"], json!(19.5), "{sensor}");
    assert!(
        sensor.get("battery").is_none(),
        "an AirQualityObserved carried a Device's battery: {sensor}"
    );

    let device = &data[1];
    assert_eq!(device["battery"]["value"], json!(88), "{device}");
    assert!(
        device.get("temperature").is_none(),
        "a Device carried an AirQualityObserved's temperature: {device}"
    );
    assert!(
        !delivered.to_string().contains("Camera"),
        "the type outside the projection is named in the delivery: {delivered}"
    );
}

/// EP-26, MP-02, EP-61, T-2261: the one delivery that has to carry nothing it should not — a
/// notification about entities of three types, one of them outside the projection, each carrying a
/// canary for every attribute including the one the endpoint hides.
///
/// The sweep asks the read surfaces this question 105 ways. A notification is the same answer sent
/// later and to somebody else, so it is asked here once, with the same kind of fixture: whatever is
/// delivered is searched for every marker it may not hold.
#[tokio::test]
async fn a_notification_carries_only_what_the_subscriber_may_read() {
    let canary = |kind: &str, id: &str| {
        let mut entity = json!({ "id": id, "type": kind });
        for attr in ["temperature", "battery", "location", UNGRANTED] {
            entity[attr] = json!({ "type": "Property", "value": format!("CANARY-{kind}-{attr}") });
        }
        entity
    };
    let (status, delivered) = deliver_projected(
        json!({
            "id": SUBSCRIPTION,
            "type": "Subscription",
            "entities": [{ "type": "AirQualityObserved" }, { "type": "Device" }],
            "notification": {
                "attributes": ["battery", "location", "temperature", UNGRANTED],
                "endpoint": { "uri": "replaced by the helper", "accept": "application/json" }
            }
        }),
        vec![
            canary("AirQualityObserved", SENSOR),
            canary(
                "Device",
                "urn:ngsi-ld:Device:banskabystrica.sk:ovzdusie:device-01",
            ),
            canary(
                "Camera",
                "urn:ngsi-ld:Camera:banskabystrica.sk:ovzdusie:cam-01",
            ),
        ],
        &[UNGRANTED],
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{delivered}");

    let sent = delivered.to_string();
    // What each type may serve, per the projection the endpoint carries.
    for forbidden in [
        "CANARY-AirQualityObserved-battery",
        "CANARY-Device-temperature",
        "CANARY-AirQualityObserved-operatorPhone",
        "CANARY-Device-operatorPhone",
        "Camera",
        UNGRANTED,
    ] {
        assert!(
            !sent.contains(forbidden),
            "the delivery carries {forbidden}: {sent}"
        );
    }
    // And it is still a delivery: a guard that sends nothing proves nothing.
    assert!(
        sent.contains("CANARY-AirQualityObserved-temperature")
            && sent.contains("CANARY-Device-battery"),
        "the delivery carries nothing at all: {sent}"
    );
}

// ---------------------------------------------------------------------------------------------
// T-1890, EP-26, MP-02: the edge cases of `egress::notifications::deliver`.
//
// The contract, in one sentence: a delivery is forwarded only to the target the STORED
// subscription names, carrying only what this endpoint serves for each entity's own type, and a
// body the handler cannot make sense of is refused rather than passed on.
//
// The tests above cover the happy path, the narrowed projection, a hidden attribute, an entity
// that stopped matching, a geographic grant, a subscription this gateway did not route, and a
// target taken from the store rather than the request. These are the shapes a forged delivery
// uses.
// ---------------------------------------------------------------------------------------------

/// A delivery whose body the test writes itself, so a body that is not a notification can be sent.
async fn deliver_raw(
    subscription: Option<Value>,
    matching: Vec<String>,
    body: String,
    slug: &str,
    to: &str,
) -> (StatusCode, Vec<Value>) {
    let (upstream, _) = broker(BrokerState {
        stored: subscription,
        matching,
        forwarded: Arc::new(Mutex::new(Vec::new())),
    })
    .await;

    let response = router(gateway_with(upstream, &[], None, None, policy()))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/endpoint/{slug}/egress/notifications?to={}",
                    percent(to)
                ))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    (response.status(), Vec::new())
}

#[tokio::test]
async fn a_body_that_is_not_a_notification_is_refused_and_nothing_is_delivered() {
    let (webhook, seen, _) = sink().await;
    let subscription = stored(&webhook, json!(["temperature"]), "((temperature<100))");

    for body in [
        String::new(),
        "not json".to_owned(),
        "null".to_owned(),
        "[]".to_owned(),
        "\"a string\"".to_owned(),
        json!({ "type": "Notification" }).to_string(),
        json!({ "subscriptionId": 7 }).to_string(),
        json!({ "subscriptionId": null }).to_string(),
        json!({ "subscriptionId": ["urn:ngsi-ld:Subscription:1"] }).to_string(),
        // `"subscriptionId": ""` is left out on purpose: the fixture broker answers the stored
        // subscription for any id, so this file cannot say what a real broker does with an empty
        // one (it answers 404). Nothing about the handler is proved either way by asserting it.
    ] {
        let (status, _) = deliver_raw(
            Some(subscription.clone()),
            vec![SENSOR.to_owned()],
            body.clone(),
            SLUG,
            &webhook,
        )
        .await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
            "{body:?} answered {status}",
        );
    }

    assert!(
        seen.lock().expect("the delivery log").is_empty(),
        "nothing a forged body carries reaches the subscriber",
    );
}

#[tokio::test]
async fn a_delivery_to_a_slug_this_gateway_does_not_serve_is_not_found() {
    let (webhook, seen, _) = sink().await;
    let subscription = stored(&webhook, json!(["temperature"]), "((temperature<100))");

    // An unknown slug, the slug of another shape, and the slug with the decorations a prober tries.
    for slug in [
        "not-a-slug",
        "",
        &SLUG.to_uppercase(),
        &format!("{SLUG}x"),
        &format!("{SLUG}%20"),
        &format!("{SLUG}.."),
    ] {
        let (status, _) = deliver_raw(
            Some(subscription.clone()),
            vec![SENSOR.to_owned()],
            notification(vec![sensor(SENSOR)]).to_string(),
            slug,
            &webhook,
        )
        .await;
        assert_ne!(
            status,
            StatusCode::NO_CONTENT,
            "slug {slug:?} delivered something"
        );
    }

    assert!(
        seen.lock().expect("the delivery log").is_empty(),
        "a slug that is not this endpoint's delivers nothing",
    );
}
#[tokio::test]
async fn an_entity_of_a_type_this_endpoint_does_not_serve_is_never_delivered() {
    // The projection is per type (T-1862): a forged delivery that names the stored subscription
    // but carries an entity of another type must not reach the subscriber, whatever shape `data`
    // has — an array, a bare object, or an array holding both.
    let (webhook, seen, _) = sink().await;
    let subscription = stored(&webhook, json!(["temperature"]), "((temperature<100))");
    let foreign = json!({
        "id": "urn:ngsi-ld:Person:hel:helsinki:1",
        "type": "Person",
        "nationalId": { "type": "Property", "value": "010101/0000" }
    });

    for data in [
        json!([foreign.clone()]),
        json!(foreign.clone()),
        json!([sensor(SENSOR), foreign.clone()]),
    ] {
        let body = json!({
            "id": "urn:ngsi-ld:Notification:1",
            "type": "Notification",
            "subscriptionId": SUBSCRIPTION,
            "notifiedAt": "2026-09-07T08:00:00Z",
            "data": data
        });
        let (_, _) = deliver_raw(
            Some(subscription.clone()),
            vec![SENSOR.to_owned()],
            body.to_string(),
            SLUG,
            &webhook,
        )
        .await;
    }

    let delivered = seen.lock().expect("the delivery log").clone();
    let text = serde_json::to_string(&delivered).expect("the log serialises");
    assert!(
        !text.contains("Person") && !text.contains("nationalId") && !text.contains("010101/0000"),
        "an entity of a type this endpoint does not serve reached the subscriber: {text}",
    );
}
