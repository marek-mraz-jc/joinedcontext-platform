//! Many brokers (ADR-N-047, PF-111, PF-112): a request goes to the broker of the space the
//! gateway pinned as tenant, or of the view it names, and nowhere else; a space whose broker
//! has no address is refused, never sent to `main`.

use axum::body::Body;
use axum::http::header::HeaderValue;
use axum::http::{HeaderMap, Method, StatusCode};
use context_gateway::proxy::{Broker, ProxyError};
use context_gateway::store::SpaceBrokers;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// What the upstream was asked.
#[derive(Clone, Default)]
struct Asked {
    headers: Vec<(String, String)>,
}

type Log = Arc<Mutex<Vec<Asked>>>;

/// An upstream that records every request and answers `status` with `body` and `headers`.
async fn upstream(
    status: u16,
    body: &'static str,
    headers: Vec<(&'static str, &'static str)>,
) -> (String, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&log);
    let app = axum::Router::new().fallback(axum::routing::any(
        move |request: axum::extract::Request| {
            let log = Arc::clone(&recorded);
            let headers = headers.clone();
            async move {
                let seen = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.to_string(),
                            String::from_utf8_lossy(value.as_bytes()).into_owned(),
                        )
                    })
                    .collect();
                log.lock().expect("the log").push(Asked { headers: seen });
                let mut answer = axum::http::Response::builder()
                    .status(StatusCode::from_u16(status).expect("a status"));
                for (name, value) in headers {
                    answer = answer.header(name, value);
                }
                answer.body(Body::from(body)).expect("an answer")
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), log)
}

fn tenant(space: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "ngsild-tenant",
        HeaderValue::from_str(space).expect("a value"),
    );
    headers
}

/// `main` and a second broker on loopback; the second is addressed by the template with its
/// name as the port, so the template is exercised as the deployment writes it.
async fn two_brokers() -> (Broker, Log, Log, String) {
    let (main, main_log) = upstream(200, "main", Vec::new()).await;
    let (canary, canary_log) = upstream(200, "canary", Vec::new()).await;
    let port = canary.rsplit(':').next().expect("a port").to_owned();
    let broker = Broker::new(main).with_template(Some("http://127.0.0.1:{broker}".to_owned()));
    broker.route(&SpaceBrokers {
        by_space: BTreeMap::from([
            ("ovzdusie-air".to_owned(), "main".to_owned()),
            ("doprava-hfp".to_owned(), port.clone()),
            ("lost-space".to_owned(), String::new()),
        ]),
        by_view: BTreeMap::from([
            ("cs-doprava-hfp".to_owned(), port.clone()),
            ("hfp-vehicles".to_owned(), port.clone()),
        ]),
        ..SpaceBrokers::default()
    });
    (broker, main_log, canary_log, port)
}

async fn body(answer: axum::http::Response<Body>) -> String {
    String::from_utf8(
        axum::body::to_bytes(answer.into_body(), 1024)
            .await
            .expect("a body")
            .to_vec(),
    )
    .expect("utf-8")
}

#[tokio::test]
async fn each_space_is_answered_by_its_own_broker() {
    let (broker, main_log, canary_log, _) = two_brokers().await;
    let path = "/ngsi-ld/v1/entities?type=Vehicle";

    let on_canary = broker
        .send(Method::GET, path, tenant("doprava-hfp"), Body::empty())
        .await
        .expect("an answer");
    assert_eq!(body(on_canary).await, "canary");
    let on_main = broker
        .send(Method::GET, path, tenant("ovzdusie-air"), Body::empty())
        .await
        .expect("an answer");
    assert_eq!(body(on_main).await, "main");
    let unlisted = broker
        .send(Method::GET, path, tenant("praha-parking"), Body::empty())
        .await
        .expect("an answer");
    assert_eq!(
        body(unlisted).await,
        "main",
        "a space of no broker of its own is main's"
    );

    assert_eq!(canary_log.lock().unwrap().len(), 1);
    assert_eq!(main_log.lock().unwrap().len(), 2);
    assert_eq!(
        canary_log.lock().unwrap()[0]
            .headers
            .iter()
            .find(|(name, _)| name == "ngsild-tenant")
            .map(|(_, value)| value.as_str()),
        Some("doprava-hfp"),
        "the tenant travels with the request to its broker"
    );
}

#[tokio::test]
async fn a_view_is_answered_by_the_broker_of_its_space() {
    let (broker, main_log, canary_log, _) = two_brokers().await;
    for path in [
        "/x/views/hfp-vehicles/mcp",
        "/x/views/cs-doprava-hfp/ngsi-ld/v1/entities?limit=1",
    ] {
        let answer = broker
            .send(Method::POST, path, HeaderMap::new(), Body::empty())
            .await
            .expect("an answer");
        assert_eq!(body(answer).await, "canary", "{path}");
    }
    assert_eq!(canary_log.lock().unwrap().len(), 2);
    assert!(main_log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_space_whose_broker_has_no_address_is_refused_not_sent_to_main() {
    let (broker, main_log, _, port) = two_brokers().await;
    let refused = broker
        .send(
            Method::GET,
            "/ngsi-ld/v1/entities",
            tenant("lost-space"),
            Body::empty(),
        )
        .await;
    assert!(
        matches!(refused, Err(ProxyError::Unreachable(_))),
        "{refused:?}"
    );

    // Without a template no broker but `main` has an address.
    let (main, _) = upstream(200, "main", Vec::new()).await;
    let untemplated = Broker::new(main);
    untemplated.route(&SpaceBrokers {
        by_space: BTreeMap::from([("doprava-hfp".to_owned(), port)]),
        ..SpaceBrokers::default()
    });
    let refused = untemplated
        .send(
            Method::GET,
            "/ngsi-ld/v1/entities",
            tenant("doprava-hfp"),
            Body::empty(),
        )
        .await;
    assert!(
        matches!(refused, Err(ProxyError::Unreachable(_))),
        "{refused:?}"
    );
    assert!(main_log.lock().unwrap().is_empty());
}

fn manifest(dir: &std::path::Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write a manifest");
}

fn space(name: &str, broker_ref: &str) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: {name}\n  \
         namespace: helsinki\nspec:\n  isSandbox: false\n  urnSegment: {name}\n{broker_ref}"
    )
}

fn broker_manifest(name: &str, default: bool) -> String {
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextBroker\nmetadata:\n  name: {name}\n  \
         namespace: org\nspec:\n  default: {default}\n  image: antares@sha256:{}\n",
        "0".repeat(64)
    )
}

/// The table the gateway routes by, read from the manifests through `broker_of`.
#[test]
fn the_repository_says_which_broker_each_space_and_view_lives_on() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-many-brokers-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    manifest(&dir, "primary.yaml", &broker_manifest("primary", true));
    manifest(&dir, "canary.yaml", &broker_manifest("canary", false));
    manifest(&dir, "air.yaml", &space("air", ""));
    manifest(&dir, "hfp.yaml", &space("hfp", "  brokerRef: canary\n"));
    manifest(&dir, "lost.yaml", &space("lost", "  brokerRef: gone\n"));
    manifest(
        &dir,
        "moving.yaml",
        &space("air2", "").replace(
            "  namespace: helsinki\n",
            "  namespace: helsinki\n  annotations:\n    joinedcontext.com/moving-to: canary\n",
        ),
    );
    manifest(
        &dir,
        "endpoint.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: hfp-all\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: hfp\n  slug: aaaaaaaaaaaaaaaaaaaaaaaaaa\n  \
         audience: public\n  enabledRepresentations: [\"ngsi-ld\"]\n",
    );

    let brokers = context_gateway::store::load(&dir)
        .expect("the repository loads")
        .7;
    assert_eq!(
        brokers.by_space["air"], "primary",
        "no brokerRef is the default broker"
    );
    assert_eq!(brokers.by_space["hfp"], "canary");
    assert_eq!(
        brokers.by_space["lost"], "",
        "an undeclared broker is no address"
    );
    assert_eq!(brokers.by_view["cs-hfp"], "canary");
    assert_eq!(
        brokers.by_view["aaaaaaaaaaaaaaaaaaaaaaaaaa"], "canary",
        "an Endpoint's view follows its space"
    );
    assert_eq!(
        brokers
            .moving
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["air2", "cs-air2"],
        "only the annotated space is held, by tenant and by its own view"
    );
}

/// A move holds its space read-only (PF-115, ADR-N-047 §2.6): a write is answered 503 with
/// Retry-After and never reaches a broker; reads, a query by POST and other spaces pass.
#[tokio::test]
async fn a_moving_space_takes_reads_and_answers_writes_503_with_retry_after() {
    let (main, main_log) = upstream(200, "main", Vec::new()).await;
    let broker = Broker::new(main);
    broker.route(&SpaceBrokers {
        moving: ["air".to_owned(), "cs-air".to_owned(), "air-slug".to_owned()].into(),
        ..SpaceBrokers::default()
    });
    let entities = "/ngsi-ld/v1/entities";
    for (method, path, headers) in [
        (Method::POST, entities, tenant("air")),
        (
            Method::PATCH,
            "/ngsi-ld/v1/entities/urn:x/attrs",
            tenant("air"),
        ),
        (Method::DELETE, "/ngsi-ld/v1/entities/urn:x", tenant("air")),
        (
            Method::POST,
            "/ngsi-ld/v1/entityOperations/upsert",
            tenant("air"),
        ),
        (Method::POST, "/x/views/air-slug/mcp", HeaderMap::new()),
        (
            Method::PUT,
            "/x/views/cs-air/ngsi-ld/v1/entities/urn:x/attrs/a",
            HeaderMap::new(),
        ),
    ] {
        let answer = broker
            .send(method.clone(), path, headers, Body::empty())
            .await
            .expect("an answer");
        assert_eq!(
            answer.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path}"
        );
        assert_eq!(answer.headers()["retry-after"], "30", "{method} {path}");
        assert!(body(answer).await.contains("space-moving"));
    }
    assert!(
        main_log.lock().unwrap().is_empty(),
        "no write reached the broker"
    );

    for (method, path, headers) in [
        (Method::GET, entities, tenant("air")),
        (Method::HEAD, entities, tenant("air")),
        (
            Method::POST,
            "/ngsi-ld/v1/entityOperations/query?limit=5",
            tenant("air"),
        ),
        (
            Method::GET,
            "/x/views/air-slug/ngsi-ld/v1/entities",
            HeaderMap::new(),
        ),
        (Method::POST, entities, tenant("water")),
    ] {
        let answer = broker
            .send(method.clone(), path, headers, Body::empty())
            .await
            .expect("an answer");
        assert_eq!(answer.status(), StatusCode::OK, "{method} {path}");
    }
    assert_eq!(main_log.lock().unwrap().len(), 5);
}

#[test]
fn each_view_names_the_broker_it_lives_on() {
    let broker = Broker::new("http://main.invalid");
    broker.route(&SpaceBrokers {
        by_view: BTreeMap::from([("hfp-slug".to_owned(), "canary".to_owned())]),
        ..SpaceBrokers::default()
    });
    assert_eq!(broker.broker_of_view("hfp-slug"), "canary");
    assert_eq!(
        broker.broker_of_view("air-slug"),
        "main",
        "a view of no broker of its own is main's"
    );
}

/// PF-115: the gateway says which spaces it holds on its scrape, so the Portal's move job reads
/// the hold before it exports; a view's slug never becomes a label, and a released space reads 0.
#[tokio::test]
async fn the_scrape_says_which_spaces_are_held() {
    context_gateway::telemetry::install();
    let (main, _) = upstream(200, "main", Vec::new()).await;
    let broker = Broker::new(main);
    let held = |moving: &[&str]| SpaceBrokers {
        by_space: [("scrape-air".to_owned(), "main".to_owned())].into(),
        moving: moving.iter().map(|s| (*s).to_owned()).collect(),
        ..SpaceBrokers::default()
    };
    broker.route(&held(&["scrape-air", "cs-scrape-air", "scrape-slug"]));
    let scrape = scraped().await;
    assert!(
        scrape.contains("jc_gateway_space_held{space=\"scrape-air\"} 1"),
        "{scrape}"
    );
    assert!(
        !scrape.contains("scrape-slug") && !scrape.contains("cs-scrape-air"),
        "{scrape}"
    );

    broker.route(&held(&[]));
    let scrape = scraped().await;
    assert!(
        scrape.contains("jc_gateway_space_held{space=\"scrape-air\"} 0"),
        "{scrape}"
    );
}

async fn scraped() -> String {
    let answer = context_gateway::telemetry::metrics().await;
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("a scrape");
    String::from_utf8_lossy(&bytes).into_owned()
}
