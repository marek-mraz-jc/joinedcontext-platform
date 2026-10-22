//! The SensorThings API surface of an Endpoint is the broker's `antares-sta` surface of its view
//! (T-4463, EP-12, EP-13), the same forward as OGC's.
//!
//! `/api/endpoint/{slug}/sta/v1.1{rest}` goes to `/x/views/{slug}/sta/v1.1{rest}` with the
//! caller's query as sent, the tenant and the verified caller pinned. The broker answers the sets,
//! the `$filter` and the decision; these tests hold what the gateway does around that hop:
//! admission, read-only, the links moved under the caller's URL so no answer names the broker
//! (T-4403), a broker failure in the gateway's own words, and the `501` for an Endpoint no view
//! serves.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use context_gateway::view_mapping::ViewMapping;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";

/// One hop as the broker received it.
#[derive(Clone)]
struct Hop {
    method: Method,
    path: String,
    query: Option<String>,
    headers: HeaderMap,
}

type Hops = Arc<Mutex<Vec<Hop>>>;

/// A broker that records every hop and answers `status` with a page naming itself by its
/// `Host`, as Antares' `public_base` does.
async fn broker(
    status: StatusCode,
    extra: &'static [(&'static str, &'static str)],
) -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&recorder);
        async move {
            let host = request.headers()["host"]
                .to_str()
                .expect("a host")
                .to_owned();
            recorder.lock().expect("the hop log").push(Hop {
                method: request.method().clone(),
                path: request.uri().path().to_owned(),
                query: request.uri().query().map(str::to_owned),
                headers: request.headers().clone(),
            });
            let root = format!("http://{host}/x/views/{SLUG}/sta/v1.1");
            let thing = format!("{root}/Things('urn:ngsi-ld:Car:1')");
            let body = json!({
                "value": [{ "@iot.id": "urn:ngsi-ld:Car:1", "@iot.selfLink": thing,
                            "Datastreams@iot.navigationLink": format!("{thing}/Datastreams") }],
                "@iot.nextLink": format!("{root}/Things?$skip=1"),
                "detail": "postgres://broker:hunter2@db/antares"
            });
            let mut answer = (status, axum::Json(body)).into_response();
            for (name, value) in extra {
                answer
                    .headers_mut()
                    .insert(*name, value.parse().expect("a header value"));
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

fn endpoint() -> Endpoint {
    let grant: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\nassigner: did:web:{DOMAIN}\nassignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity]\n\
         information:\n  - entities: [{{ type: Car }}]\n"
    ))
    .expect("the policy spec parses");
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
        representations: vec![Representation::Sta],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant],
    }
}

async fn call(
    upstream: &str,
    endpoint: Endpoint,
    request: Request<Body>,
) -> (StatusCode, HeaderMap, String) {
    let gateway =
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint]);
    let response = router(Arc::new(gateway))
        .oneshot(request)
        .await
        .expect("the gateway answers");
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

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", "session=not-for-the-broker")
        .body(Body::empty())
        .expect("a request")
}

fn get(uri: &str) -> Request<Body> {
    request(Method::GET, uri)
}

fn hops_of(hops: &Hops) -> Vec<Hop> {
    hops.lock().expect("the hop log").clone()
}

/// EP-12: the slug's view, the path and query byte for byte (a quoted `$filter` and a key holding
/// an encoded slash included), the tenant and the caller pinned, not the caller's cookie; every
/// link the broker wrote comes back under the Endpoint's URL and none names the broker.
#[tokio::test]
async fn a_read_goes_to_the_views_sta_surface_as_sent() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let raw = "$filter=name%20eq%20%27%C5%BDilina%27&$top=1";
    let (status, _, body) = call(
        &upstream,
        endpoint(),
        get(&format!("/api/endpoint/{SLUG}/sta/v1.1/Things?{raw}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = hops_of(&hops);
    assert_eq!(hops.len(), 1);
    let hop = &hops[0];
    assert_eq!(hop.method, Method::GET);
    assert_eq!(hop.path, format!("/x/views/{SLUG}/sta/v1.1/Things"));
    assert_eq!(hop.query.as_deref(), Some(raw));
    assert_eq!(hop.headers["ngsild-tenant"], SPACE);
    assert_eq!(hop.headers["x-jc-roles"], "public");
    assert!(hop.headers.get("cookie").is_none());

    let page: Value = serde_json::from_str(&body).expect("JSON");
    let thing = format!("/api/endpoint/{SLUG}/sta/v1.1/Things('urn:ngsi-ld:Car:1')");
    assert_eq!(page["value"][0]["@iot.selfLink"], thing.as_str());
    assert_eq!(
        page["value"][0]["Datastreams@iot.navigationLink"],
        format!("{thing}/Datastreams")
    );
    assert_eq!(
        page["@iot.nextLink"],
        format!("/api/endpoint/{SLUG}/sta/v1.1/Things?$skip=1")
    );
    assert!(
        !body.contains("/x/views/"),
        "the broker's mount leaked: {body}"
    );
    assert!(
        !body.contains("127.0.0.1"),
        "the broker's host leaked: {body}"
    );
}

/// The root in its spellings is the one broker root, and a Datastream's key goes on still
/// percent-encoded.
#[tokio::test]
async fn the_root_and_a_keyed_path_keep_their_paths() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let observations = "/Datastreams('urn:ngsi-ld:Car:1%2Fspeed')/Observations";
    for path in ["", "/", observations] {
        call(
            &upstream,
            endpoint(),
            get(&format!("/api/endpoint/{SLUG}/sta/v1.1{path}")),
        )
        .await;
    }
    let paths: Vec<String> = hops_of(&hops).into_iter().map(|hop| hop.path).collect();
    let root = format!("/x/views/{SLUG}/sta/v1.1");
    assert_eq!(
        paths,
        [root.clone(), root.clone(), format!("{root}{observations}")]
    );
}

/// EP-13: no write half; `OPTIONS` says so, and the broker is not asked.
#[tokio::test]
async fn a_write_is_refused_before_the_broker() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let things = format!("/api/endpoint/{SLUG}/sta/v1.1/Things");
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        let (status, headers, _) =
            call(&upstream, endpoint(), request(method.clone(), &things)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(headers["allow"], "GET, HEAD, OPTIONS");
    }
    let (status, _, _) = call(&upstream, endpoint(), request(Method::OPTIONS, &things)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(hops_of(&hops).is_empty());
}

/// An Endpoint that does not list SensorThings has no such surface, and the broker is not asked.
#[tokio::test]
async fn an_endpoint_without_sta_answers_none() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let without = Endpoint {
        representations: vec![Representation::NgsiLd],
        ..endpoint()
    };
    let (status, _, body) = call(
        &upstream,
        without,
        get(&format!("/api/endpoint/{SLUG}/sta/v1.1/Things")),
    )
    .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(hops_of(&hops).is_empty());
}

/// An Endpoint no view serves yet (a view of another model, G18) has no SensorThings surface: a
/// named `501`, and the broker is not asked.
#[tokio::test]
async fn an_endpoint_without_a_view_is_501() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let mapping = ViewMapping::parse(&json!({
        "version": 2,
        "sourceClass": "Senzor",
        "targetClass": "Car",
        "slots": [{ "target": "speed", "source": "rychlost", "kind": "rename", "filterable": true }]
    }))
    .expect("the IR parses");
    let mapped = Endpoint {
        view_mapping: Some(Arc::new(mapping)),
        ..endpoint()
    };
    let (status, _, body) = call(
        &upstream,
        mapped,
        get(&format!("/api/endpoint/{SLUG}/sta/v1.1/Things")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert!(body.contains("ogc-view-unavailable"), "{body}");
    assert!(body.contains("SensorThings API"), "{body}");
    assert!(hops_of(&hops).is_empty());
}

/// T-2260: a failing broker's words stay in the operator's log.
#[tokio::test]
async fn a_broker_failure_is_the_gateways_document() {
    let (upstream, _) = broker(StatusCode::INTERNAL_SERVER_ERROR, &[]).await;
    let (status, _, body) = call(
        &upstream,
        endpoint(),
        get(&format!("/api/endpoint/{SLUG}/sta/v1.1/Things")),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("broker-failure"), "{body}");
    assert!(!body.contains("hunter2"), "{body}");
}
