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

// T-5849: brotli quality 2, gzip level 1 and a 1 KiB minimum. The handler below answers a fixed
// body, so each case sets the exact size and type.

fn fixed(body: Vec<u8>, content_type: &'static str) -> Router {
    Router::new()
        .route(
            "/",
            axum::routing::get(move || {
                let body = body.clone();
                async move { ([(header::CONTENT_TYPE, content_type)], body) }
            }),
        )
        .layer(context_gateway::app::compression())
}

async fn fetch(app: Router, encoding: &str) -> (HeaderMap, Vec<u8>) {
    let response = app
        .oneshot(
            Request::get("/")
                .header(header::ACCEPT_ENCODING, encoding)
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (headers, bytes.to_vec())
}

/// A page of entities as the broker answers it: repeated attribute names, varying values, ~20 KB.
fn page() -> Vec<u8> {
    let rows: Vec<String> = (0..80)
        .map(|n| {
            format!(
                r#"{{"id":"urn:ngsi-ld:BikeHireDockingStation:{n:03}","type":"BikeHireDockingStation","availableBikeNumber":{{"type":"Property","value":{}}},"location":{{"type":"GeoProperty","value":{{"type":"Point","coordinates":[24.{:04},60.{:04}]}}}}}}"#,
                n * 7 % 23,
                n * 131 % 9973,
                n * 89 % 9967
            )
        })
        .collect();
    format!("[{}]", rows.join(",")).into_bytes()
}

fn json_of(len: usize) -> Vec<u8> {
    let mut body = page();
    body.truncate(len);
    body
}

fn encoding(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
}

fn gunzip(packed: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(packed)
        .read_to_end(&mut out)
        .expect("gzip");
    out
}

fn unbrotli(packed: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::Decompressor::new(packed, 4096)
        .read_to_end(&mut out)
        .expect("br");
    out
}

#[tokio::test]
async fn a_900_byte_body_stays_plain() {
    let body = json_of(900);
    let (headers, got) = fetch(fixed(body.clone(), "application/json"), "gzip, br").await;
    assert_eq!(encoding(&headers), None, "{headers:?}");
    assert_eq!(got, body);
}

#[tokio::test]
async fn a_2_kib_body_is_packed() {
    let body = json_of(2048);
    let (headers, got) = fetch(fixed(body.clone(), "application/json"), "gzip").await;
    assert_eq!(encoding(&headers), Some("gzip"));
    assert_eq!(gunzip(&got), body);
}

#[tokio::test]
async fn br_wins_when_both_are_offered() {
    for offer in ["gzip, br", "br, gzip", "gzip;q=1, br;q=0.9"] {
        let (headers, got) = fetch(fixed(page(), "application/json"), offer).await;
        assert_eq!(encoding(&headers), Some("br"), "{offer}");
        let vary = headers
            .get_all(header::VARY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",")
            .to_ascii_lowercase();
        assert_eq!(vary.matches("accept-encoding").count(), 1, "{vary}");
        assert_eq!(unbrotli(&got), page());
    }
}

#[tokio::test]
async fn event_streams_and_images_stay_plain() {
    for content_type in ["text/event-stream", "image/png"] {
        let body = json_of(4096);
        let (headers, got) = fetch(fixed(body.clone(), content_type), "gzip, br").await;
        assert_eq!(encoding(&headers), None, "{content_type}: {headers:?}");
        assert_eq!(got, body);
    }
}

#[tokio::test]
async fn gzip_runs_at_level_1() {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(1));
    enc.write_all(&page()).expect("gzip");
    let want = enc.finish().expect("gzip");
    let (headers, got) = fetch(fixed(page(), "application/json"), "gzip").await;
    assert_eq!(encoding(&headers), Some("gzip"));
    assert_eq!(got, want);
}

#[tokio::test]
async fn br_runs_at_quality_2() {
    use std::io::Write;
    let mut enc = brotli::CompressorWriter::new(Vec::new(), 4096, 2, 22);
    enc.write_all(&page()).expect("br");
    let want = enc.into_inner();
    let (headers, got) = fetch(fixed(page(), "application/json"), "br").await;
    assert_eq!(encoding(&headers), Some("br"));
    assert_eq!(got, want);
}

// T-5851: a proxied answer arrives in frames with a `Pending` between them, and the encoder ends
// a block at each `Pending`. The gateway hands it one frame per `coalesce::CAP` bytes instead.

/// `rows` stations, ~250 bytes each: 1600 make the 400 KB fixture.
fn stations(rows: usize) -> Vec<u8> {
    let rows: Vec<String> = (0..rows)
        .map(|n| {
            format!(
                r#"{{"id":"urn:ngsi-ld:BikeHireDockingStation:{n:04}","type":"BikeHireDockingStation","name":{{"type":"Property","value":"Asema {}"}},"availableBikeNumber":{{"type":"Property","value":{},"observedAt":"2026-10-27T14:{:02}:00Z"}},"location":{{"type":"GeoProperty","value":{{"type":"Point","coordinates":[24.{:04},60.{:04}]}}}}}}"#,
                n * 37 % 997,
                n * 7 % 23,
                n % 60,
                n * 131 % 9973,
                n * 89 % 9967
            )
        })
        .collect();
    format!("[{}]", rows.join(",")).into_bytes()
}

/// `body` in `frames` frames, each behind one `Pending`, as hyper hands a proxied answer on.
fn framed(body: &[u8], frames: usize) -> Body {
    use futures_util::StreamExt;
    let size = body.len().div_ceil(frames);
    let chunks: Vec<axum::body::Bytes> = body
        .chunks(size)
        .map(axum::body::Bytes::copy_from_slice)
        .collect();
    Body::from_stream(
        futures_util::stream::unfold(chunks.into_iter(), |mut chunks| async move {
            tokio::task::yield_now().await;
            chunks
                .next()
                .map(|chunk| (Ok::<_, std::io::Error>(chunk), chunks))
        })
        .fuse(),
    )
}

/// A route answering `make()`'s body with `content_type`, behind the gateway's compression.
fn streamed(make: fn() -> Body, content_type: &'static str) -> Router {
    Router::new()
        .route(
            "/",
            axum::routing::get(
                move || async move { ([(header::CONTENT_TYPE, content_type)], make()) },
            ),
        )
        .layer(context_gateway::app::compression())
}

fn fixture_in_64_frames() -> Body {
    framed(&stations(1600), 64)
}

/// One frame per station, as Antares streams an entity query (one chunk per entity).
fn fixture_per_entity() -> Body {
    framed(&stations(1600), 1600)
}

fn fixture_in_one_frame() -> Body {
    Body::from(stations(1600))
}

/// A name and the body it makes.
type Shape = (&'static str, fn() -> Body);

#[tokio::test]
async fn a_400_kb_answer_in_many_frames_packs_as_well_as_in_one() {
    assert!(stations(1600).len() > 400_000);
    let shapes: [Shape; 2] = [
        ("64 frames", fixture_in_64_frames),
        ("a frame per entity", fixture_per_entity),
    ];
    for offer in ["br", "gzip"] {
        let (_, once) = fetch(streamed(fixture_in_one_frame, "application/json"), offer).await;
        for (shape, make) in shapes {
            let (headers, framed) = fetch(streamed(make, "application/json"), offer).await;
            assert_eq!(encoding(&headers), Some(offer));
            let plain = if offer == "br" {
                unbrotli(&framed)
            } else {
                gunzip(&framed)
            };
            assert_eq!(plain, stations(1600), "{offer}, {shape}");
            assert!(
                framed.len() * 10 <= once.len() * 11,
                "{offer}: {shape} packed to {} bytes, one frame to {}",
                framed.len(),
                once.len()
            );
        }
    }
}

/// The first frame of `app`'s answer, or `None` when none comes within two seconds.
async fn first_frame(app: Router, content_type: &str) -> Option<Vec<u8>> {
    use http_body_util::BodyExt;
    let response = app
        .oneshot(
            Request::get("/")
                .header(header::ACCEPT_ENCODING, "br")
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("an answer");
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.as_bytes()),
        Some(content_type.as_bytes())
    );
    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame()).await;
    frame
        .ok()
        .flatten()
        .map(|f| f.expect("a frame").into_data().expect("data").to_vec())
}

/// `first` then a stream that never ends.
fn then_open(first: Vec<u8>) -> Body {
    use futures_util::StreamExt;
    Body::from_stream(
        futures_util::stream::once(async move {
            Ok::<_, std::io::Error>(axum::body::Bytes::from(first))
        })
        .chain(futures_util::stream::pending()),
    )
}

fn one_event_then_open() -> Body {
    then_open(b"event: message\ndata: {\"jsonrpc\":\"2.0\"}\n\n".to_vec())
}

fn over_the_cap_then_open() -> Body {
    then_open(stations(1600).repeat(2))
}

#[tokio::test]
async fn an_event_stream_is_not_held() {
    let first = first_frame(
        streamed(one_event_then_open, "text/event-stream"),
        "text/event-stream",
    )
    .await;
    assert_eq!(
        first.as_deref(),
        Some(&b"event: message\ndata: {\"jsonrpc\":\"2.0\"}\n\n"[..])
    );
}

#[tokio::test]
async fn a_body_over_the_cap_still_streams() {
    assert!(stations(1600).len() * 2 > context_gateway::middleware::coalesce::CAP);
    let first = first_frame(
        streamed(over_the_cap_then_open, "application/json"),
        "application/json",
    )
    .await;
    assert!(
        first.is_some_and(|f| !f.is_empty()),
        "nothing came before the end"
    );
}
