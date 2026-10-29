//! The preview poll's ETag (T-1602, CC-78): the gateway sends back the tag of the list it last
//! wrote, a `304` changes nothing, a preview directory that went missing is written again by
//! the next full list, and a Portal that sends no tag is asked in full every time.

use context_gateway::previews::Mirror;
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TAG: &str = "\"v1\"";

fn scratch(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("gw-preview-etag-{test}-{now}"));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A Portal listing one preview. With `tagged`, it answers `ETag: "v1"` and a `304` to that
/// tag; it records the `If-None-Match` of every call, `""` for none.
async fn portal(tagged: bool) -> (String, Arc<Mutex<Vec<String>>>) {
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = asked.clone();
    let app = axum::Router::new().route(
        "/internal/previews",
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let log = log.clone();
            async move {
                let sent = headers
                    .get(axum::http::header::IF_NONE_MATCH)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                if let Ok(mut log) = log.lock() {
                    log.push(sent.clone());
                }
                let body =
                    json!({ "items": [ { "prefix": "ws-air-", "files": { "space.yaml": "x" } } ] })
                        .to_string();
                let mut response = axum::response::Response::builder()
                    .header(axum::http::header::CONTENT_TYPE, "application/json");
                if tagged {
                    response = response.header(axum::http::header::ETAG, TAG);
                    if sent == TAG {
                        return response
                            .status(304)
                            .body(axum::body::Body::empty())
                            .expect("a response");
                    }
                }
                response
                    .status(200)
                    .body(axum::body::Body::from(body))
                    .expect("a response")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/internal/previews"), asked)
}

fn calls(asked: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    asked.lock().map(|log| log.clone()).unwrap_or_default()
}

/// Lets the paused clock run until `done` holds.
async fn until(done: impl Fn() -> bool) {
    for _ in 0..600 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("the poller never got there");
}

/// CC-78: after one full list the gateway asks with its tag, and the `304` leaves the preview
/// served as it was.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_next_poll_sends_the_tag_and_a_304_leaves_the_preview_in_place() {
    let dir = scratch("tagged");
    let (url, asked) = portal(true).await;
    tokio::spawn(Mirror::new(&dir).follow(url, None));

    until(|| calls(&asked).len() >= 3).await;
    let sent = calls(&asked);
    assert_eq!(sent[0], "", "the first poll has nothing to send back");
    assert!(sent[1..].iter().all(|tag| tag == TAG), "{sent:?}");
    assert!(dir.join("ws-air-/space.yaml").is_file());
}

/// CC-78: a preview directory removed under a `304` is not left missing: the gateway drops the
/// tag, and the next full list writes it again.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_preview_directory_that_went_missing_is_written_again_by_the_next_full_list() {
    let dir = scratch("missing");
    let (url, asked) = portal(true).await;
    tokio::spawn(Mirror::new(&dir).follow(url, None));

    until(|| calls(&asked).len() >= 2).await;
    std::fs::remove_dir_all(dir.join("ws-air-")).expect("removed");
    let before = calls(&asked).len();
    until(|| calls(&asked).len() >= before + 2).await;
    until(|| dir.join("ws-air-/space.yaml").is_file()).await;
    assert!(
        calls(&asked)[before..].iter().any(|tag| tag.is_empty()),
        "a full list was asked for: {:?}",
        calls(&asked)
    );
}

/// A Portal that sends no tag is asked in full every time, and nothing is sent back.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_portal_without_an_etag_is_never_sent_one() {
    let dir = scratch("untagged");
    let (url, asked) = portal(false).await;
    tokio::spawn(Mirror::new(&dir).follow(url, None));

    until(|| calls(&asked).len() >= 3).await;
    assert!(
        calls(&asked).iter().all(String::is_empty),
        "{:?}",
        calls(&asked)
    );
    assert!(dir.join("ws-air-/space.yaml").is_file());
}
