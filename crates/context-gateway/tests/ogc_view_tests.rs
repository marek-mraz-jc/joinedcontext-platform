//! The OGC API Features surface of an Endpoint is the broker's `antares-ogc` surface of its view
//! (T-3752, ADR-0029 decision 3, EP-29, EP-39).
//!
//! `/api/endpoint/{slug}/ogc/features{rest}` goes to `/x/views/{slug}/ogc/features{rest}` with
//! the caller's query as sent, the tenant and the verified caller pinned. The broker answers the
//! documents, the CQL2 filter and the decision; these tests hold what the gateway does around
//! that hop: admission, read-only, the links moved under the caller's URL, a broker failure in
//! the gateway's own words, and the `501` for an Endpoint no view serves.

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

/// A vector tile's first bytes: no UTF-8, and a broker URL inside its tag strings.
const TILE: &[u8] = b"\x1a\xff\xfe\x0a\x03Car http://127.0.0.1/x/views/";

/// A broker that records every hop and answers `status` with a page naming itself by its
/// `Host`, as Antares' `public_base` does, in the body and in a `Link` header.
async fn broker(
    status: StatusCode,
    extra: &'static [(&'static str, &'static str)],
) -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&recorder);
        async move {
            let host = request.headers()["host"].to_str().expect("a host").to_owned();
            recorder.lock().expect("the hop log").push(Hop {
                method: request.method().clone(),
                path: request.uri().path().to_owned(),
                query: request.uri().query().map(str::to_owned),
                headers: request.headers().clone(),
            });
            if request.uri().path().contains("/tiles/WebMercatorQuad/") {
                let tile = [("content-type", "application/vnd.mapbox-vector-tile"), ("etag", "W/\"t1\"")];
                return (status, tile, TILE.to_vec()).into_response();
            }
            let root = format!("http://{host}/x/views/{SLUG}/ogc/features");
            // Antares links a feature to its entity on the same view's NGSI-LD surface (T-4403).
            let entity = format!("http://{host}/x/views/{SLUG}/ngsi-ld/v1/entities/urn%3Angsi-ld%3ACar%3A1");
            let body = json!({
                "type": "FeatureCollection",
                "features": [{ "type": "Feature", "id": "urn:ngsi-ld:Car:1", "geometry": null, "properties": {},
                               "links": [{ "rel": "alternate", "href": entity }] }],
                "links": [{ "rel": "next", "href": format!("{root}/collections/Car/items?next=t1") }],
                "detail": "postgres://broker:hunter2@db/antares"
            });
            let mut answer = (status, axum::Json(body)).into_response();
            let link = format!("<{root}/collections>; rel=\"data\"");
            answer
                .headers_mut()
                .insert("link", link.parse().expect("a header value"));
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
        representations: vec![Representation::OgcFeatures],
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

/// EP-29: the slug's view, the path and query byte for byte (a CQL2 filter included), the tenant
/// and the caller pinned, not the caller's cookie; the broker's links come back under the
/// Endpoint's URL.
#[tokio::test]
async fn an_items_read_goes_to_the_views_ogc_surface_as_sent() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let raw = "filter=speed%20%3E%2020%20AND%20colour%20%3D%20%27red%27&limit=7";
    let (status, headers, body) = call(
        &upstream,
        endpoint(),
        get(&format!(
            "/api/endpoint/{SLUG}/ogc/features/collections/Car/items?{raw}"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let hops = hops_of(&hops);
    assert_eq!(hops.len(), 1);
    let hop = &hops[0];
    assert_eq!(hop.method, Method::GET);
    assert_eq!(
        hop.path,
        format!("/x/views/{SLUG}/ogc/features/collections/Car/items")
    );
    assert_eq!(hop.query.as_deref(), Some(raw));
    assert_eq!(hop.headers["ngsild-tenant"], SPACE);
    assert_eq!(hop.headers["x-jc-roles"], "public");
    assert!(hop.headers.get("cookie").is_none());

    let page: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(
        page["links"][0]["href"],
        format!("/api/endpoint/{SLUG}/ogc/features/collections/Car/items?next=t1")
    );
    assert_eq!(
        page["features"][0]["links"][0]["href"],
        format!("/api/endpoint/{SLUG}/ngsi-ld/v1/entities/urn%3Angsi-ld%3ACar%3A1"),
        "a feature's entity is the Endpoint's NGSI-LD surface (T-4403)"
    );
    assert!(
        !body.contains("/x/views/"),
        "the broker's mount leaked: {body}"
    );
    assert_eq!(
        headers["link"],
        format!("</api/endpoint/{SLUG}/ogc/features/collections>; rel=\"data\"").as_str()
    );
}

/// The three spellings of the landing page are the one broker landing page, and an item id goes
/// on still percent-encoded.
#[tokio::test]
async fn the_landing_page_and_an_item_keep_their_paths() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let item = "urn%3Angsi-ld%3ACar%3A1";
    for path in [
        String::new(),
        "/".to_owned(),
        format!("/collections/Car/items/{item}"),
    ] {
        call(
            &upstream,
            endpoint(),
            get(&format!("/api/endpoint/{SLUG}/ogc/features{path}")),
        )
        .await;
    }
    let paths: Vec<String> = hops_of(&hops).into_iter().map(|hop| hop.path).collect();
    let root = format!("/x/views/{SLUG}/ogc/features");
    assert_eq!(
        paths,
        [
            root.clone(),
            root.clone(),
            format!("{root}/collections/Car/items/{item}")
        ]
    );
}

/// EP-39: no write half, whatever the broker's view would allow; `OPTIONS` says so.
#[tokio::test]
async fn a_write_is_refused_before_the_broker() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let items = format!("/api/endpoint/{SLUG}/ogc/features/collections/Car/items");
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        let (status, headers, _) =
            call(&upstream, endpoint(), request(method.clone(), &items)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(headers["allow"], "GET, HEAD, OPTIONS");
    }
    let (status, _, _) = call(&upstream, endpoint(), request(Method::OPTIONS, &items)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(hops_of(&hops).is_empty());
}

/// An Endpoint no view serves yet (here: a view of another model, G18) has no OGC surface: a
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
        get(&format!("/api/endpoint/{SLUG}/ogc/features/collections")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert!(body.contains("ogc-view-unavailable"), "{body}");
    assert!(hops_of(&hops).is_empty());
}

/// T-2260: a failing broker's words stay in the operator's log.
#[tokio::test]
async fn a_broker_failure_is_the_gateways_document() {
    let (upstream, _) = broker(StatusCode::INTERNAL_SERVER_ERROR, &[]).await;
    let (status, _, body) = call(
        &upstream,
        endpoint(),
        get(&format!(
            "/api/endpoint/{SLUG}/ogc/features/collections/Car/items"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("broker-failure"), "{body}");
    assert!(!body.contains("hunter2"), "{body}");
}

/// R22: a caller who asks to be told about narrowing is told under the specification's name,
/// and the broker's own marker never reaches anyone.
#[tokio::test]
async fn a_narrowed_page_says_so_to_a_caller_who_asked() {
    let (upstream, hops) = broker(StatusCode::OK, &[("antares-results-restricted", "true")]).await;
    let items = format!("/api/endpoint/{SLUG}/ogc/features/collections/Car/items");
    let mut asked = get(&items);
    asked.headers_mut().insert(
        "ngsild-results-restricted",
        "true".parse().expect("a header value"),
    );
    let (_, headers, _) = call(&upstream, endpoint(), asked).await;
    assert!(headers.get("antares-results-restricted").is_none());
    assert_eq!(headers["ngsild-results-restricted"], "true");
    assert_eq!(hops_of(&hops)[0].headers["x-jc-restricted"], "true");

    let (_, headers, _) = call(&upstream, endpoint(), get(&items)).await;
    assert!(headers.get("antares-results-restricted").is_none());
    assert!(headers.get("ngsild-results-restricted").is_none());
}

/// T-5121: a vector tile is binary: the gateway passes its bytes and its cache headers as the
/// broker sent them, with no link rewrite that would corrupt the protobuf.
#[tokio::test]
async fn a_vector_tile_passes_byte_for_byte() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let gateway =
        Gateway::new(Broker::new(&upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]);
    let tile =
        format!("/api/endpoint/{SLUG}/ogc/features/collections/Car/tiles/WebMercatorQuad/0/0/0");
    let response = router(Arc::new(gateway))
        .oneshot(get(&tile))
        .await
        .expect("the gateway answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "application/vnd.mapbox-vector-tile"
    );
    assert_eq!(response.headers()["etag"], "W/\"t1\"");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    assert_eq!(bytes.as_ref(), TILE);
    assert_eq!(
        hops_of(&hops)[0].path,
        format!("/x/views/{SLUG}/ogc/features/collections/Car/tiles/WebMercatorQuad/0/0/0")
    );
}

/// T-5204: a HEAD answers the Content-Length of the GET the caller would get, its links moved
/// under the Endpoint's URL, and no body.
#[tokio::test]
async fn a_head_answers_the_length_of_its_get() {
    let (upstream, hops) = broker(StatusCode::OK, &[]).await;
    let items = format!("/api/endpoint/{SLUG}/ogc/features/collections/Car/items?limit=1");
    let (_, got, body) = call(&upstream, endpoint(), get(&items)).await;
    let (status, headers, empty) = call(&upstream, endpoint(), request(Method::HEAD, &items)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(empty.is_empty());
    assert_eq!(headers["content-length"], body.len().to_string());
    assert_eq!(headers["content-length"], got["content-length"]);
    assert!(hops_of(&hops).iter().all(|hop| hop.method == Method::GET));
}
