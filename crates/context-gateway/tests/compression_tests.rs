//! EP-166, T-5847: the gateway packs its answer for the client and asks the broker for plain bytes, so
//! every route that parses or rewrites the broker's body reads JSON, never gzip.

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "k4y7pq2mzt6vhx3nbwrs5cjd8f";
const SPACE: &str = "ovzdusie";
const DOMAIN: &str = "banskabystrica.sk";

type Hops = Arc<Mutex<Vec<HeaderMap>>>;

/// A broker that answers a page of ten entities and records the headers of every hop.
async fn broker() -> (String, Hops) {
    let page: Vec<Value> = (0..10)
        .map(|n| {
            json!({
                "id": format!("urn:ngsi-ld:AirQualityObserved:{n}"),
                "type": "AirQualityObserved",
                "pm10": {"type": "Property", "value": n},
                "name": {"type": "Property", "value": "Námestie SNP"},
            })
        })
        .collect();
    let body = Value::Array(page).to_string();
    let hops: Hops = Arc::default();
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(move |request: Request<Body>| {
        let (recorder, body) = (Arc::clone(&recorder), body.clone());
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.headers().clone());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
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
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![grant],
    }
}

async fn read(app: &Router, encoding: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::get(format!(
        "/api/endpoint/{SLUG}/ngsi-ld/v1/entities?type=AirQualityObserved"
    ));
    if let Some(encoding) = encoding {
        request = request.header(header::ACCEPT_ENCODING, encoding);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).expect("a request"))
        .await
        .expect("the gateway answers");
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (status, headers, bytes.to_vec())
}

/// EP-166: the client's gzip answer decodes to the plain one; the broker hop carries no
/// Accept-Encoding.
#[tokio::test]
async fn the_client_gets_gzip_and_the_broker_plain_json() {
    let (url, hops) = broker().await;
    let app = router(Arc::new(
        Gateway::new(Broker::new(url), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
    ));

    let (status, headers, plain) = read(&app, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&plain)
    );
    assert!(
        headers.get(header::CONTENT_ENCODING).is_none(),
        "{headers:?}"
    );

    let (status, headers, packed) = read(&app, Some("gzip")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_ENCODING], "gzip", "{headers:?}");
    // Appended to the gateway's own Vary, never in place of it.
    let vary = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ")
        .to_ascii_lowercase();
    assert!(vary.contains("accept-encoding"), "{vary}");
    assert!(vary.contains("authorization, accept,"), "{vary}");
    assert_eq!(vary.matches("accept-encoding").count(), 1, "{vary}");
    assert!(
        packed.len() < plain.len(),
        "{} >= {}",
        packed.len(),
        plain.len()
    );
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(packed.as_slice())
        .read_to_end(&mut decoded)
        .expect("gzip");
    assert_eq!(decoded, plain);

    let hops = hops.lock().expect("the hop log");
    assert_eq!(hops.len(), 2, "one broker read per client read");
    for hop in hops.iter() {
        assert!(
            hop.get(header::ACCEPT_ENCODING).is_none(),
            "Accept-Encoding reached the broker: {hop:?}"
        );
    }
}
