//! A test App: `/api/hello`, `/api/state` (a counter that a fresh instance starts at one),
//! `/api/loop` (never ends), `/api/alloc` (asks for 128 MiB), `/api/fetch?<url>` (an outgoing
//! GET, answering the status or the error), `/api/sql` (the host's `jc:app/sql`), `/api/reach`
//! (what the component gets of the environment, the files and the sockets); the hostile ones of
//! T-4777: `/api/recurse` (unbounded recursion), `/api/passwd` (a host file by path),
//! `/api/answer-then-spin` (answers, then never ends), `/api/upload?<MiB>` (an outgoing POST of
//! that many MiB to the gateway); and the jobs of `wit/jobs.wit`.

use std::sync::atomic::{AtomicU64, Ordering};

use wasip2::http::outgoing_handler;
use wasip2::http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingRequest, OutgoingResponse, ResponseOutparam, Scheme};

wit_bindgen::generate!({
    path: "wit",
    world: "jc:test-guest/guest",
    generate_all,
});

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct App;

fn answer(out: ResponseOutparam, status: u16, text: &str) {
    let response = OutgoingResponse::new(Fields::new());
    let _ = response.set_status_code(status);
    let body = response.body().expect("body");
    ResponseOutparam::set(out, Ok(response));
    let stream = body.write().expect("stream");
    let _ = stream.blocking_write_and_flush(text.as_bytes());
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

/// Everything WASI would let a component reach beyond HTTP (AP-147): the environment, the
/// preopened directories, a name lookup, a TCP connection and a UDP bind, each with what it got.
fn reach(port: u16) -> String {
    use wasip2::sockets::network::{IpAddressFamily, IpSocketAddress, Ipv4SocketAddress};
    let env = wasip2::cli::environment::get_environment().len();
    let dirs = wasip2::filesystem::preopens::get_directories().len();
    let network = wasip2::sockets::instance_network::instance_network();
    let lookup = match wasip2::sockets::ip_name_lookup::resolve_addresses(&network, "localhost") {
        Err(code) => format!("refused {code:?}"),
        Ok(stream) => {
            stream.subscribe().block();
            match stream.resolve_next_address() {
                Ok(Some(_)) => "resolved".to_owned(),
                Ok(None) => "empty".to_owned(),
                Err(code) => format!("refused {code:?}"),
            }
        }
    };
    let loopback = |port| IpSocketAddress::Ipv4(Ipv4SocketAddress { port, address: (127, 0, 0, 1) });
    let tcp = match wasip2::sockets::tcp_create_socket::create_tcp_socket(IpAddressFamily::Ipv4) {
        Err(code) => format!("refused {code:?}"),
        Ok(socket) => match socket.start_connect(&network, loopback(port)) {
            Err(code) => format!("refused {code:?}"),
            Ok(()) => {
                socket.subscribe().block();
                match socket.finish_connect() {
                    Ok(_) => "connected".to_owned(),
                    Err(code) => format!("refused {code:?}"),
                }
            }
        },
    };
    let udp = match wasip2::sockets::udp_create_socket::create_udp_socket(IpAddressFamily::Ipv4) {
        Err(code) => format!("refused {code:?}"),
        Ok(socket) => match socket.start_bind(&network, loopback(0)) {
            Err(code) => format!("refused {code:?}"),
            Ok(()) => {
                socket.subscribe().block();
                match socket.finish_bind() {
                    Ok(()) => "bound".to_owned(),
                    Err(code) => format!("refused {code:?}"),
                }
            }
        },
    };
    format!("env={env} dirs={dirs} lookup={lookup} tcp={tcp} udp={udp}")
}

/// Recursion with no end: each frame keeps a buffer so the optimiser cannot fold it away.
fn recurse(depth: u64) -> u64 {
    if std::hint::black_box(depth) == u64::MAX {
        return 0;
    }
    let frame = std::hint::black_box([depth; 64]);
    recurse(depth + 1).wrapping_add(frame[(depth % 64) as usize])
}

/// A POST of `mib` MiB to the App's own Endpoint, written in 4 KiB chunks as a stream allows.
fn upload(mib: usize) -> String {
    use wasip2::http::types::Method;
    let request = OutgoingRequest::new(Fields::new());
    let _ = request.set_method(&Method::Post);
    let _ = request.set_scheme(Some(&Scheme::Http));
    let _ = request.set_authority(Some("gateway"));
    let _ = request.set_path_with_query(Some("/ngsi-ld/v1/entities"));
    let body = request.body().expect("body");
    let future = match outgoing_handler::handle(request, None) {
        Err(code) => return format!("refused: {code:?}"),
        Ok(future) => future,
    };
    let stream = body.write().expect("stream");
    let chunk = [b'x'; 4096];
    let mut sent = 0usize;
    while sent < mib << 20 {
        if stream.blocking_write_and_flush(&chunk).is_err() {
            break;
        }
        sent += chunk.len();
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
    future.subscribe().block();
    match future.get() {
        Some(Ok(Ok(response))) => format!("status {}", response.status()),
        Some(Ok(Err(code))) => format!("refused: {code:?}"),
        other => format!("no answer: {other:?}"),
    }
}

fn fetch(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some(("https", rest)) => (Scheme::Https, rest),
        Some(("http", rest)) => (Scheme::Http, rest),
        _ => return "not a URL".into(),
    };
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
    let request = OutgoingRequest::new(Fields::new());
    let _ = request.set_scheme(Some(&scheme));
    let _ = request.set_authority(Some(authority));
    let _ = request.set_path_with_query(Some(&path));
    match outgoing_handler::handle(request, None) {
        Err(code) => format!("refused: {code:?}"),
        Ok(future) => {
            future.subscribe().block();
            match future.get() {
                Some(Ok(Ok(response))) => format!("status {}", response.status()),
                Some(Ok(Err(code))) => format!("refused: {code:?}"),
                other => format!("no answer: {other:?}"),
            }
        }
    }
}

impl wasip2::exports::http::incoming_handler::Guest for App {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let (route, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
        match route {
            "/api/hello" => answer(out, 200, &format!("hello {path}")),
            "/api/state" => answer(out, 200, &(COUNTER.fetch_add(1, Ordering::SeqCst) + 1).to_string()),
            "/api/loop" => {
                let mut n: u64 = 0;
                loop {
                    n = std::hint::black_box(n.wrapping_add(1));
                }
            }
            "/api/alloc" => {
                let big = vec![7u8; 128 << 20];
                answer(out, 200, &std::hint::black_box(big).len().to_string());
            }
            "/api/fetch" => answer(out, 200, &fetch(query)),
            "/api/upload" => answer(out, 200, &upload(query.parse().unwrap_or(1))),
            "/api/recurse" => answer(out, 200, &recurse(0).to_string()),
            "/api/passwd" => match std::fs::read_to_string("/etc/passwd") {
                Ok(text) => answer(out, 200, &format!("read {} bytes", text.len())),
                Err(err) => answer(out, 200, &format!("refused: {err}")),
            },
            "/api/answer-then-spin" => {
                answer(out, 200, "answered");
                let mut n: u64 = 0;
                loop {
                    n = std::hint::black_box(n.wrapping_add(1));
                }
            }
            "/api/reach" => answer(out, 200, &reach(query.parse().unwrap_or(80))),
            "/api/sql" => match jc::app::sql::query("select 1", &[]) {
                Ok(rows) => answer(out, 200, &format!("{} rows", rows.values.len())),
                Err(err) => answer(out, 200, &format!("sql: {err:?}")),
            },
            "/api/headers" => {
                let headers = request.headers().entries();
                let names: Vec<String> = headers.iter().map(|(name, _)| name.clone()).collect();
                answer(out, 200, &names.join(","))
            }
            _ => answer(out, 404, "no such route"),
        }
    }
}

wasip2::http::proxy::export!(App);

impl Guest for App {
    fn tick() -> Result<(), String> {
        Ok(())
    }

    fn fail() -> Result<(), String> {
        Err("the indicator could not be computed: no readings in the last hour".into())
    }

    fn spin() -> Result<(), String> {
        let mut n: u64 = 0;
        loop {
            n = std::hint::black_box(n.wrapping_add(1));
        }
    }

    fn call_gateway() -> Result<(), String> {
        let status = fetch("http://gateway/ngsi-ld/v1/entities");
        if status.starts_with("status 2") {
            Ok(())
        } else {
            Err(status)
        }
    }

    fn wrong_shape(n: u32) -> u32 {
        n
    }
}

export!(App);
