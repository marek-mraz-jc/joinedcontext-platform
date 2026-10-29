//! A sweep over many Endpoints holds a bounded amount of memory (T-5182).
//!
//! On dev one anonymous sweep that opened `/ws` and read the first 64 KiB of `file.csv` on every
//! Endpoint at once took the context-gateway from 179 Mi to 1.3 Gi and past its limit: each
//! `/ws` open started a fan-out that read its Endpoint's whole view into an entity table and held
//! it for the 60 s linger, although no client ever subscribed. This test counts every byte the
//! process allocates (the gateway and its stub broker) while the same sweep runs over
//! [`ENDPOINTS`] Endpoints of [`ENTITIES`] entities each, and holds the peak under [`BOUND`].

#[path = "../common/mod.rs"]
mod common;

use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::extract::{Path, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use futures_util::StreamExt;
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Counts the bytes allocated now and the most at any moment since [`reset_peak`].
struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call goes to `System` unchanged; the counters only observe the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let at = unsafe { System.alloc(layout) };
        if !at.is_null() {
            grew(layout.size());
        }
        at
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let at = unsafe { System.alloc_zeroed(layout) };
        if !at.is_null() {
            grew(layout.size());
        }
        at
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        unsafe { System.dealloc(at, layout) };
        NOW.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(at, layout, size) };
        if !moved.is_null() {
            NOW.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(size);
        }
        moved
    }
}

fn grew(size: usize) {
    let now = NOW.fetch_add(size, Ordering::Relaxed).saturating_add(size);
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn reset_peak() -> usize {
    let now = NOW.load(Ordering::Relaxed);
    PEAK.store(now, Ordering::Relaxed);
    now
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Endpoints the sweep reaches at once, as dev's anonymous sweep reached about twenty.
const ENDPOINTS: usize = 20;
/// Entities each Endpoint's view holds.
const ENTITIES: usize = 3000;
/// What the sweep may add to the process at its peak: measured 42-60 MiB after T-5182, nearly
/// all of it the two `file.csv` tables the download cap admits; before T-5182 the twenty fan-outs
/// held each view whole, 843 MiB here.
const BOUND: usize = 96 * 1024 * 1024;

const HOST: &str = "https://hel.example.fi";

/// The base32 slug of Endpoint `n`.
fn slug(n: usize) -> String {
    let letter = |k: usize| char::from(b'a' + u8::try_from(k % 26).expect("a letter"));
    format!("{}{}{}", "s".repeat(24), letter(n / 26), letter(n))
}

fn repo() -> std::path::PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-sweep-memory-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).expect("write");
    write(
        "space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: helsinki\n  \
         namespace: helsinki\nspec:\n  isSandbox: false\n  urnSegment: helsinki\n",
    );
    for n in 0..ENDPOINTS {
        write(
            &format!("endpoint-{n}.yaml"),
            &format!(
                "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: hsl-{n}\n  \
                 namespace: helsinki\nspec:\n  contextSpaceRef: helsinki\n  slug: {}\n  audience: public\n",
                slug(n)
            ),
        );
    }
    write(
        "policy.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: public-vehicles\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: { kind: ContextSpace, name: helsinki }\n  \
         assigner: did:web:hel.fi\n  assignee: { kind: role, id: public }\n  \
         operations: [retrieveOps]\n  information:\n    - entities:\n        - type: Vehicle\n",
    );
    dir
}

/// One Vehicle of about 2 KiB of JSON, as the HSL feed writes them.
fn vehicle(n: usize) -> serde_json::Value {
    let mut entity = json!({
        "id": format!("urn:ngsi-ld:Vehicle:hsl:{n:06}"),
        "type": "Vehicle",
        "location": {"type": "GeoProperty",
            "value": {"type": "Point", "coordinates": [24.94 + n as f64 * 1e-5, 60.17]},
            "observedAt": "2026-10-22T05:00:00Z"},
    });
    for name in [
        "speed",
        "heading",
        "odometer",
        "delay",
        "occupancy",
        "doors",
        "route",
        "direction",
        "operator",
        "journeyStart",
        "stop",
        "nextStop",
        "line",
        "trip",
        "vehicleNumber",
    ] {
        entity[name] = json!({"type": "Property", "value": format!("{name}-{n}-{}", "x".repeat(60)),
            "observedAt": "2026-10-22T05:00:00Z", "unitCode": "KMH"});
    }
    entity
}

/// The stub broker: each view's WebSocket welcomes and accepts the one subscription, and its
/// entity query pages [`ENTITIES`] Vehicles out of pages built once.
async fn broker() -> String {
    let pages: Arc<Vec<axum::body::Bytes>> = Arc::new(
        (0..ENTITIES)
            .step_by(1000)
            .map(|from| {
                let page: Vec<_> = (from..(from + 1000).min(ENTITIES)).map(vehicle).collect();
                axum::body::Bytes::from(serde_json::to_vec(&page).expect("a page"))
            })
            .collect(),
    );
    let app = Router::new()
        .route(
            "/x/views/{view}/ws",
            get(
                |Path(_view): Path<String>, upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(|mut socket| async move {
                        let welcome = json!({"metadata": {"x-op": "x-welcome",
                        "x-connection": "c1", "x-resume-token": "t1"}});
                        if socket
                            .send(AxumMessage::Text(welcome.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        while let Some(Ok(AxumMessage::Text(text))) = socket.recv().await {
                            if text.as_str().contains("x-subscribe") {
                                let created = json!({"metadata": {"x-op": "x-result", "x-id": "up",
                                "x-status": 201}, "body": {"id": "urn:ngsi-ld:Subscription:up",
                                "type": "Subscription"}});
                                if socket
                                    .send(AxumMessage::Text(created.to_string().into()))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    })
                },
            ),
        )
        .fallback(move |request: Request| {
            let pages = Arc::clone(&pages);
            async move { view(&pages, &request) }
        });
    serve(app).await
}

/// A view read: the type list, or a page of Vehicles counted when asked.
fn view(pages: &[axum::body::Bytes], request: &Request) -> Response {
    let path = request.uri().path();
    let query = request.uri().query().unwrap_or_default();
    if path.ends_with("/types") {
        return axum::Json(json!({"type": "EntityTypeList", "typeList": ["Vehicle"]}))
            .into_response();
    }
    if !path.ends_with("/entities") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let offset: usize = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("offset="))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let page = pages
        .get(offset / 1000)
        .cloned()
        .unwrap_or_else(|| axum::body::Bytes::from_static(b"[]"));
    let mut response = (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        page,
    )
        .into_response();
    if query.contains("count=true") {
        response.headers_mut().insert(
            "NGSILD-Results-Count",
            ENTITIES.to_string().parse().expect("a header value"),
        );
    }
    response
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("127.0.0.1:{}", address.port())
}

async fn gateway(upstream: &str, realm: &common::Realm) -> String {
    let (endpoints, ..) = store::load(&repo()).expect("the repository loads");
    let gateway = Gateway::new(
        Broker::new(format!("http://{upstream}")),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve(endpoints)
    .authenticate(
        Arc::new(realm.verifier()),
        ServiceAccounts::new(),
        Some(HOST.to_owned()),
    );
    serve(router(Arc::new(gateway))).await
}

/// What the probe does to `/ws`: open, read the welcome, close.
async fn touch_ws(at: &str, n: usize) {
    let mut request = format!("ws://{at}/api/endpoint/{}/ws", slug(n))
        .into_client_request()
        .expect("a request");
    request.headers_mut().insert(
        "x-forwarded-for",
        format!("203.0.113.{n}").parse().expect("a value"),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("upgraded");
    let _welcome = socket.next().await;
    let _ = socket.close(None).await;
}

/// What the probe does to `file.csv`: read the first 64 KiB and hang up.
async fn touch_csv(at: &str, n: usize) {
    let Ok(mut stream) = tokio::net::TcpStream::connect(at).await else {
        return;
    };
    let ask = format!(
        "GET /api/endpoint/{}/file.csv HTTP/1.1\r\nHost: {at}\r\nX-Forwarded-For: 198.51.100.{n}\r\n\r\n",
        slug(n)
    );
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if stream.write_all(ask.as_bytes()).await.is_err() {
        return;
    }
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut read = 0;
    while read < buffer.len() {
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer[read..])).await
        {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(got)) => read += got,
        }
    }
}

/// T-5182: twenty `/ws` opens and twenty `file.csv` reads at once stay under [`BOUND`], at the
/// peak and once the fan-outs linger with no client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sweep_over_many_endpoints_stays_bounded() {
    let realm = common::Realm::new();
    let upstream = broker().await;
    let at = gateway(&upstream, &realm).await;
    // Warm the shared tables (JSON-LD context, compiled policies) before counting.
    touch_csv(&at, 0).await;
    touch_ws(&at, 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let before = reset_peak();
    let sweep = (0..ENDPOINTS).map(|n| {
        let at = at.clone();
        async move {
            tokio::join!(touch_ws(&at, n), touch_csv(&at, n));
        }
    });
    futures_util::future::join_all(sweep).await;
    // The fan-outs' upstreams subscribe and, before T-5182, read their views now.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    let held = NOW.load(Ordering::Relaxed).saturating_sub(before);
    eprintln!(
        "sweep peak {} KiB, held after {} KiB",
        peak / 1024,
        held / 1024
    );
    assert!(peak < BOUND, "the sweep peaked at {} MiB", peak >> 20);
    assert!(
        held < BOUND / 4,
        "the lingering fan-outs hold {} MiB",
        held >> 20
    );
}
