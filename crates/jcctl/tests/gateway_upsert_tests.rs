//! T-2531: the edge cases of `Gateway::upsert`, the reconciler's one write to a running platform
//! (CC-04, CC-16, CC-18).
//!
//! The stub answers every request with one canned status and body and records what it was
//! sent, so each case pins what the client does with one kind of answer.
mod common;

use jcctl::gateway::{Broker, BrokerError, Gateway};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "eyJhbGciOiJSUzI1NiJ9.the-reconcilers-token-for-upsert";

/// Starts a gateway that answers every request with `status` and `body`; returns its base
/// URL and the requests it received, as raw text.
fn answering(status: u16, body: &str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let body = body.to_owned();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut raw = Vec::new();
            let mut buffer = [0_u8; 4096];
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        break;
                    }
                }
            }
            log.lock()
                .expect("the log")
                .push(String::from_utf8_lossy(&raw).into_owned());
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn gateway(base: &str) -> Gateway {
    let dir = common::temp_dir("gateway-upsert-token");
    let file = dir.join("token");
    std::fs::write(&file, TOKEN).expect("the token file");
    Gateway::new(base, Gateway::token_from(&file).expect("the token reads")).expect("a client")
}

fn entities() -> Vec<Value> {
    vec![
        json!({ "id": "urn:ngsi-ld:Station:bb.sk:ovzdusie:1", "type": "Station" }),
        json!({ "id": "urn:ngsi-ld:Station:bb.sk:ovzdusie:2", "type": "Station" }),
    ]
}

fn refused(result: Result<(), BrokerError>) -> (u16, String) {
    match result {
        Err(BrokerError::Refused {
            status, message, ..
        }) => (status, message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// CC-18: a 207 whose `errors` is empty means every entity landed.
#[test]
fn a_207_with_an_empty_errors_array_is_treated_as_full_success() {
    let (base, _) = answering(207, r#"{"success":["a","b"],"errors":[]}"#);
    assert_eq!(gateway(&base).upsert("ovzdusie", &entities()), Ok(()));
}

/// CC-18: one refused entity in a 207 fails the run, and the error repeats what the broker
/// said about it.
#[test]
fn a_207_with_a_nonempty_errors_array_is_a_refusal_naming_the_snippet() {
    let body = json!({
        "success": ["urn:ngsi-ld:Station:bb.sk:ovzdusie:1"],
        "errors": [{ "entityId": "urn:ngsi-ld:Station:bb.sk:ovzdusie:2", "error": { "title": "no Policy grants update" } }]
    });
    let (base, _) = answering(207, &body.to_string());
    let (status, message) = refused(gateway(&base).upsert("ovzdusie", &entities()));
    assert_eq!(status, 207);
    assert!(message.contains("ovzdusie:2"), "{message}");
    assert!(message.contains("no Policy grants update"), "{message}");
}

/// CC-16: nothing to write is no call at all, so an empty seed never touches the platform.
#[test]
fn an_empty_entity_list_makes_no_http_call_at_all() {
    let (base, seen) = answering(500, "");
    assert_eq!(gateway(&base).upsert("ovzdusie", &[]), Ok(()));
    assert!(seen.lock().expect("the log").is_empty());
}

/// CC-18: 201 and 204 both mean every entity landed.
#[test]
fn a_201_and_a_204_are_both_treated_as_full_success() {
    for status in [201, 204] {
        let (base, seen) = answering(status, "");
        assert_eq!(
            gateway(&base).upsert("ovzdusie", &entities()),
            Ok(()),
            "{status}"
        );
        let seen = seen.lock().expect("the log");
        assert_eq!(seen.len(), 1, "one request per upsert");
        assert!(
            seen[0].starts_with("POST /cs/ovzdusie/ngsi-ld/v1/entityOperations/upsert "),
            "{}",
            seen[0]
        );
    }
}

/// CC-18: a 500 is a refusal that repeats the start of what the broker said.
#[test]
fn a_500_is_a_refusal_carrying_a_body_snippet_not_the_whole_body() {
    let (base, _) = answering(500, r#"{"title":"the broker is restarting"}"#);
    let (status, message) = refused(gateway(&base).upsert("ovzdusie", &entities()));
    assert_eq!(status, 500);
    assert!(message.contains("the broker is restarting"), "{message}");
}

/// CC-18: a proxy's error page is cut to its first 200 characters, marked as cut.
#[test]
fn a_refusal_body_larger_than_the_snippet_bound_is_truncated() {
    let page = "x".repeat(5_000);
    let (base, _) = answering(502, &page);
    let (_, message) = refused(gateway(&base).upsert("ovzdusie", &entities()));
    assert_eq!(
        message.chars().count(),
        201,
        "200 characters and the ellipsis"
    );
    assert!(message.ends_with('…'));
}

/// CC-18: a gateway nobody answers at is `Unavailable`, which is not the platform refusing.
#[test]
fn an_unreachable_host_is_unavailable_not_refused() {
    let closed = TcpListener::bind("127.0.0.1:0").expect("a port");
    let base = format!("http://{}", closed.local_addr().expect("an address"));
    drop(closed);
    match gateway(&base).upsert("ovzdusie", &entities()) {
        Err(BrokerError::Unavailable { url, message }) => {
            assert!(url.starts_with(&base), "{url}");
            assert!(!message.contains(TOKEN), "{message}");
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
}

/// CC-04: the token goes in the Authorization header and nowhere else; a refusal that echoes
/// the request back does not carry it into the error.
#[test]
fn the_bearer_token_never_appears_in_a_refused_error_message() {
    let (base, seen) = answering(403, "");
    let error = gateway(&base)
        .upsert("ovzdusie", &entities())
        .expect_err("refused");
    assert!(!error.to_string().contains(TOKEN), "{error}");
    assert!(!format!("{error:?}").contains(TOKEN), "{error:?}");
    let request = seen.lock().expect("the log")[0].clone();
    assert!(
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("authorization: Bearer {TOKEN}"))),
        "{request}"
    );
    assert_eq!(request.matches(TOKEN).count(), 1, "only in the header");
}

/// CC-18: a 207 whose body is not JSON says nothing about which entities landed, so it
/// cannot be counted as all of them.
#[test]
fn a_207_whose_body_is_not_json_is_not_treated_as_success() {
    let (base, _) = answering(207, "<html>upstream reset</html>");
    let result = gateway(&base).upsert("ovzdusie", &entities());
    assert!(
        result.is_err(),
        "a 207 nobody can read was taken as success"
    );
}

/// CC-18: a 207 whose `errors` is not a list is not a report the client can read either.
#[test]
fn a_207_with_an_errors_key_that_is_not_an_array_is_not_treated_as_success() {
    let (base, _) = answering(207, r#"{"errors":"the batch was cut short"}"#);
    let result = gateway(&base).upsert("ovzdusie", &entities());
    assert!(
        result.is_err(),
        "a 207 with an unreadable errors member was taken as success"
    );
}

/// CC-18, T-2539: with no `errors`, only a `success` list naming every entity sent is a full
/// success; one that names fewer, or none at all, is a refusal saying why.
#[test]
fn a_207_without_errors_is_success_only_when_every_entity_is_named() {
    let sent = entities().len();
    let all: Vec<String> = (0..sent).map(|n| format!("urn:{n}")).collect();
    let (base, _) = answering(207, &serde_json::json!({ "success": all }).to_string());
    assert!(gateway(&base).upsert("ovzdusie", &entities()).is_ok());

    for body in [r#"{"success":["urn:0"]}"#, "{}", "[]"] {
        let (base, _) = answering(207, body);
        let (status, message) = refused(gateway(&base).upsert("ovzdusie", &entities()));
        assert_eq!(status, 207, "{body}");
        assert!(
            message.contains("does not say which entities landed"),
            "{body}: {message}"
        );
    }
}
