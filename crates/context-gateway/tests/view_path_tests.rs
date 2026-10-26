//! The NGSI-LD surface of an Endpoint is the broker's view of it (T-3643, plan 5.1 and 5.2).
//!
//! `/api/endpoint/{slug}/ngsi-ld/v1{rest}` goes to `/x/views/{slug}/ngsi-ld/v1{rest}` and
//! `/cs/{space}/ngsi-ld/v1{rest}` to `/x/views/cs-{space}/ngsi-ld/v1{rest}`, with the caller's
//! query as sent but for the two caps, the tenant and the verified caller pinned. The broker's
//! engine decides; these tests hold what the gateway still does around that hop, and which
//! requests stay on the gateway's own pipeline because no view serves them yet.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use context_gateway::view_mapping::ViewMapping;
use jc_core::kinds::{Audience, Creates, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

mod common;

/// The realm every signed-in caller of these tests comes from.
static REALM: std::sync::LazyLock<common::Realm> = std::sync::LazyLock::new(common::Realm::new);

/// `request` as a signed-in person sends it: only a caller the platform can name registers or
/// changes a subscription (F-BNT1-003).
fn signed(mut request: Request<Body>) -> Request<Body> {
    let token = REALM.mint(&json!({
        "iss": common::ISSUER,
        "sub": "sub-jana",
        "aud": [SLUG, SPACE],
        "preferred_username": "jana",
        "exp": common::in_seconds(300),
        "iat": common::in_seconds(-10),
    }));
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {token}").parse().expect("a header value"),
    );
    request
}

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const STATION: &str = "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:st-1";

/// One hop as the broker received it.
#[derive(Clone)]
struct Hop {
    method: Method,
    path: String,
    query: Option<String>,
    headers: HeaderMap,
    body: Value,
}

type Hops = Arc<Mutex<Vec<Hop>>>;

/// A broker that records every hop and answers each with `status`, `headers` and `body`.
async fn broker(
    status: StatusCode,
    headers: &[(&'static str, &'static str)],
    body: Value,
) -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let headers: Vec<(&'static str, &'static str)> = headers.to_vec();
    let app = Router::new().fallback(move |request: Request<Body>| {
        let (recorder, headers, body) = (Arc::clone(&recorder), headers.clone(), body.clone());
        async move {
            let (parts, sent) = request.into_parts();
            let sent = axum::body::to_bytes(sent, usize::MAX)
                .await
                .expect("a body");
            recorder.lock().expect("the hop log").push(Hop {
                method: parts.method.clone(),
                path: parts.uri.path().to_owned(),
                query: parts.uri.query().map(str::to_owned),
                headers: parts.headers.clone(),
                body: serde_json::from_slice(&sent).unwrap_or(Value::Null),
            });
            let mut answer = (status, axum::Json(body)).into_response();
            for (name, value) in headers {
                answer
                    .headers_mut()
                    .insert(name, value.parse().expect("a header value"));
            }
            answer
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("an address"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, hops)
}

fn grant() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\nassigner: did:web:{DOMAIN}\nassignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, createEntity, createSubscription]\n\
         information:\n  - entities: [{{ type: AirQualityObserved }}]\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
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
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant()],
    }
}

fn app(upstream: String, endpoint: Endpoint) -> Router {
    let space = Space {
        endpoint: Arc::new(Endpoint {
            slug: SPACE.to_owned(),
            base_path: format!("/cs/{SPACE}"),
            ..endpoint.clone()
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    };
    let gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
        .serve([endpoint])
        .serve_spaces([space])
        .authenticate(Arc::new(REALM.verifier()), ServiceAccounts::new(), None);
    router(Arc::new(gateway))
}

async fn call(app: Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.oneshot(request).await.expect("the gateway answers");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("cookie", "session=not-for-the-broker")
        .body(Body::empty())
        .expect("a request")
}

fn post(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("a request")
}

fn station() -> Value {
    json!({ "id": STATION, "type": "AirQualityObserved",
            "pm10": { "type": "Property", "value": 1.0 } })
}

/// Plan 5.1: the slug's view, the caller's query byte for byte, the tenant and the caller
/// pinned, and not the caller's cookie (K36; the token, `subject_headers_tests`).
#[tokio::test]
async fn a_read_through_the_slug_goes_to_its_view_as_sent() {
    let (upstream, hops) = broker(StatusCode::OK, &[], json!([])).await;
    let raw = "type=AirQualityObserved&q=pm10%3E1;pm25%3C9&attrs=pm10,pm25&limit=7";
    let (status, _, _) = call(
        app(upstream, endpoint()),
        get(&format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities?{raw}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hops = hops.lock().expect("the hop log").clone();
    assert_eq!(hops.len(), 1);
    let hop = &hops[0];
    assert_eq!(hop.method, Method::GET);
    assert_eq!(hop.path, format!("/x/views/{SLUG}/ngsi-ld/v1/entities"));
    assert_eq!(hop.query.as_deref(), Some(raw));
    assert_eq!(hop.headers["ngsild-tenant"], SPACE);
    assert_eq!(hop.headers["x-jc-roles"], "public");
    assert!(hop.headers.get("cookie").is_none());
}

/// Plan 5.1: the space surface is the `cs-{space}` view.
#[tokio::test]
async fn a_read_through_the_space_goes_to_the_space_view() {
    let (upstream, hops) = broker(StatusCode::OK, &[], json!({ "id": STATION })).await;
    let (status, _, body) = call(
        app(upstream, endpoint()),
        get(&format!("/cs/{SPACE}/ngsi-ld/v1/entities/{STATION}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = hops.lock().expect("the hop log").clone();
    assert_eq!(
        hops[0].path,
        format!("/x/views/cs-{SPACE}/ngsi-ld/v1/entities/{STATION}")
    );
}

/// GW26, AG-25: the two costs a request puts on the shared broker stay capped on the view hop;
/// every other pair goes on untouched.
#[tokio::test]
async fn last_n_and_join_level_are_capped_and_nothing_else_is_touched() {
    let (upstream, hops) = broker(StatusCode::OK, &[], json!([])).await;
    call(
        app(upstream, endpoint()),
        get(&format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved&join=inline&joinLevel=9&lastN=5000&x=lastN%3D9"
        )),
    )
    .await;
    let hops = hops.lock().expect("the hop log").clone();
    assert_eq!(
        hops[0].query.as_deref(),
        Some("type=AirQualityObserved&join=inline&joinLevel=3&lastN=1000&x=lastN%3D9")
    );
}

/// An Endpoint with a view mapping (G18) is the broker's view too: the Portal writes the
/// Mapping IR into the view of its slug (T-3671), the broker inverts each read and rebuilds the
/// answer in the target model (T-3669, T-3670), and refuses a write with 405 (`mapping::READS`).
/// The gateway forwards both as sent and judges no body against the source model.
#[tokio::test]
async fn a_mapped_endpoint_is_served_by_its_view() {
    let mapping = ViewMapping::parse(&json!({
        "version": 2,
        "sourceClass": "Senzor",
        "targetClass": "AirQualityObserved",
        "slots": [{ "target": "pm10", "source": "pm10", "kind": "rename", "filterable": true }]
    }))
    .expect("the IR parses");
    let mapped = Endpoint {
        view_mapping: Some(Arc::new(mapping)),
        ..endpoint()
    };
    let (upstream, hops) = broker(StatusCode::OK, &[], json!([])).await;
    let (status, _, _) = call(
        app(upstream, mapped.clone()),
        get(&format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved&q=pm10%3E2"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (upstream, writes) = broker(
        StatusCode::METHOD_NOT_ALLOWED,
        &[("allow", "GET")],
        json!({ "type": "https://uri.etsi.org/ngsi-ld/errors/OperationNotSupported" }),
    )
    .await;
    let (status, headers, _) = call(
        app(upstream, mapped),
        post(
            &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities"),
            json!({ "id": "urn:ngsi-ld:Vehicle:x", "type": "Vehicle" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers["allow"], "GET");

    let read = hops.lock().expect("the hop log").clone();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].path, format!("/x/views/{SLUG}/ngsi-ld/v1/entities"));
    assert_eq!(
        read[0].query.as_deref(),
        Some("type=AirQualityObserved&q=pm10%3E2")
    );
    let write = writes.lock().expect("the hop log").clone();
    assert_eq!(
        write.len(),
        1,
        "the write reached the view, judged by the broker"
    );
    assert_eq!(
        (write[0].method.clone(), write[0].path.as_str()),
        (
            Method::POST,
            format!("/x/views/{SLUG}/ngsi-ld/v1/entities").as_str()
        )
    );
}

/// T-3644: the view decides every NGSI-LD request. An Endpoint whose slug no view id can hold
/// is refused on that surface, a subscription and a read alike, and nothing reaches the broker
/// around a view; there is no gateway pipeline left to answer it.
#[tokio::test]
async fn an_endpoint_no_view_can_hold_is_refused_and_nothing_reaches_the_broker() {
    let (upstream, hops) = broker(StatusCode::CREATED, &[], json!({})).await;
    let long = "s".repeat(64);
    let unviewable = Endpoint {
        slug: long.clone(),
        base_path: format!("/api/endpoint/{long}"),
        ..endpoint()
    };
    for path in ["subscriptions", "entities?type=AirQualityObserved"] {
        let (status, _, body) = call(
            app(upstream.clone(), unviewable.clone()),
            get(&format!("/api/endpoint/{long}/ngsi-ld/v1/{path}")),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
        assert!(body.to_string().contains("no-view"), "{path}: {body}");
    }
    assert!(
        hops.lock().expect("the hop log").is_empty(),
        "a request reached the broker around its view"
    );
}

/// T-3644 with T-3789 (R45, GW19): a conditional write is the view's like any other; the broker
/// compares the entity tag, so `If-Match` reaches it as the caller sent it.
#[tokio::test]
async fn a_conditional_write_goes_to_the_view_with_its_if_match() {
    let (upstream, hops) = broker(StatusCode::NO_CONTENT, &[], json!({})).await;
    let mut conditional = signed(post(
        &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/subscriptions/urn:ngsi-ld:Subscription:air-1"),
        json!({ "notification": { "endpoint": { "uri": "https://hooks.example/air" } } }),
    ));
    *conditional.method_mut() = Method::PATCH;
    conditional
        .headers_mut()
        .insert("if-match", "\"1\"".parse().expect("a header value"));
    let (status, _, body) = call(app(upstream, endpoint()), conditional).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let sent = hops.lock().expect("the hop log").clone();
    assert_eq!(sent.len(), 1, "one hop to the view");
    assert_eq!(
        sent[0].path,
        format!("/x/views/{SLUG}/ngsi-ld/v1/subscriptions/urn:ngsi-ld:Subscription:air-1")
    );
    assert_eq!(sent[0].headers["if-match"], "\"1\"");
}

fn subscription(uri: &str) -> Value {
    json!({
        "id": "urn:ngsi-ld:Subscription:air-1",
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": uri, "accept": "application/json" } },
    })
}

/// T-3653, plan 5.5: a subscription is the view's. It reaches the view as sent, its subscriber's
/// own address untouched and no route through the gateway written into it, under the verified
/// subscriber, so the broker delivers to it directly under `pre_notify`. It is no entity: no
/// model check refuses its type.
#[tokio::test]
async fn a_subscription_goes_to_the_view_with_its_own_target() {
    let (upstream, hops) = broker(StatusCode::CREATED, &[], json!({})).await;
    let sent = subscription("https://hooks.example/air");
    let (status, _, body) = call(
        app(upstream.clone(), endpoint()),
        signed(post(
            &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/subscriptions"),
            sent.clone(),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for (method, rest) in [
        (Method::GET, "/subscriptions"),
        (Method::GET, "/subscriptions/urn:ngsi-ld:Subscription:air-1"),
        (
            Method::DELETE,
            "/subscriptions/urn:ngsi-ld:Subscription:air-1",
        ),
    ] {
        let mut request = signed(get(&format!("/api/endpoint/{SLUG}/ngsi-ld/v1{rest}")));
        *request.method_mut() = method;
        call(app(upstream.clone(), endpoint()), request).await;
    }

    let hops = hops.lock().expect("the hop log").clone();
    let paths: Vec<&str> = hops.iter().map(|hop| hop.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            format!("/x/views/{SLUG}/ngsi-ld/v1/subscriptions"),
            format!("/x/views/{SLUG}/ngsi-ld/v1/subscriptions"),
            format!("/x/views/{SLUG}/ngsi-ld/v1/subscriptions/urn:ngsi-ld:Subscription:air-1"),
            format!("/x/views/{SLUG}/ngsi-ld/v1/subscriptions/urn:ngsi-ld:Subscription:air-1"),
        ]
    );
    assert_eq!(
        hops[0].body, sent,
        "the subscription reached the view as sent"
    );
    // The anonymous caller of this public Endpoint is its `public` role: the subscriber the
    // broker stores and decides each notification under (T-3499).
    assert_eq!(
        hops[0]
            .headers
            .get("x-jc-roles")
            .and_then(|v| v.to_str().ok()),
        Some("public"),
        "the subscriber is pinned for the broker to store"
    );
}

/// F-BNT1-003 (User-Guide/05): an anonymous caller registers, changes and removes no
/// subscription, even where a public Policy grants `createSubscription`: 401 before the view, on
/// the slug and on the space. A signed-in caller under the same grant passes (above).
#[tokio::test]
async fn an_anonymous_caller_changes_no_subscription() {
    let (upstream, hops) = broker(StatusCode::CREATED, &[], json!({})).await;
    let one = "subscriptions/urn:ngsi-ld:Subscription:air-1";
    for prefix in [format!("/api/endpoint/{SLUG}"), format!("/cs/{SPACE}")] {
        for (method, rest) in [
            (Method::POST, "subscriptions"),
            (Method::PATCH, one),
            (Method::DELETE, one),
        ] {
            let mut request = post(
                &format!("{prefix}/ngsi-ld/v1/{rest}"),
                subscription("https://hooks.example/air"),
            );
            *request.method_mut() = method.clone();
            let (status, headers, body) = call(app(upstream.clone(), endpoint()), request).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {prefix}/{rest}: {body}"
            );
            assert_eq!(headers["www-authenticate"], "Bearer");
        }
    }
    assert!(
        hops.lock().expect("the hop log").is_empty(),
        "an anonymous subscription reached the broker"
    );
}

/// G11, T-3653: the broker delivers to whatever a stored subscription names, so a target inside
/// the platform's networks is refused before the view sees it, in every spelling the delivering
/// client would dial, on a create and on a PATCH; and so is anything but HTTP(S).
#[tokio::test]
async fn a_subscription_aimed_inside_the_platform_never_reaches_the_view() {
    let (upstream, hops) = broker(StatusCode::CREATED, &[], json!({})).await;
    for uri in [
        "http://127.0.0.1?x",
        "http://0x7f.1/",
        "http://2130706433:9090/q/admin",
        "http://localhost./",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/",
        "http://10.43.0.10/",
        "mqtt://broker.example/air",
    ] {
        let (status, _, body) = call(
            app(upstream.clone(), endpoint()),
            signed(post(
                &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/subscriptions"),
                subscription(uri),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");

        let mut patch = signed(post(
            &format!(
                "/api/endpoint/{SLUG}/ngsi-ld/v1/subscriptions/urn:ngsi-ld:Subscription:air-1"
            ),
            json!({ "notification": { "endpoint": { "uri": uri } } }),
        ));
        *patch.method_mut() = Method::PATCH;
        let (status, _, body) = call(app(upstream.clone(), endpoint()), patch).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "PATCH {uri}: {body}");
    }
    assert!(hops.lock().expect("the hop log").is_empty());
}

/// T-1302 kept: a host the installation names in `JC_GATEWAY_EGRESS_PRIVATE_HOSTS` may be a
/// subscriber inside the cluster.
#[tokio::test]
async fn a_named_private_host_may_be_subscribed() {
    let (upstream, hops) = broker(StatusCode::CREATED, &[], json!({})).await;
    let space = Space {
        endpoint: Arc::new(Endpoint {
            slug: SPACE.to_owned(),
            base_path: format!("/cs/{SPACE}"),
            ..endpoint()
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    };
    let gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
        .serve([endpoint()])
        .serve_spaces([space])
        .deliver_privately_to(vec!["receiver.cluster.local".to_owned()])
        .authenticate(Arc::new(REALM.verifier()), ServiceAccounts::new(), None);
    let (status, _, body) = call(
        router(Arc::new(gateway)),
        signed(post(
            &format!("/cs/{SPACE}/ngsi-ld/v1/subscriptions"),
            subscription("http://receiver.cluster.local/air"),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let hops = hops.lock().expect("the hop log");
    assert_eq!(hops.len(), 1);
    assert_eq!(
        hops[0].path,
        format!("/x/views/cs-{SPACE}/ngsi-ld/v1/subscriptions")
    );
}

/// T-3801 (G6, T-3632): an Endpoint whose projection has a `temporalQ` is served by its view,
/// whose permits carry the window (T-3812), a temporal read as sent and nothing clamped here.
#[tokio::test]
async fn a_projection_with_a_temporal_q_is_served_by_its_view() {
    let (upstream, hops) = broker(StatusCode::OK, &[], json!([])).await;
    let projection = serde_json::from_value(json!({
        "contextSpaceRef": SPACE, "dataModelRef": {"name": "air", "version": "1.0.0"},
        "classes": [{"name": "AirQualityObserved"}],
        "filter": {"temporalQ": "timerel=after;timeAt=P-1D"}
    }))
    .expect("a projection");
    let windowed = Endpoint {
        projection: Some(Arc::new(projection)),
        ..endpoint()
    };
    let raw = "type=AirQualityObserved&timerel=after&timeAt=2020-01-01T00:00:00Z";
    let (status, _, _) = call(
        app(upstream, windowed),
        get(&format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/temporal/entities?{raw}"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hops = hops.lock().expect("the hop log").clone();
    let sent: Vec<(&str, Option<&str>)> = hops
        .iter()
        .map(|hop| (hop.path.as_str(), hop.query.as_deref()))
        .collect();
    assert_eq!(
        sent,
        [(
            format!("/x/views/{SLUG}/ngsi-ld/v1/temporal/entities").as_str(),
            Some(raw)
        )]
    );
}

/// R20, T-2260: what the broker prints when it fails stays with the gateway.
#[tokio::test]
async fn a_broker_failure_behind_the_view_is_the_gateways_own_answer() {
    let (upstream, _) = broker(
        StatusCode::INTERNAL_SERVER_ERROR,
        &[],
        json!({ "detail": "postgres://antares:hunter2@db/antares failed" }),
    )
    .await;
    let (status, _, body) = call(
        app(upstream, endpoint()),
        get(&format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{STATION}"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !body.contains("hunter2") && !body.contains("postgres"),
        "{body}"
    );
}

/// T-4438: a broker that is down or overloaded is not an `@context` that could not be fetched.
/// The caller gets the gateway's own 503 with the broker's Retry-After and no broker host.
#[tokio::test]
async fn a_broker_503_is_no_ld_context_problem_and_keeps_its_retry_after() {
    let (upstream, _) = broker(
        StatusCode::SERVICE_UNAVAILABLE,
        &[("retry-after", "7")],
        json!({ "detail": "antares-broker.antares.svc:8080 is draining" }),
    )
    .await;
    let (status, headers, body) = call(
        app(upstream, endpoint()),
        get(&format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{STATION}"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        headers.get("retry-after").map(|v| v.as_bytes()),
        Some(&b"7"[..])
    );
    let problem: Value = serde_json::from_str(&body).expect("a problem document");
    assert_ne!(
        problem["type"], "https://uri.etsi.org/ngsi-ld/errors/LdContextNotAvailable",
        "{body}"
    );
    assert!(
        !body.contains("antares-broker") && !body.contains("svc"),
        "{body}"
    );
}

/// EP-22, R20: a space nobody has written to has no tenant at the broker. A listing answers
/// empty and a read of one thing the plain miss, and neither names the tenant.
#[tokio::test]
async fn an_empty_space_answers_empty_and_never_names_its_tenant() {
    let nonexistent = json!({
        "type": "https://uri.etsi.org/ngsi-ld/errors/NonexistentTenant",
        "title": "NonexistentTenant",
        "status": 404,
        "detail": format!("tenant {SPACE} does not exist"),
    });
    let (upstream, _) = broker(StatusCode::NOT_FOUND, &[], nonexistent).await;
    let (status, _, body) = call(
        app(upstream.clone(), endpoint()),
        get(&format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
        )),
    )
    .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));
    let (status, _, body) = call(
        app(upstream, endpoint()),
        get(&format!(
            "/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{STATION}"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains(SPACE) && !body.contains("Tenant"), "{body}");
}

/// Views.md 2: the broker names its view root in `Location` and `Link`; the caller gets their
/// own prefix and never the internal mount.
#[tokio::test]
async fn the_view_root_in_location_and_link_is_the_callers_prefix() {
    let root = "/x/views/k4y7pq2mzt6vhx3nbwrs5cjd8f/ngsi-ld/v1";
    let location: &'static str = Box::leak(format!("{root}/entities/{STATION}").into_boxed_str());
    let link: &'static str = Box::leak(
        format!("<{root}/entities?type=AirQualityObserved&offset=20>; rel=\"next\"")
            .into_boxed_str(),
    );
    let (upstream, _) = broker(
        StatusCode::CREATED,
        &[("location", location), ("link", link)],
        json!({}),
    )
    .await;
    let (status, headers, _) = call(
        app(upstream, endpoint()),
        post(
            &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities"),
            station(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let prefix = format!("/api/endpoint/{SLUG}/ngsi-ld/v1");
    assert_eq!(
        headers["location"],
        format!("{prefix}/entities/{STATION}").as_str()
    );
    let links: Vec<&str> = headers
        .get_all("link")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    assert!(
        links.contains(
            &format!("<{prefix}/entities?type=AirQualityObserved&offset=20>; rel=\"next\"")
                .as_str()
        ),
        "{links:?}"
    );
    assert!(
        links.iter().all(|link| !link.contains("/x/views")),
        "{links:?}"
    );
}

/// EP-39: a form's daily creates are counted on the view hop, and a create the broker refuses
/// spends none of them.
#[tokio::test]
async fn a_create_the_broker_refuses_spends_no_daily_create() {
    let form = Endpoint {
        creates: Some(Creates {
            per_day: Some(1),
            ..Default::default()
        }),
        ..endpoint()
    };
    let (refusing, _) = broker(StatusCode::FORBIDDEN, &[], json!({ "status": 403 })).await;
    let (accepting, _) = broker(StatusCode::CREATED, &[], json!({})).await;
    let refused = app(refusing, form.clone());
    let uri = format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities");
    for _ in 0..3 {
        let (status, _, _) = call(refused.clone(), post(&uri, station())).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the cap was spent by a refusal"
        );
    }
    let accepted = app(accepting, form);
    let (status, _, _) = call(accepted.clone(), post(&uri, station())).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, headers, _) = call(accepted, post(&uri, station())).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key("retry-after"));
}

/// O-8, API/00 §4: the engine's refusal keeps the broker's type, and carries the platform's slug
/// as `code` with its hint, so the Portal says who can give access as it did before T-3643.
#[tokio::test]
async fn the_engines_refusal_carries_the_platforms_slug_and_hint() {
    let denied = json!({
        "type": "urn:antares:error:AccessDenied",
        "title": "AccessDenied",
        "status": 403,
        "detail": "no policy grants this operation",
    });
    let (upstream, _) = broker(StatusCode::FORBIDDEN, &[], denied).await;
    let (status, _, body) = call(
        app(upstream, endpoint()),
        post(
            &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities"),
            station(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let problem: Value = serde_json::from_str(&body).expect("a problem");
    assert_eq!(problem["type"], json!("urn:antares:error:AccessDenied"));
    assert_eq!(problem["code"], json!("forbidden"));
    assert_eq!(
        problem["hint"],
        json!("Ask a project owner or an organization administrator for the role this needs.")
    );
}

/// T-3651: the file translators read the slug's view with the caller pinned
/// and their own selector as sent, and an Endpoint no view serves (a mapped one, G18) still
/// reads the plain root through the gateway's PDP, with the grant's type folded in.
#[tokio::test]
async fn the_translators_read_the_view_and_what_no_view_serves_reads_the_plain_root() {
    let translated = || Endpoint {
        representations: vec![Representation::NgsiLd, Representation::Csv],
        ..endpoint()
    };
    // An entity with a place, so the GeoJSON download holds it.
    let located = json!([{ "id": STATION, "type": "AirQualityObserved",
        "pm10": { "type": "Property", "value": 1.0 },
        "location": { "type": "GeoProperty",
                      "value": { "type": "Point", "coordinates": [19.1, 48.7] } } }]);
    // Each names its selector: with none the type list is asked first, which this broker does
    // not answer (that hop, `dataset_download_tests`).
    let rest = "file.csv?type=AirQualityObserved";
    let (upstream, hops) = broker(StatusCode::OK, &[], located.clone()).await;
    let (status, _, body) = call(
        app(upstream, translated()),
        get(&format!("/api/endpoint/{SLUG}/{rest}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rest}: {body}");
    assert!(body.contains("st-1"), "{rest}: {body}");
    let hops = hops.lock().expect("the hop log").clone();
    let entity_reads: Vec<&Hop> = hops
        .iter()
        .filter(|hop| hop.path.ends_with("/entities"))
        .collect();
    assert!(!entity_reads.is_empty(), "{rest}: no entity read");
    for hop in entity_reads {
        assert_eq!(
            hop.path,
            format!("/x/views/{SLUG}/ngsi-ld/v1/entities"),
            "{rest}"
        );
        assert_eq!(
            hop.headers.get("x-jc-roles").and_then(|v| v.to_str().ok()),
            Some("public"),
            "{rest}: the caller is pinned"
        );
        assert!(hop.headers.get("cookie").is_none(), "{rest}");
    }

    let mapping = ViewMapping::parse(&json!({
        "version": 2,
        "sourceClass": "AirQualityObserved",
        "targetClass": "AirQualityObserved",
        "slots": [{ "target": "pm10", "source": "pm10", "kind": "rename", "filterable": true }]
    }))
    .expect("the IR parses");
    let mapped = Endpoint {
        view_mapping: Some(Arc::new(mapping)),
        ..translated()
    };
    let (upstream, hops) = broker(StatusCode::OK, &[], json!([station()])).await;
    let (status, _, body) = call(
        app(upstream, mapped),
        get(&format!(
            "/api/endpoint/{SLUG}/file.csv?type=AirQualityObserved"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let paths: Vec<String> = hops
        .lock()
        .expect("the hop log")
        .iter()
        .map(|hop| hop.path.clone())
        .collect();
    assert!(
        paths.contains(&format!("/x/views/{SLUG}/ngsi-ld/v1/entities")),
        "{paths:?}"
    );
    assert!(
        paths.iter().all(|path| !path.starts_with("/ngsi-ld/")),
        "a mapped Endpoint never reads the plain root: {paths:?}"
    );
}
