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
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Space};
use context_gateway::translators::view_mapping::ViewMapping;
use jc_core::kinds::{Audience, Creates, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

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
            recorder.lock().expect("the hop log").push(Hop {
                method: request.method().clone(),
                path: request.uri().path().to_owned(),
                query: request.uri().query().map(str::to_owned),
                headers: request.headers().clone(),
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
        policy_names: Vec::new(),
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
        .serve_spaces([space]);
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

/// What no view serves yet stays on the gateway's own pipeline: a conditional write (R45,
/// T-3789) and a subscription (5.5).
#[tokio::test]
async fn what_no_view_serves_stays_on_the_gateway() {
    let (upstream, hops) = broker(StatusCode::CREATED, &[], json!({})).await;
    let subscription = json!({
        "type": "Subscription",
        "entities": [{ "type": "AirQualityObserved" }],
        "notification": { "endpoint": { "uri": "https://hooks.example/air" } },
    });
    // The broker answers every hop 201; these two answers are the gateway's own pipeline's:
    // a subscription with no egress configured, a write the grant has no update for.
    let (status, _, _) = call(
        app(upstream.clone(), endpoint()),
        post(
            &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/subscriptions"),
            subscription,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    let mut conditional = post(
        &format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/{STATION}/attrs"),
        json!({ "pm10": { "type": "Property", "value": 2.0 } }),
    );
    *conditional.method_mut() = Method::PATCH;
    conditional
        .headers_mut()
        .insert("if-match", "\"1\"".parse().expect("a header value"));
    let (status, _, _) = call(app(upstream.clone(), endpoint()), conditional).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    assert!(hops.lock().expect("the hop log").is_empty());
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
