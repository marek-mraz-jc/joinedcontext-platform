//! The `ws-fanout` rig (ADR-N-049 §2.9, EP-115, T-4133): N WebSocket clients with random filters
//! on one Endpoint, a generated Vehicle stream pushed through a stub broker, and every client's
//! notifications compared with the ground truth computed offline from the same stream by
//! `antares-matcher` on every (filter, change) pair: no miss, no extra, the stream's order; a
//! client the gateway coalesced for (it fell behind) still gets every entity's last state.
//!
//! `RIG_SOCKETS` (default 100), `RIG_CHANGES` (default 1000) and `RIG_RATE` (changes per second,
//! default 200) size a run; `RIG_REPORT` names a file the numbers are written to as JSON. The
//! gateway runs in this process, so the CPU and memory reported are the gateway's and the
//! clients' together. Run it on a GitHub runner (`ws-fanout-rig.yml`), never on the shared host.

mod common;

use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::routing::get;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::fanout::{changed, compile, hub::core};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const SLUG: &str = "vvvvvvvvvvvvvvvvvvvvvvvvvv";
const VEHICLES: u64 = 50;
const MODES: [&str; 3] = ["bus", "tram", "metro"];

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn repo() -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-ws-rig-{now}"));
    std::fs::create_dir_all(&dir).expect("create the repository");
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).expect("write");
    write(
        "space.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: transport\n  \
         namespace: helsinki\nspec:\n  isSandbox: false\n  urnSegment: transport\n",
    );
    write(
        "endpoint.yaml",
        &format!(
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: vehicles\n  \
             namespace: helsinki\nspec:\n  contextSpaceRef: transport\n  slug: {SLUG}\n  audience: public\n"
        ),
    );
    write(
        "policy.yaml",
        "apiVersion: joinedcontext.com/v1alpha1\nkind: Policy\nmetadata:\n  name: public-vehicles\n  \
         namespace: helsinki\nspec:\n  contextSpaceRef: { kind: ContextSpace, name: transport }\n  \
         assigner: did:web:hel.fi\n  assignee: { kind: role, id: public }\n  \
         operations: [retrieveOps]\n  information:\n    - entities:\n        - type: Vehicle\n",
    );
    dir
}

/// A broker whose view socket welcomes, accepts the gateway's subscription and forwards every
/// line the rig pushes; its entity query answers an empty page.
async fn broker() -> (String, tokio::sync::broadcast::Sender<Arc<String>>) {
    let (push, _) = tokio::sync::broadcast::channel::<Arc<String>>(1 << 16);
    let held = push.clone();
    let app = Router::new()
        .route(
            "/x/views/{view}/ws",
            get(move |upgrade: WebSocketUpgrade| {
                let mut pushed = held.subscribe();
                async move {
                    upgrade.on_upgrade(|mut socket| async move {
                        let welcome = json!({"metadata": {"x-op": "x-welcome",
                            "x-connection": "c", "x-resume-token": "t"}});
                        let _ = socket.send(AxumMessage::Text(welcome.to_string().into())).await;
                        loop {
                            tokio::select! {
                                read = socket.recv() => {
                                    let Some(Ok(AxumMessage::Text(text))) = read else { return };
                                    if text.as_str().contains("x-subscribe") {
                                        let created = json!({"metadata": {"x-op": "x-result",
                                            "x-id": "up", "x-status": 201}});
                                        let _ = socket.send(AxumMessage::Text(created.to_string().into())).await;
                                    }
                                }
                                Ok(text) = pushed.recv() => {
                                    if socket.send(AxumMessage::Text(text.as_str().into())).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    })
                }
            }),
        )
        .route(
            "/x/views/{view}/ngsi-ld/v1/entities",
            get(|| async { axum::Json(json!([])) }),
        );
    (serve(app).await, push)
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

/// A small deterministic generator, so a run replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    #[allow(clippy::cast_precision_loss)]
    fn float(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (self.next() % 1_000_000) as f64 / 1_000_000.0 * (hi - lo)
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[usize::try_from(self.below(items.len() as u64)).unwrap_or(0)]
    }
}

/// One filter of the grammar ADR-N-049 §2.3 serves; `rig` stays in every attribute list, since
/// it carries the change's number the latency is read from.
fn filter(rng: &mut Rng) -> Value {
    let speed = rng.below(30);
    let mode = rng.pick(&MODES);
    let (lon, lat) = (rng.float(24.90, 25.00), rng.float(60.15, 60.20));
    let n = rng.below(VEHICLES);
    let mut body = json!({"type": "Subscription", "entities": [{"type": "Vehicle"}]});
    let extra = match rng.below(9) {
        0 => json!({}),
        1 => json!({"entities": [{"type": "Vehicle", "idPattern": format!(":{n}$")}]}),
        2 => json!({"q": format!("speed>{speed}")}),
        3 => json!({"q": format!("speed<={speed}")}),
        4 => json!({"q": format!("transportMode==\"{mode}\"")}),
        5 => json!({"q": format!("speed>{speed};transportMode==\"{mode}\"")}),
        6 => json!({"geoQ": {"georel": format!("near;maxDistance=={}", 300 + rng.below(3000)),
                             "geometry": "Point", "coordinates": [lon, lat]}}),
        7 => json!({"watchedAttributes": ["speed"], "q": format!("speed>={speed}")}),
        _ => {
            json!({"notification": {"attributes": ["speed", "rig"]}, "q": format!("speed>{speed}|transportMode==\"{mode}\"")})
        }
    };
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        body.extend(extra.clone());
    }
    body
}

/// Change `n` of the stream: one vehicle's new state.
fn change(rng: &mut Rng, n: usize) -> Value {
    // A vehicle keeps its speed now and then, so a watched attribute sometimes does not change.
    let speed = rng.below(30);
    json!({
        "id": format!("urn:ngsi-ld:Vehicle:{}", rng.below(VEHICLES)),
        "type": "Vehicle",
        "speed": {"type": "Property", "value": speed},
        "transportMode": {"type": "Property", "value": rng.pick(&MODES)},
        "location": {"type": "GeoProperty", "value": {"type": "Point",
            "coordinates": [rng.float(24.88, 25.02), rng.float(60.14, 60.21)]}},
        "rig": {"type": "Property", "value": n},
    })
}

fn expanded(entity: &Value) -> Value {
    antares_jsonld::expand_entity(
        entity.as_object().expect("an entity"),
        core(),
        antares_jsonld::ExpandOpts::default(),
    )
    .expect("a valid entity")
}

/// What every filter must receive, computed without the gateway: the stream's changes each
/// filter's predicate and delta admit, in the stream's order.
fn ground_truth(filters: &[Value], stream: &[Value]) -> Vec<Vec<usize>> {
    let compiled: Vec<_> = filters
        .iter()
        .map(|f| compile(f, core()).expect("the grammar compiles"))
        .collect();
    let watched: Vec<Vec<String>> = filters
        .iter()
        .map(|f| {
            f["watchedAttributes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|a| antares_jsonld::expand_attr_name(a, core()).expect("a name"))
                .collect()
        })
        .collect();
    let mut previous: HashMap<String, Value> = HashMap::new();
    let mut expected = vec![Vec::new(); filters.len()];
    for (n, entity) in stream.iter().enumerate() {
        let id = entity["id"].as_str().expect("an id").to_owned();
        let now = expanded(entity);
        let delta = changed(previous.get(&id), &now);
        for (f, filter) in compiled.iter().enumerate() {
            let fires = !delta.is_empty()
                && (watched[f].is_empty() || watched[f].iter().any(|a| delta.contains(a)));
            if fires
                && antares_matcher::would_notify(filter.subscription(), &now, core(), &|_| None)
            {
                expected[f].push(n);
            }
        }
        previous.insert(id, now);
    }
    expected
}

/// A client that fell behind gets its queue coalesced (ADR-N-049 §2.6): per entity it receives
/// a subsequence of what its filter admits, always ending with the entity's last admitted state.
fn holds_coalesced(got: &[usize], expected: &[usize], stream: &[Value]) -> bool {
    let by_entity = |changes: &[usize]| {
        let mut map: HashMap<&str, Vec<usize>> = HashMap::new();
        for &n in changes {
            map.entry(stream[n]["id"].as_str().unwrap_or_default())
                .or_default()
                .push(n);
        }
        map
    };
    let (got, expected) = (by_entity(got), by_entity(expected));
    got.len() == expected.len()
        && expected.iter().all(|(id, want)| {
            got.get(id).is_some_and(|have| {
                let mut rest = want.iter();
                have.last() == want.last() && have.iter().all(|n| rest.any(|w| w == n))
            })
        })
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let at = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[at]
}

/// `VmRSS` and user + system CPU of this process, from `/proc` (Linux runners).
fn usage() -> (u64, Duration) {
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .unwrap_or(0);
    let cpu = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let after = s.rsplit_once(')')?.1;
            let fields: Vec<&str> = after.split_whitespace().collect();
            let ticks: u64 =
                fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
            // ponytail: the usual 100 ticks per second; sysconf would be a libc dependency.
            Some(Duration::from_millis(ticks * 10))
        })
        .unwrap_or_default();
    (rss, cpu)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_socket_receives_exactly_what_its_filter_admits() {
    let sockets = env("RIG_SOCKETS", 100);
    let changes = env("RIG_CHANGES", 1000);
    let rate = env("RIG_RATE", 200).max(1);
    // SAFETY of the measurement: the cap is read once, before the first connection.
    std::env::set_var("JC_GATEWAY_WS_PER_SPACE", (sockets + 1).to_string());

    let realm = common::Realm::new();
    let (upstream, push) = broker().await;
    let (endpoints, ..) = store::load(&repo()).expect("the repository loads");
    let gateway = Gateway::new(
        Broker::new(format!("http://{upstream}")),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve(endpoints)
    .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None);
    let at = serve(router(Arc::new(gateway))).await;

    let mut rng = Rng(4133);
    let filters: Vec<Value> = (0..sockets).map(|_| filter(&mut rng)).collect();
    let stream: Vec<Value> = (0..changes).map(|n| change(&mut rng, n)).collect();
    let expected = ground_truth(&filters, &stream);

    let (before_rss, before_cpu) = usage();
    let sent_at: Arc<std::sync::Mutex<Vec<Option<Instant>>>> =
        Arc::new(std::sync::Mutex::new(vec![None; changes]));
    let mut clients = Vec::new();
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel::<Result<(), String>>(sockets.max(1));
    for (i, body) in filters.iter().enumerate() {
        // Ten connections per caller (EP-107): each client is its own address.
        let mut request = format!("ws://{at}/api/endpoint/{SLUG}/ws")
            .into_client_request()
            .expect("a request");
        request.headers_mut().insert(
            "x-forwarded-for",
            format!("10.{}.{}.{}", i / 65536 % 256, i / 256 % 256, i % 256)
                .parse()
                .expect("a value"),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("upgraded");
        let subscribe = json!({"metadata": {"x-op": "x-subscribe", "x-id": "s"}, "body": body});
        socket
            .send(Message::text(subscribe.to_string()))
            .await
            .expect("sent");
        let ready = ready_tx.clone();
        let sent_at = Arc::clone(&sent_at);
        clients.push(tokio::spawn(async move {
            let mut got: Vec<usize> = Vec::new();
            let mut latencies: Vec<Duration> = Vec::new();
            let mut gaps = 0;
            // A client is done once no text arrived for 20 s; the server's pings do not count.
            let mut last = Instant::now();
            while let Ok(Some(Ok(message))) = tokio::time::timeout(
                Duration::from_secs(20).saturating_sub(last.elapsed()),
                socket.next(),
            )
            .await
            {
                let Message::Text(text) = message else {
                    continue;
                };
                last = Instant::now();
                let message: Value = serde_json::from_str(text.as_str()).expect("JSON");
                match message["metadata"]["x-op"].as_str() {
                    Some("x-result") => {
                        let answer = if message["metadata"]["x-status"] == 201 {
                            Ok(())
                        } else {
                            Err(message.to_string())
                        };
                        let _ = ready.send(answer).await;
                    }
                    Some("x-notification") => {
                        let n = message["body"]["data"][0]["rig"]["value"]
                            .as_u64()
                            .and_then(|n| usize::try_from(n).ok())
                            .expect("the change's number");
                        if let Some(at) = sent_at.lock().expect("stamps")[n] {
                            latencies.push(at.elapsed());
                        }
                        got.push(n);
                    }
                    Some("x-gap") => gaps += 1,
                    _ => {}
                }
            }
            (got, latencies, gaps)
        }));
    }
    drop(ready_tx);
    for _ in 0..sockets {
        let answer = tokio::time::timeout(Duration::from_secs(60), ready_rx.recv()).await;
        match answer {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(refused))) => panic!("a subscription was refused: {refused}"),
            Ok(None) | Err(_) => panic!("a client never had its subscription answered"),
        }
    }
    // The upstream's subscription is answered before the first change is pushed.
    for _ in 0..250 {
        if push.receiver_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = Instant::now();
    #[allow(clippy::cast_precision_loss)]
    let every = Duration::from_secs_f64(1.0 / rate as f64);
    for (n, entity) in stream.iter().enumerate() {
        sent_at.lock().expect("stamps")[n] = Some(Instant::now());
        let line = json!({"metadata": {"x-op": "x-notification", "x-seq": n + 1},
            "body": {"type": "Notification", "data": [entity]}});
        push.send(Arc::new(line.to_string())).expect("an upstream");
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            started + every * u32::try_from(n + 1).unwrap_or(u32::MAX),
        ))
        .await;
    }
    let streamed = started.elapsed();
    // One broker subscription for every client of the Endpoint (EP-114).
    assert_eq!(push.receiver_count(), 1, "one upstream per Endpoint");

    let mut latencies = Vec::new();
    let mut wrong = Vec::new();
    let mut delivered = 0;
    let mut coalesced = 0;
    let mut gaps = 0;
    for (i, client) in clients.into_iter().enumerate() {
        let (got, mut seen, gapped) = client.await.expect("the client ran");
        gaps += gapped;
        delivered += got.len();
        if got != expected[i] {
            if holds_coalesced(&got, &expected[i], &stream) {
                coalesced += expected[i].len() - got.len();
            } else {
                wrong.push((i, filters[i].clone(), expected[i].len(), got.len()));
            }
        }
        latencies.append(&mut seen);
    }
    let (after_rss, after_cpu) = usage();
    latencies.sort_unstable();
    let report = json!({
        "sockets": sockets,
        "changes": changes,
        "rate": rate,
        "notifications": delivered,
        "wrong_sockets": wrong.len(),
        "coalesced": coalesced,
        "gaps": gaps,
        "latency_p50_ms": percentile(&latencies, 0.50).as_secs_f64() * 1000.0,
        "latency_p99_ms": percentile(&latencies, 0.99).as_secs_f64() * 1000.0,
        "rss_kib_before": before_rss,
        "rss_kib_after": after_rss,
        "cpu_s": (after_cpu.saturating_sub(before_cpu)).as_secs_f64(),
        "stream_s": streamed.as_secs_f64(),
    });
    println!("ws-fanout rig: {report}");
    if let Ok(path) = std::env::var("RIG_REPORT") {
        std::fs::write(path, report.to_string()).expect("the report is written");
    }
    let shown: BTreeMap<usize, String> = wrong
        .iter()
        .take(5)
        .map(|(i, f, want, got)| (*i, format!("{f} wanted {want} got {got}")))
        .collect();
    assert!(
        wrong.is_empty(),
        "{} sockets differ from the ground truth: {shown:?}",
        wrong.len()
    );
    assert_eq!(gaps, 0, "no client was told of a gap");
    assert!(delivered > 0, "the stream notified something");
}
