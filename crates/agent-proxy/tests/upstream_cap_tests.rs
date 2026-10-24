//! Every door that reads an upstream's answer whole holds it only up to a cap (T-5510).
//!
//! The stub upstream sends one byte past the door's cap, chunked and without a length, and never
//! ends the body. A proxy that buffered the whole answer would wait for the end forever; a capped
//! one answers 502 naming the cap and drops the connection, which the stub sees.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{app, authed, body_of, sample_run, Bases};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower::ServiceExt;

const GATEWAY_CAP: usize = 8 * 1024 * 1024;
const MODEL_CAP: usize = 1024 * 1024;
const SMALL_CAP: usize = 256 * 1024;

/// An upstream that answers its first request with `cap + 1` bytes and no end, and reports
/// whether the caller closed the connection afterwards.
async fn endless_upstream(cap: usize) -> (String, oneshot::Receiver<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let addr = listener.local_addr().expect("an address");
    let (closed, seen) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("a connection");
        let _ = socket.read(&mut [0u8; 16 * 1024]).await;
        let head =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut left = cap + 1;
        let mut sent = socket.write_all(head.as_bytes()).await.is_ok();
        while sent && left > 0 {
            let n = left.min(64 * 1024);
            let chunk = format!("{n:x}\r\n{}\r\n", "x".repeat(n));
            sent = socket.write_all(chunk.as_bytes()).await.is_ok();
            left -= n;
        }
        // The body never ends: only the caller closing the connection gets past this read.
        let eof = matches!(
            tokio::time::timeout(Duration::from_secs(10), socket.read(&mut [0u8; 1])).await,
            Ok(Ok(0) | Err(_))
        );
        let _ = closed.send(!sent || eof);
    });
    (format!("http://localhost:{}", addr.port()), seen)
}

/// Sends `request` to a proxy whose `upstream` is the endless stub, and checks the refusal.
async fn refused(
    cap: usize,
    bases: impl FnOnce(String) -> Bases,
    request: axum::http::Request<Body>,
) {
    let (base, closed) = endless_upstream(cap).await;
    let uri = request.uri().to_string();
    let response = tokio::time::timeout(
        Duration::from_secs(20),
        app(sample_run(true), bases(base)).oneshot(request),
    )
    .await
    .unwrap_or_else(|_| panic!("{uri}: the proxy waited for an endless body"))
    .expect("an answer");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{uri}");
    let detail = body_of(response).await;
    assert!(
        detail.contains(&format!("more than {cap} bytes")),
        "{uri}: {detail}"
    );
    assert!(
        closed.await.expect("the stub reports"),
        "{uri}: the proxy kept the connection open"
    );
}

#[tokio::test]
async fn a_gateway_answer_past_the_cap_is_refused() {
    refused(
        GATEWAY_CAP,
        |gateway| Bases {
            gateway,
            ..Bases::default()
        },
        authed("GET", "/v1/data/ngsi-ld/v1/entities")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
}

#[tokio::test]
async fn a_forge_answer_past_the_cap_is_refused() {
    refused(
        GATEWAY_CAP,
        |forge| Bases {
            forge,
            ..Bases::default()
        },
        authed(
            "GET",
            "/v1/forge/contents/projects/helsinki/apps/bikes/src/main.rs",
        )
        .body(Body::empty())
        .expect("a request"),
    )
    .await;
}

#[tokio::test]
async fn a_model_answer_past_the_cap_is_refused() {
    refused(
        MODEL_CAP,
        |model| Bases {
            model,
            ..Bases::default()
        },
        authed("POST", "/v1/llm/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"claude-3-7","messages":[]}"#))
            .expect("a request"),
    )
    .await;
}

#[tokio::test]
async fn portal_answers_past_the_cap_are_refused() {
    let portal = |portal| Bases {
        portal,
        ..Bases::default()
    };
    refused(
        GATEWAY_CAP,
        portal,
        authed("POST", "/v1/mcp")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .expect("a request"),
    )
    .await;
    refused(
        GATEWAY_CAP,
        portal,
        authed("GET", "/v1/runs/inbox")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    refused(
        GATEWAY_CAP,
        portal,
        authed("GET", "/v1/diagnostics/pipeline/hsl-bikes")
            .body(Body::empty())
            .expect("a request"),
    )
    .await;
    refused(
        SMALL_CAP,
        portal,
        authed("POST", "/v1/runs/events")
            .body(Body::from(r#"{"kind":"log","payload":{}}"#))
            .expect("a request"),
    )
    .await;
}
