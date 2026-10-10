//! A test App: `/api/hello`, `/api/state` (a counter that a fresh instance starts at one),
//! `/api/loop` (never ends), `/api/alloc` (asks for 128 MiB), `/api/fetch?<url>` (an outgoing
//! GET, answering the status or the error), `/api/sql` (the host's `jc:app/sql`); and the jobs of
//! `wit/jobs.wit`.

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
