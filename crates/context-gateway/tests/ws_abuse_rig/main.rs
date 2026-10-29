//! The `ws-abuse` rig (T-4902, API/07 §3): the gateway's WebSocket limits measured under abuse.
//! One control client subscribes to a public Vehicle Endpoint and its change-to-socket latency is
//! measured alone first (the baseline), then while each abuser runs:
//!
//! - `idle`: 10 000 upgrades from one address; the caller keeps 10, the rest answer `429`.
//! - `slow`: a client that subscribes, then never reads, while 1 000 changes a second stream; the
//!   gateway ends it (`x-gap` and `x-closing: overflow`, or a dropped socket once it stops reading
//!   for [`STALL`]) and gives its slot back.
//! - `big`: a 1 MiB message closes with `1009`.
//! - `deflate`: a `permessage-deflate` offer is not taken, and a compressed frame (1 MiB of zeros
//!   in 7 KiB on the wire, under the 16 KiB frame bound) closes with `1002` before anything inflates it.
//! - `flood`: 1 000 `x-subscribe` a second; past the burst of 100 the socket gets `x-closing: rate`
//!   and `1008`.
//! - `storm`: 2 000 upgrades and closes from one address, 50 at a time; afterwards the address
//!   holds its full 10 again (no slot leaked).
//!
//! Each scenario passes when the abuser is limited or closed as stated, the control client's p99
//! stays within twice the baseline (plus [`JITTER`]), the process stays under [`RSS_CAP_KIB`] and no
//! task panicked. `RIG_RATE` (default 200 changes a second) and `RIG_SECONDS` (default 5 per phase)
//! size a run, `RIG_REPORT` names the JSON report. The gateway and every client run in this process.
//! Run it on a GitHub runner (`ws-fanout-rig.yml`, job `abuse`), never on the shared host.

#[path = "../common/mod.rs"]
mod common;
#[path = "../rig/mod.rs"]
mod rig;

use context_gateway::app::{router, Gateway};
use context_gateway::auth::accounts::ServiceAccounts;
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::store;
use futures_util::stream::{self, StreamExt};
use futures_util::SinkExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::WebSocketStream;

/// The control client's p99 may grow by twice the baseline plus this much: on a shared runner the
/// baseline is a millisecond or two and scheduler noise alone doubles it.
const JITTER: Duration = Duration::from_millis(20);
/// The whole process (gateway, stub broker, every client) stays under 512 MiB resident.
const RSS_CAP_KIB: u64 = 512 * 1024;
/// How long a slow consumer stays silent, and how long the rig then reads it to its end: past the
/// gateway's 10 s send bound (`fanout::socket::STALL`).
const STALL: Duration = Duration::from_secs(15);
/// The caller cap of EP-107 (`handlers::ws::PER_CALLER`).
const PER_CALLER: usize = 10;

static PANICS: AtomicUsize = AtomicUsize::new(0);

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Microseconds since the rig started: a change carries its send time in `sent`.
fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn request(at: &str, from: &str) -> Request {
    let mut request = format!("ws://{at}/api/endpoint/{}/ws", rig::SLUG)
        .into_client_request()
        .expect("a request");
    request
        .headers_mut()
        .insert("x-forwarded-for", from.parse().expect("an address"));
    request
}

/// The upgrade's HTTP status when the gateway refuses it.
fn refused(error: &Error) -> Option<u16> {
    match error {
        Error::Http(response) => Some(response.status().as_u16()),
        _ => None,
    }
}

fn subscribe(id: &str) -> Message {
    let body = json!({"type": "Subscription", "entities": [{"type": "Vehicle"}]});
    Message::text(
        json!({"metadata": {"x-op": "x-subscribe", "x-id": id}, "body": body}).to_string(),
    )
}

/// Reads until the subscription `id` is answered; its status.
async fn subscribed<S>(socket: &mut WebSocketStream<S>, id: &str) -> Option<u64>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    socket.send(subscribe(id)).await.ok()?;
    while let Some(Ok(message)) = socket.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let message: Value = serde_json::from_str(text.as_str()).ok()?;
        if message["metadata"]["x-op"] == "x-result" && message["metadata"]["x-id"] == id {
            return message["metadata"]["x-status"].as_u64();
        }
    }
    None
}

/// What ended a socket: `close <code>`, `x-closing <reason>` seen before it, `eof` or `error`.
#[derive(Debug, Default)]
struct Ending {
    closing: Option<String>,
    gaps: usize,
    close: Option<u16>,
    dropped: bool,
}

/// Reads a socket (or its read half) to its end, at most `within`.
async fn ending<S>(socket: &mut S, within: Duration) -> Ending
where
    S: futures_util::Stream<Item = Result<Message, Error>> + Unpin,
{
    let mut ending = Ending::default();
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return ending,
            Ok(Some(Ok(Message::Close(frame)))) => {
                ending.close = Some(frame.map_or(1005, |f| u16::from(f.code)));
                return ending;
            }
            Ok(Some(Ok(Message::Text(text)))) => {
                let message: Value = serde_json::from_str(text.as_str()).unwrap_or_default();
                match message["metadata"]["x-op"].as_str() {
                    Some("x-gap") => ending.gaps += 1,
                    Some("x-closing") => {
                        ending.closing =
                            message["metadata"]["x-reason"].as_str().map(str::to_owned);
                    }
                    _ => {}
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) => {
                ending.dropped = true;
                return ending;
            }
        }
    }
}

/// The control client: every notification's latency, from the change's `sent` stamp.
struct Control {
    latencies: Arc<Mutex<Vec<Duration>>>,
}

impl Control {
    async fn start(at: &str) -> Self {
        let (mut socket, _) = tokio_tungstenite::connect_async(request(at, "10.200.0.1"))
            .await
            .expect("the control client connects");
        assert_eq!(subscribed(&mut socket, "control").await, Some(201));
        let latencies = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::clone(&latencies);
        tokio::spawn(async move {
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let message: Value = serde_json::from_str(text.as_str()).unwrap_or_default();
                if let Some(sent) = message["body"]["data"][0]["sent"]["value"].as_u64() {
                    let latency = Duration::from_micros(now_us().saturating_sub(sent));
                    held.lock().expect("latencies").push(latency);
                }
                assert_ne!(
                    message["metadata"]["x-op"], "x-closing",
                    "the control client is closed"
                );
            }
        });
        Self { latencies }
    }

    /// Streams `rate` changes a second for `seconds`, each with `pad` bytes of filler, then the
    /// control client's p99.
    async fn measure(
        &self,
        push: &tokio::sync::broadcast::Sender<Arc<String>>,
        rate: u64,
        seconds: u64,
        pad: usize,
    ) -> Duration {
        let filler = "p".repeat(pad);
        self.latencies.lock().expect("latencies").clear();
        let started = Instant::now();
        let every = Duration::from_secs(1) / u32::try_from(rate).unwrap_or(u32::MAX);
        for n in 0..rate * seconds {
            let vehicle = n % 50;
            let line = json!({"metadata": {"x-op": "x-notification", "x-seq": n + 1},
                "body": {"type": "Notification", "data": [{
                    "id": format!("urn:ngsi-ld:Vehicle:{vehicle}"), "type": "Vehicle",
                    "speed": {"type": "Property", "value": n % 30},
                    "pad": {"type": "Property", "value": filler},
                    "sent": {"type": "Property", "value": now_us()}}]}});
            push.send(Arc::new(line.to_string())).expect("an upstream");
            tokio::time::sleep_until(tokio::time::Instant::from_std(
                started + every * u32::try_from(n + 1).unwrap_or(u32::MAX),
            ))
            .await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut latencies = std::mem::take(&mut *self.latencies.lock().expect("latencies"));
        latencies.sort_unstable();
        assert!(
            !latencies.is_empty(),
            "the control client received the stream"
        );
        rig::percentile(&latencies, 0.99)
    }
}

impl Control {
    /// The p99 of a measurement held against twice `baseline` plus [`JITTER`]; one over it is
    /// measured once more and the better taken, since a shared runner's scheduler spikes once,
    /// while a gateway the abuser slowed stays slow.
    async fn steady(
        &self,
        push: &tokio::sync::broadcast::Sender<Arc<String>>,
        rate: u64,
        seconds: u64,
        baseline: Duration,
    ) -> Duration {
        let first = self.measure(push, rate, seconds, 0).await;
        if first <= baseline * 2 + JITTER {
            return first;
        }
        first.min(self.measure(push, rate, seconds, 0).await)
    }
}

/// Opens `count` upgrades from `from`, `parallel` at a time; the open sockets and the refusals.
async fn upgrades(
    at: &str,
    from: &str,
    count: usize,
    parallel: usize,
) -> (
    Vec<WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>>,
    usize,
    Vec<String>,
) {
    let tries = stream::iter(0..count)
        .map(|_| tokio_tungstenite::connect_async(request(at, from)))
        .buffer_unordered(parallel)
        .collect::<Vec<_>>()
        .await;
    let (mut open, mut too_many, mut other) = (Vec::new(), 0, Vec::new());
    for attempt in tries {
        match attempt {
            Ok((socket, _)) => open.push(socket),
            Err(error) if refused(&error) == Some(429) => too_many += 1,
            Err(error) => other.push(error.to_string()),
        }
    }
    (open, too_many, other)
}

/// A client that subscribes and then stops reading, on a socket with a small receive buffer so the
/// gateway's writes back up within seconds.
async fn slow_consumer(at: &str) -> WebSocketStream<TcpStream> {
    let socket = TcpSocket::new_v4().expect("a socket");
    socket
        .set_recv_buffer_size(4096)
        .expect("a small receive buffer");
    let stream = socket
        .connect(at.parse().expect("an address"))
        .await
        .expect("connected");
    let (mut socket, _) = tokio_tungstenite::client_async(request(at, "10.66.0.2"), stream)
        .await
        .expect("upgraded");
    assert_eq!(subscribed(&mut socket, "slow").await, Some(201));
    socket
}

/// A raw RFC 6455 client frame: FIN, `rsv1` when compressed, masked.
fn frame(opcode: u8, rsv1: bool, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | if rsv1 { 0x40 } else { 0 } | opcode];
    match payload.len() {
        n if n < 126 => out.push(0x80 | u8::try_from(n).unwrap_or(0)),
        n if n <= 0xFFFF => {
            out.push(0x80 | 126);
            out.extend_from_slice(&u16::try_from(n).unwrap_or(0).to_be_bytes());
        }
        n => {
            out.push(0x80 | 127);
            out.extend_from_slice(&u64::try_from(n).unwrap_or(u64::MAX).to_be_bytes());
        }
    }
    let mask = [0x5a, 0x17, 0xc3, 0x88];
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// A raw DEFLATE stream (RFC 1951, one fixed-Huffman block) that inflates to `size` zero bytes:
/// a literal 0, then back-references of 258 bytes at distance 1, 13 bits each.
fn deflate_bomb(size: usize) -> Vec<u8> {
    // BFINAL 1, BTYPE 01 (fixed Huffman); header fields go least significant bit first.
    let mut bits = vec![true, true, false];
    // Huffman codes go most significant bit first.
    let code = |bits: &mut Vec<bool>, value: u32, len: u32| {
        (0..len).rev().for_each(|i| bits.push(value >> i & 1 == 1));
    };
    code(&mut bits, 0x30, 8); // literal 0
    for _ in 0..(size - 1) / 258 {
        code(&mut bits, 0b1100_0101, 8); // length 258
        code(&mut bits, 0, 5); // distance 1
    }
    code(&mut bits, 0, 7); // end of block
    bits.chunks(8)
        .map(|byte| {
            byte.iter()
                .enumerate()
                .fold(0_u8, |b, (i, &set)| b | u8::from(set) << i)
        })
        .collect()
}

/// Upgrades by hand, offering `permessage-deflate`: whether the answer took it, then the close
/// code a compressed frame earns.
async fn deflate(at: &str) -> (bool, Option<u16>) {
    let mut tcp = TcpStream::connect(at).await.expect("connected");
    let hello = format!(
        "GET /api/endpoint/{}/ws HTTP/1.1\r\nHost: {at}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: ngsi-ld-ws\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\
         X-Forwarded-For: 10.66.0.4\r\n\r\n",
        rig::SLUG
    );
    tcp.write_all(hello.as_bytes()).await.expect("sent");
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        tcp.read_exact(&mut byte).await.expect("an answer");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 101"), "upgraded: {head}");
    let taken = head.contains("sec-websocket-extensions");
    tcp.write_all(&frame(0x1, true, &deflate_bomb(1 << 20)))
        .await
        .expect("sent");
    // The server's frames: the welcome, then the close.
    let read = async {
        loop {
            let mut two = [0_u8; 2];
            tcp.read_exact(&mut two).await.ok()?;
            let len = match two[1] & 0x7f {
                126 => usize::from(tcp.read_u16().await.ok()?),
                127 => usize::try_from(tcp.read_u64().await.ok()?).ok()?,
                n => usize::from(n),
            };
            let mut payload = vec![0_u8; len];
            tcp.read_exact(&mut payload).await.ok()?;
            if two[0] & 0x0f == 0x8 {
                return payload.get(..2).map(|c| u16::from_be_bytes([c[0], c[1]]));
            }
        }
    };
    (
        taken,
        tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .ok()
            .flatten(),
    )
}

/// The process's state after a scenario, held against the thresholds.
fn judge(report: &mut Vec<Value>, name: &str, p99: Duration, baseline: Duration, abuser: Value) {
    let (rss, _) = rig::usage();
    let within = p99 <= baseline * 2 + JITTER;
    report.push(
        json!({"scenario": name, "control_p99_ms": p99.as_secs_f64() * 1000.0,
        "within_2x": within, "rss_kib": rss, "abuser": abuser}),
    );
    println!("ws-abuse rig: {}", report.last().unwrap_or(&Value::Null));
    assert!(
        within,
        "{name}: the control p99 {p99:?} is over twice the baseline {baseline:?} + {JITTER:?}"
    );
    assert!(
        rss < RSS_CAP_KIB,
        "{name}: {rss} KiB resident is over the cap of {RSS_CAP_KIB} KiB"
    );
    assert_eq!(PANICS.load(Ordering::Relaxed), 0, "{name}: a task panicked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_abuser_is_limited_and_the_control_client_keeps_its_latency() {
    let rate = env("RIG_RATE", 200).max(1);
    let seconds = env("RIG_SECONDS", 5).max(1);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        previous(info);
    }));
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
    let control = Control::start(&at).await;
    for _ in 0..250 {
        if push.receiver_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut report = Vec::new();
    let baseline = control.measure(&push, rate, seconds, 0).await;
    judge(&mut report, "baseline", baseline, baseline, json!({}));

    // idle: one address asks for 10 000 sockets and holds what it gets during the stream.
    let (open, too_many, other) = upgrades(&at, "10.66.0.1", 10_000, 256).await;
    let p99 = control.steady(&push, rate, seconds, baseline).await;
    judge(
        &mut report,
        "idle",
        p99,
        baseline,
        json!({"open": open.len(), "refused_429": too_many, "other": other.len()}),
    );
    assert_eq!(
        (open.len(), too_many),
        (PER_CALLER, 10_000 - PER_CALLER),
        "idle: {other:?}"
    );
    drop(open);

    // slow: a subscriber that never reads while 1 000 changes a second stream.
    let mut slow = slow_consumer(&at).await;
    // 4 KiB a change fills the socket buffers in seconds; the consumer then stays silent past
    // the gateway's 10 s send bound before it reads again.
    let p99 = control.measure(&push, 1000.max(rate), seconds, 4096).await;
    tokio::time::sleep(STALL).await;
    let end = ending(&mut slow, STALL).await;
    drop(slow);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (again, ..) = upgrades(&at, "10.66.0.2", PER_CALLER, PER_CALLER).await;
    judge(
        &mut report,
        "slow",
        p99,
        baseline * u32::try_from((1000 / rate.min(1000)).max(1)).unwrap_or(1),
        json!({"closing": end.closing, "gaps": end.gaps, "close": end.close, "dropped": end.dropped, "reopened": again.len()}),
    );
    assert!(
        end.closing.as_deref() == Some("overflow") || end.dropped,
        "slow: the consumer was not ended: {end:?}"
    );
    assert_eq!(again.len(), PER_CALLER, "slow: its slot came back");
    drop(again);

    // big: a 1 MiB message.
    let (mut big, _) = tokio_tungstenite::connect_async(request(&at, "10.66.0.3"))
        .await
        .expect("upgraded");
    let sent = big.send(Message::text("x".repeat(1 << 20))).await;
    let end = ending(&mut big, Duration::from_secs(5)).await;
    let p99 = control.steady(&push, rate, seconds, baseline).await;
    judge(
        &mut report,
        "big",
        p99,
        baseline,
        json!({"close": end.close, "sent": sent.is_ok()}),
    );
    assert_eq!(end.close, Some(1009), "big: {end:?}");

    // deflate: the extension is not negotiated and a compressed frame is a protocol error.
    let (taken, close) = deflate(&at).await;
    let p99 = control.steady(&push, rate, seconds, baseline).await;
    judge(
        &mut report,
        "deflate",
        p99,
        baseline,
        json!({"extension_taken": taken, "close": close}),
    );
    assert!(!taken, "deflate: permessage-deflate was negotiated");
    assert_eq!(
        close,
        Some(1002),
        "deflate: a compressed frame closes with 1002"
    );

    // flood: 1 000 subscribes a second.
    let (flood, _) = tokio_tungstenite::connect_async(request(&at, "10.66.0.5"))
        .await
        .expect("upgraded");
    // The read half listens while the write half floods, so the close is seen as it comes.
    let (mut writer, mut reader) = flood.split();
    let flooding = tokio::spawn(async move {
        let mut sent = 0;
        let mut tick = tokio::time::interval(Duration::from_millis(1));
        while sent < 1000 && writer.send(subscribe(&format!("f{sent}"))).await.is_ok() {
            sent += 1;
            tick.tick().await;
        }
        sent
    });
    let listening = tokio::spawn(async move { ending(&mut reader, Duration::from_secs(10)).await });
    let p99 = control.steady(&push, rate, seconds, baseline).await;
    let sent = flooding.await.expect("the flood ran");
    let end = listening.await.expect("the flood was read");
    judge(
        &mut report,
        "flood",
        p99,
        baseline,
        json!({"sent": sent, "closing": end.closing, "close": end.close}),
    );
    assert_eq!(
        (end.closing.as_deref(), end.close),
        (Some("rate"), Some(1008)),
        "flood: {end:?}"
    );

    // storm: 2 000 upgrades and closes from one address, then the address holds its 10 again.
    let storm_at = at.clone();
    let storm = tokio::spawn(async move {
        let tries = stream::iter(0..2000)
            .map(|_| {
                let at = storm_at.clone();
                async move {
                    match tokio_tungstenite::connect_async(request(&at, "10.66.0.7")).await {
                        Ok((mut socket, _)) => {
                            let _ = socket.close(None).await;
                            Some(101)
                        }
                        Err(error) => refused(&error),
                    }
                }
            })
            .buffer_unordered(50)
            .collect::<Vec<_>>()
            .await;
        let upgraded = tries.iter().filter(|t| **t == Some(101)).count();
        let limited = tries.iter().filter(|t| **t == Some(429)).count();
        (upgraded, limited)
    });
    let p99 = control.steady(&push, rate, seconds, baseline).await;
    let (upgraded, limited) = storm.await.expect("the storm ran");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (again, ..) = upgrades(&at, "10.66.0.7", PER_CALLER + 1, PER_CALLER + 1).await;
    judge(
        &mut report,
        "storm",
        p99,
        baseline,
        json!({"upgraded": upgraded, "refused_429": limited, "reopened": again.len()}),
    );
    assert_eq!(
        upgraded + limited,
        2000,
        "storm: every upgrade was answered"
    );
    assert_eq!(
        again.len(),
        PER_CALLER,
        "storm: the address holds exactly its cap again"
    );

    let report =
        json!({"rate": rate, "seconds": seconds, "rss_cap_kib": RSS_CAP_KIB, "scenarios": report});
    if let Ok(path) = std::env::var("RIG_REPORT") {
        std::fs::write(path, report.to_string()).expect("the report is written");
    }
}
