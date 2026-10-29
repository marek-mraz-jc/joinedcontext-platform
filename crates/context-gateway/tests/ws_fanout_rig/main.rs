//! The `ws-fanout` rig (ADR-N-049 §2.9, EP-115, T-4133): N WebSocket clients with random filters
//! on one Endpoint, a generated Vehicle stream pushed through a stub broker, and every client's
//! notifications compared with the ground truth computed offline from the same stream by
//! `antares-matcher` on every (filter, change) pair: no miss, no extra, the stream's order; a
//! client the gateway coalesced for (it fell behind) still gets every entity's last state.
//!
//! `RIG_SOCKETS` (default 100), `RIG_CHANGES` (default 1000) and `RIG_RATE` (changes per second,
//! default 20, the rate of dev's Helsinki transit stream) size a run; `RIG_REPORT` names a file
//! the numbers are written to as JSON. The gateway runs in this process, so the CPU and memory reported are the gateway's and the
//! clients' together. Run it on a GitHub runner (`ws-fanout-rig.yml`), never on the shared host.

#[path = "../common/mod.rs"]
mod common;
#[path = "../rig/mod.rs"]
mod rig;

use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::fanout::{changed, compile, hub::core};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const VEHICLES: u64 = 50;

/// What a client reads of every frame. The clients share the runner's cores with the gateway, so
/// each frame is read into these few fields instead of a whole `Value` (T-5351).
#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow)]
    metadata: Meta<'a>,
}

#[derive(Deserialize)]
struct Meta<'a> {
    #[serde(rename = "x-op", borrow)]
    op: Option<&'a str>,
    #[serde(rename = "x-status")]
    status: Option<u16>,
    #[serde(rename = "x-reason", borrow)]
    reason: Option<&'a str>,
}

/// An `x-notification`: the change's number each rig entity carries.
#[derive(Deserialize)]
struct Note {
    body: NoteBody,
}

#[derive(Deserialize)]
struct NoteBody {
    data: Vec<Datum>,
}

#[derive(Deserialize)]
struct Datum {
    rig: Number,
}

#[derive(Deserialize)]
struct Number {
    value: usize,
}
const MODES: [&str; 3] = ["bus", "tram", "metro"];

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_socket_receives_exactly_what_its_filter_admits() {
    let sockets = env("RIG_SOCKETS", 100);
    let changes = env("RIG_CHANGES", 1000);
    let rate = env("RIG_RATE", 20).max(1);
    // SAFETY of the measurement: the cap is read once, before the first connection.
    std::env::set_var("JC_GATEWAY_WS_PER_SPACE", (sockets + 1).to_string());

    let realm = common::Realm::new();
    let (upstream, push) = rig::broker().await;
    let (endpoints, ..) = store::load(&rig::repo()).expect("the repository loads");
    let gateway = Gateway::new(
        Broker::new(format!("http://{upstream}")),
        Box::new(PolicyPdp),
        "hel.fi",
    )
    .serve(endpoints)
    .authenticate(Arc::new(realm.verifier()), ServiceAccounts::new(), None);
    let at = rig::serve(router(Arc::new(gateway))).await;

    let mut rng = Rng(4133);
    let filters: Vec<Value> = (0..sockets).map(|_| filter(&mut rng)).collect();
    let stream: Vec<Value> = (0..changes).map(|n| change(&mut rng, n)).collect();
    let expected = ground_truth(&filters, &stream);

    let (before_rss, before_cpu) = rig::usage();
    let received = Arc::new(AtomicUsize::new(0));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(());
    // Each change's push time, written once and read by every client without a lock: one mutex
    // over 5 000 clients parked the runner's threads and cut the clients' throughput (T-5351).
    let sent_at: Arc<Vec<OnceLock<Instant>>> =
        Arc::new((0..changes).map(|_| OnceLock::new()).collect());
    let mut clients = Vec::new();
    // When each socket the server ended ended, for the report.
    let connecting = Instant::now();
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel::<Result<(), String>>(sockets.max(1));
    for (i, body) in filters.iter().enumerate() {
        // Ten connections per caller (EP-107): each client is its own address.
        let mut request = format!("ws://{at}/api/endpoint/{}/ws", rig::SLUG)
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
        let received = Arc::clone(&received);
        let mut stop = stop_rx.clone();
        clients.push(tokio::spawn(async move {
            let mut got: Vec<usize> = Vec::new();
            let mut latencies: Vec<Duration> = Vec::new();
            let mut gaps = 0;
            // How the server ended the socket, when it did before the rig stopped it.
            let mut ended: Option<String> = None;
            // A client reads until the rig says every socket has drained (see below).
            loop {
                let message = tokio::select! {
                    read = socket.next() => match read {
                        Some(Ok(Message::Close(frame))) => {
                            ended = Some(frame.map_or_else(
                                || "close".to_owned(),
                                |f| format!("close {} {}", u16::from(f.code), f.reason),
                            ));
                            break;
                        }
                        Some(Ok(message)) => message,
                        Some(Err(error)) => {
                            ended = Some(format!("error {error}"));
                            break;
                        }
                        None => {
                            ended = Some("eof".to_owned());
                            break;
                        }
                    },
                    _ = stop.changed() => break,
                };
                let Message::Text(text) = message else {
                    continue;
                };
                received.fetch_add(1, Ordering::Relaxed);
                let head: Head = serde_json::from_str(text.as_str()).expect("JSON");
                match head.metadata.op {
                    Some("x-result") => {
                        let answer = if head.metadata.status == Some(201) {
                            Ok(())
                        } else {
                            Err(text.to_string())
                        };
                        let _ = ready.send(answer).await;
                    }
                    Some("x-notification") => {
                        let note: Note = serde_json::from_str(text.as_str()).expect("a rig entity");
                        let n = note
                            .body
                            .data
                            .first()
                            .expect("the change's number")
                            .rig
                            .value;
                        if let Some(at) = sent_at[n].get() {
                            latencies.push(at.elapsed());
                        }
                        got.push(n);
                    }
                    Some("x-gap") => gaps += 1,
                    Some("x-closing") => {
                        ended = Some(format!(
                            "x-closing {}",
                            head.metadata.reason.unwrap_or_default()
                        ));
                    }
                    _ => {}
                }
            }
            let ended = ended.map(|reason| (reason, connecting.elapsed().as_secs()));
            (got, latencies, gaps, ended)
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
        let _ = sent_at[n].set(Instant::now());
        let line = json!({"metadata": {"x-op": "x-notification", "x-seq": n + 1},
            "body": {"type": "Notification", "data": [entity]}});
        push.send(Arc::new(line.to_string())).expect("an upstream");
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            started + every * u32::try_from(n + 1).unwrap_or(u32::MAX),
        ))
        .await;
    }
    let streamed = started.elapsed();
    // Drained: no client received anything for 10 s. A backlog the gateway is still working
    // off keeps the count growing however long it takes, so a slow run is measured, not cut.
    let mut seen = received.load(Ordering::Relaxed);
    let mut still = 0;
    while still < 10 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = received.load(Ordering::Relaxed);
        still = if now == seen { still + 1 } else { 0 };
        seen = now;
    }
    let drained = started.elapsed() - Duration::from_secs(10);
    let _ = stop_tx.send(());
    // One broker subscription for every client of the Endpoint (EP-114).
    assert_eq!(push.receiver_count(), 1, "one upstream per Endpoint");

    let mut latencies = Vec::new();
    let mut wrong = Vec::new();
    let mut delivered = 0;
    let mut coalesced = 0;
    let mut gaps = 0;
    let mut ends: BTreeMap<String, usize> = BTreeMap::new();
    // The first sockets ended per reason: (index, seconds after the first connection, received).
    let mut ended_first: BTreeMap<String, Vec<(usize, u64, usize)>> = BTreeMap::new();
    for (i, client) in clients.into_iter().enumerate() {
        let (got, mut seen, gapped, ended) = client.await.expect("the client ran");
        gaps += gapped;
        if let Some((ended, at)) = ended {
            *ends.entry(ended.clone()).or_default() += 1;
            let first = ended_first.entry(ended).or_default();
            if first.len() < 20 {
                first.push((i, at, got.len()));
            }
        }
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
    let (after_rss, after_cpu) = rig::usage();
    latencies.sort_unstable();
    let report = json!({
        "sockets": sockets,
        "changes": changes,
        "rate": rate,
        "notifications": delivered,
        "wrong_sockets": wrong.len(),
        "coalesced": coalesced,
        "gaps": gaps,
        "ended_by_server": ends,
        "latency_p50_ms": rig::percentile(&latencies, 0.50).as_secs_f64() * 1000.0,
        "latency_p99_ms": rig::percentile(&latencies, 0.99).as_secs_f64() * 1000.0,
        "rss_kib_before": before_rss,
        "rss_kib_after": after_rss,
        "cpu_s": (after_cpu.saturating_sub(before_cpu)).as_secs_f64(),
        "stream_s": streamed.as_secs_f64(),
        "drained_s": drained.as_secs_f64(),
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
        ends.is_empty(),
        "the server ended sockets: {ends:?}, first (socket, s, received): {ended_first:?}"
    );
    assert!(
        wrong.is_empty(),
        "{} sockets differ from the ground truth: {shown:?}",
        wrong.len()
    );
    assert_eq!(gaps, 0, "no client was told of a gap");
    assert!(delivered > 0, "the stream notified something");
}
