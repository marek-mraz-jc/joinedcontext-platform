//! The `schema/` child of an Endpoint and of a space is the broker's LinkML surface of its view
//! (T-3645, plan 5.4, EP-46, SP-04).
//!
//! `/api/endpoint/{slug}/schema{rest}` goes to `/x/views/{slug}/linkml/schema{rest}`, and
//! `/cs/{space}/schema{rest}` to the view `cs-{space}`, with `Accept` and `If-None-Match` as
//! sent and the tenant and the verified caller pinned. The broker narrows every document; these
//! tests hold what the gateway does around that hop: the path, the revalidation, a hidden name
//! answered as an unknown one (LK8), a broker failure in the gateway's own words, the refusal's
//! platform slug, and the `404` of an Endpoint no view serves (T-3646).

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model, Space};
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";
const DIGEST: &str = "5d41402abc4b2a76b9719d911017c592aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// One hop as the broker received it.
#[derive(Clone)]
struct Hop {
    path: String,
    headers: HeaderMap,
}

type Hops = Arc<Mutex<Vec<Hop>>>;

/// A broker that records every hop and answers `status`: an index naming one artifact with
/// [`DIGEST`], any other document as the bytes `shapes`, a `304` to a matching
/// `If-None-Match`, and a failure or a refusal carrying a secret the caller may not read.
async fn broker(status: StatusCode) -> (String, Hops) {
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let recorder = Arc::clone(&recorder);
        async move {
            let path = request.uri().path().to_owned();
            let revalidated = request
                .headers()
                .get("if-none-match")
                .is_some_and(|tag| tag == format!("\"{DIGEST}\"").as_str());
            recorder.lock().expect("the hop log").push(Hop {
                path: path.clone(),
                headers: request.headers().clone(),
            });
            let etag = [("etag", format!("\"{DIGEST}\""))];
            if !status.is_success() {
                let problem = json!({
                    "type": if status == StatusCode::FORBIDDEN { "urn:antares:error:AccessDenied" } else { "about:blank" },
                    "detail": "class InternalIncident of postgres://broker:hunter2@db/antares",
                });
                return (status, axum::Json(problem)).into_response();
            }
            if revalidated {
                return (StatusCode::NOT_MODIFIED, etag).into_response();
            }
            if path.ends_with("/index.json") {
                let index = json!({ "endpoint": SLUG, "models": [{
                    "name": "bb-air-quality", "version": 1, "semver": "1.4.0",
                    "artifacts": { "model.shacl.ttl": { "type": "text/turtle", "bytes": 6, "sha256": DIGEST } },
                }] });
                return (etag, axum::Json(index)).into_response();
            }
            (etag, "shapes").into_response()
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
         information:\n  - entities: [{{ type: AirQualityObserved }}]\n"
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
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: Default::default(),
        projection: None,
        models: vec![Model {
            name: "bb-air-quality".to_owned(),
            version: "1.4.0".to_owned(),
            major: 1,
            classes: vec!["AirQualityObserved".to_owned()],
            json_schema: Some(
                json!({ "$defs": { "AirQualityObserved": {
                "type": "object", "properties": { "pm10": { "type": "number" } } } } })
                .into(),
            ),
            context: Some(json!({ "@context": { "pm10": "https://bb.example.sk/pm10" } }).into()),
        }],
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant],
    }
}

/// The space surface of the same Endpoint, as the resolver builds it (SP-03).
fn space() -> Space {
    Space {
        endpoint: Arc::new(Endpoint {
            slug: SPACE.to_owned(),
            base_path: format!("/cs/{SPACE}"),
            ..endpoint()
        }),
        title: Default::default(),
        description: Default::default(),
        is_sandbox: false,
        default_locale: None,
    }
}

async fn call(
    upstream: &str,
    endpoint: Endpoint,
    request: Request<Body>,
) -> (StatusCode, HeaderMap, String) {
    let gateway = Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN)
        .serve([endpoint])
        .serve_spaces([space()]);
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

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("cookie", "session=not-for-the-broker")
        .body(Body::empty())
        .expect("a request")
}

fn paths(hops: &Hops) -> Vec<String> {
    hops.lock()
        .expect("the hop log")
        .iter()
        .map(|hop| hop.path.clone())
        .collect()
}

/// EP-46, EP-49: the index and one artifact go to the slug's view, the artifact name still
/// percent-encoded, `Accept` as sent, the tenant and the caller pinned, not the cookie.
#[tokio::test]
async fn the_index_and_an_artifact_go_to_the_views_linkml_surface() {
    let (upstream, hops) = broker(StatusCode::OK).await;
    let (status, _, body) = call(
        &upstream,
        endpoint(),
        get(&format!("/api/endpoint/{SLUG}/schema/index.json")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(DIGEST), "{body}");

    let mut asked = get(&format!("/api/endpoint/{SLUG}/schema/v1/model%2Eshacl.ttl"));
    asked
        .headers_mut()
        .insert("accept", "text/turtle".parse().expect("a header value"));
    let (status, _, body) = call(&upstream, endpoint(), asked).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "shapes");

    let root = format!("/x/views/{SLUG}/linkml/schema");
    assert_eq!(
        paths(&hops),
        [
            format!("{root}/index.json"),
            format!("{root}/v1/model%2Eshacl.ttl")
        ]
    );
    let hops = hops.lock().expect("the hop log");
    let hop = hops.last().expect("a hop");
    assert_eq!(hop.headers["accept"], "text/turtle");
    assert_eq!(hop.headers["ngsild-tenant"], SPACE);
    assert_eq!(hop.headers["x-jc-roles"], "public");
    assert!(hop.headers.get("cookie").is_none());
}

/// EP-48: the broker's strong ETag is the one the caller revalidates with, and its `304` comes
/// back as a `304`.
#[tokio::test]
async fn a_revalidation_is_the_brokers_304() {
    let (upstream, hops) = broker(StatusCode::OK).await;
    let mut asked = get(&format!("/api/endpoint/{SLUG}/schema/v1/model.shacl.ttl"));
    asked.headers_mut().insert(
        "if-none-match",
        format!("\"{DIGEST}\"").parse().expect("a header value"),
    );
    let (status, headers, body) = call(&upstream, endpoint(), asked).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED, "{body}");
    assert_eq!(headers["etag"], format!("\"{DIGEST}\"").as_str());
    assert_eq!(
        hops.lock().expect("the hop log")[0].headers["if-none-match"],
        format!("\"{DIGEST}\"").as_str()
    );
}

/// SP-04: the space prefix reads the view the Portal writes per Context Space.
#[tokio::test]
async fn the_space_prefix_reads_the_spaces_view() {
    let (upstream, hops) = broker(StatusCode::OK).await;
    for path in ["index.json", "v1/model.shacl.ttl"] {
        let (status, _, body) = call(
            &upstream,
            endpoint(),
            get(&format!("/cs/{SPACE}/schema/{path}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    assert_eq!(
        paths(&hops),
        [
            format!("/x/views/cs-{SPACE}/linkml/schema/index.json"),
            format!("/x/views/cs-{SPACE}/linkml/schema/v1/model.shacl.ttl")
        ]
    );
}

/// LK8: a class, slot or model the caller may not see is the broker's `404`, and the caller gets
/// the gateway's own `404`, the body of a name that does not exist, not the broker's words.
#[tokio::test]
async fn a_hidden_name_is_the_plain_404() {
    let (upstream, _) = broker(StatusCode::NOT_FOUND).await;
    let (status, _, body) = call(
        &upstream,
        endpoint(),
        get(&format!(
            "/api/endpoint/{SLUG}/schema/v1/classes/InternalIncident"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("resource-not-found"), "{body}");
    assert!(!body.contains("InternalIncident"), "{body}");
}

/// T-2260: a failing broker's words stay in the operator's log.
#[tokio::test]
async fn a_broker_failure_is_the_gateways_document() {
    let (upstream, _) = broker(StatusCode::INTERNAL_SERVER_ERROR).await;
    let (status, _, body) = call(
        &upstream,
        endpoint(),
        get(&format!("/api/endpoint/{SLUG}/schema/index.json")),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("broker-failure"), "{body}");
    assert!(!body.contains("hunter2"), "{body}");
}

/// API/00 §4: the broker's refusal gains the platform's slug, as on every other surface.
#[tokio::test]
async fn a_refusal_carries_the_platform_slug() {
    let (upstream, _) = broker(StatusCode::FORBIDDEN).await;
    let (status, _, body) = call(
        &upstream,
        endpoint(),
        get(&format!("/api/endpoint/{SLUG}/schema/index.json")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let problem: Value = serde_json::from_str(&body).expect("a problem document");
    assert_eq!(problem["code"], "forbidden", "{body}");
}

/// An Endpoint no view serves (here a slug longer than a view id may be) has no schema child:
/// the plain `404`, no document rendered by the gateway, and the broker is not asked (T-3646).
#[tokio::test]
async fn an_endpoint_without_a_view_has_no_schema() {
    let (upstream, hops) = broker(StatusCode::OK).await;
    let slug = "a".repeat(64);
    let unviewed = Endpoint {
        slug: slug.clone(),
        base_path: format!("/api/endpoint/{slug}"),
        ..endpoint()
    };
    let (status, _, body) = call(
        &upstream,
        unviewed,
        get(&format!("/api/endpoint/{slug}/schema/index.json")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!body.contains("bb-air-quality"), "{body}");
    for artifact in ["v1/model.linkml.yaml", "v1/model.schema.json", "v1/model"] {
        let (status, _, body) = call(
            &upstream,
            Endpoint {
                slug: slug.clone(),
                base_path: format!("/api/endpoint/{slug}"),
                ..endpoint()
            },
            get(&format!("/api/endpoint/{slug}/schema/{artifact}")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{artifact}: {body}");
    }
    assert!(paths(&hops).is_empty());
}

/// EP-68: the record names the digest the broker's index names, the ETag the artifact answers.
#[tokio::test]
async fn the_records_digest_is_the_brokers() {
    let (upstream, _) = broker(StatusCode::OK).await;
    let mut asked = get(&format!("/api/endpoint/{SLUG}/"));
    asked.headers_mut().insert(
        "accept",
        "application/ld+json".parse().expect("a header value"),
    );
    let (status, _, body) = call(&upstream, endpoint(), asked).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let record: Value = serde_json::from_str(&body).expect("JSON-LD");
    let shacl = record["dcat:distribution"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| {
            d["dcat:accessURL"]
                .as_str()
                .is_some_and(|url| url.ends_with("/schema/v1/model.shacl.ttl"))
        })
        .unwrap_or_else(|| panic!("the record lists the broker's artifact: {body}"));
    assert_eq!(shacl["spdx:checksum"]["spdx:checksumValue"], DIGEST);
}

/// A space and a slug named `schema` hold `/schema/` twice: the path after the surface's own
/// prefix is the one forwarded, not the path after the first `/schema/`.
#[tokio::test]
async fn a_space_or_slug_named_schema_keeps_its_path() {
    let (upstream, hops) = broker(StatusCode::OK).await;
    let named = Endpoint {
        slug: "schema".to_owned(),
        base_path: "/api/endpoint/schema".to_owned(),
        ..endpoint()
    };
    let named_space = Space {
        endpoint: Arc::new(Endpoint {
            slug: "schema".to_owned(),
            space: "schema".to_owned(),
            base_path: "/cs/schema".to_owned(),
            ..endpoint()
        }),
        ..space()
    };
    let gateway = Gateway::new(Broker::new(&upstream), Box::new(PolicyPdp), DOMAIN)
        .serve([named])
        .serve_spaces([named_space]);
    let app = router(Arc::new(gateway));
    for uri in [
        "/api/endpoint/schema/schema/index.json",
        "/cs/schema/schema/index.json",
    ] {
        let response = app
            .clone()
            .oneshot(get(uri))
            .await
            .expect("the gateway answers");
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
    assert_eq!(
        paths(&hops),
        [
            "/x/views/schema/linkml/schema/index.json",
            "/x/views/cs-schema/linkml/schema/index.json"
        ]
    );
}
