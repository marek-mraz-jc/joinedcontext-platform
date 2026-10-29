//! The OpenBao transport against a loopback server (T-0423, OPS-37, PL-17).
//!
//! `secrets::openbao` was tested against a double from the day it landed; this is the other
//! half — what actually goes on the wire, and what comes back when the store refuses. The
//! properties worth a test are the ones a mistake makes invisible: the API root, the header
//! the token travels in, that a 404 is "nothing here" and not an error, and that no refusal
//! ever repeats a credential.

use jcctl::secrets::bao_http::HttpBao;
use jcctl::secrets::openbao::{login, BaoApi, BaoError, Settings};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct Call {
    method: String,
    path: String,
    token: Option<String>,
    body: Value,
}

#[derive(Default)]
struct Store {
    calls: Vec<Call>,
    /// Status and body for the next answer, in order; the last one repeats.
    answers: Vec<(u16, String)>,
}

/// An OpenBao that answers on a loopback port. The thread is detached; the test binary ends it.
fn spawn(store: Arc<Mutex<Store>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("a bound address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            let (head, body) = request.split_once("\r\n\r\n").unwrap_or((&request, ""));
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or_default();
            let mut words = request_line.split_whitespace();
            let method = words.next().unwrap_or_default().to_owned();
            let path = words.next().unwrap_or("/").to_owned();
            let token = lines.clone().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-vault-token")
                    .then(|| value.trim().to_owned())
            });

            let (status, answer) = {
                let mut store = store.lock().expect("the store");
                store.calls.push(Call {
                    method,
                    path,
                    token,
                    body: serde_json::from_str(body).unwrap_or(Value::Null),
                });
                match store.answers.len() {
                    0 => (500, "{}".to_owned()),
                    1 => store.answers[0].clone(),
                    _ => store.answers.remove(0),
                }
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                )
                .as_bytes(),
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let announced: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().to_owned())
                })
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            if body.len() >= announced {
                break;
            }
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

fn store_answering(answers: Vec<(u16, Value)>) -> (Arc<Mutex<Store>>, String) {
    let store = Arc::new(Mutex::new(Store {
        answers: answers
            .into_iter()
            .map(|(status, body)| (status, body.to_string()))
            .collect(),
        ..Store::default()
    }));
    let address = spawn(Arc::clone(&store));
    (store, address)
}

fn calls(store: &Arc<Mutex<Store>>) -> Vec<Call> {
    store.lock().expect("the store").calls.clone()
}

const LOGIN_OK: fn() -> Value = || {
    json!({"auth": {
        "client_token": "s.the-session-token",
        "lease_duration": 3600,
        "renewable": true,
    }})
};

#[test]
fn a_login_posts_the_jwt_to_the_v1_api_root_and_carries_no_token() {
    let (store, address) = store_answering(vec![(200, LOGIN_OK())]);
    let mut api = HttpBao::new(&address).expect("a client");
    let settings = Settings::new("pipeline-runner");

    let session = login(&mut api, &settings, "the.service.account.jwt").expect("a session");
    assert_eq!(session.lease_seconds(), 3600);

    let call = calls(&store).pop().expect("one call");
    assert_eq!(call.method, "POST");
    // The paths this crate builds are relative to the API root, so the transport is what adds
    // `/v1`. Forgetting it answers 404 on every secret and looks like a policy problem.
    assert_eq!(call.path, "/v1/auth/kubernetes/login");
    assert_eq!(call.body["jwt"], "the.service.account.jwt");
    assert_eq!(call.body["role"], "pipeline-runner");
    assert!(
        call.token.is_none(),
        "the login call needs no session token"
    );
}

#[test]
fn a_read_sends_the_session_token_in_the_header_and_nowhere_else() {
    let (store, address) = store_answering(vec![(
        200,
        json!({"data": {"data": {"password": "p"}, "metadata": {"version": 1}}}),
    )]);
    let api = HttpBao::new(&address).expect("a client");

    let answer = api
        .get("secret/data/city/mqtt", "s.the-session-token")
        .expect("a value")
        .expect("something at the path");
    assert_eq!(answer["data"]["data"]["password"], "p");

    let call = calls(&store).pop().expect("one call");
    assert_eq!(call.method, "GET");
    assert_eq!(call.path, "/v1/secret/data/city/mqtt");
    assert_eq!(call.token.as_deref(), Some("s.the-session-token"));
    // A token in the URL lands in every access log between here and the store.
    assert!(!call.path.contains("s.the-session-token"));
}

#[test]
fn a_path_that_holds_nothing_is_not_an_error() {
    // OpenBao answers 404 both for an empty path and for one the policy hides, so the
    // resolver has to see the same thing in both cases and say so itself.
    let (_store, address) = store_answering(vec![(404, json!({"errors": []}))]);
    let api = HttpBao::new(&address).expect("a client");
    assert!(api
        .get("secret/data/city/absent", "s.token")
        .expect("a 404 is an answer")
        .is_none());
}

#[test]
fn a_refusal_carries_what_openbao_said_and_not_the_token() {
    let (_store, address) = store_answering(vec![(
        403,
        json!({"errors": ["1 error occurred:\n\t* permission denied\n\n"]}),
    )]);
    let api = HttpBao::new(&address).expect("a client");

    let error = api
        .get("secret/data/other-project/mqtt", "s.the-session-token")
        .expect_err("403 is a refusal");
    let said = error.to_string();
    assert!(said.contains("403"), "{said}");
    assert!(said.contains("permission denied"), "{said}");
    assert!(!said.contains("s.the-session-token"), "{said}");
    assert!(matches!(error, BaoError::Api { status: 403, .. }));
}

#[test]
fn an_answer_that_is_not_json_says_so_with_a_snippet_and_not_a_panic() {
    let store = Arc::new(Mutex::new(Store {
        answers: vec![(
            200,
            "<html><body><h1>502 Bad Gateway</h1></body></html>".to_owned(),
        )],
        ..Store::default()
    }));
    let address = spawn(Arc::clone(&store));
    let api = HttpBao::new(&address).expect("a client");

    let error = api
        .get("secret/data/city/mqtt", "s.token")
        .expect_err("not JSON");
    assert!(matches!(error, BaoError::Malformed { .. }), "{error}");
    assert!(error.to_string().contains("Bad Gateway"), "{error}");
}

#[test]
fn a_store_that_does_not_answer_is_a_transport_error_naming_the_path() {
    // Nothing listens on this port. The apply has to fail with the path it was reading, not
    // with a panic and not with a value it never got.
    let api = HttpBao::new("http://127.0.0.1:1").expect("a client");
    let error = api
        .get("secret/data/city/mqtt", "s.token")
        .expect_err("nothing answers");
    assert!(matches!(error, BaoError::Transport { .. }), "{error}");
    assert!(
        error.to_string().contains("secret/data/city/mqtt"),
        "{error}"
    );
}

#[test]
fn an_address_carrying_credentials_or_no_host_is_refused_at_start_up() {
    for address in [
        "https://user:password@openbao.example.test",
        "not a url",
        "file:///etc/passwd",
    ] {
        assert!(
            HttpBao::new(address).is_err(),
            "{address} was accepted as an OpenBao address"
        );
    }
    let client = HttpBao::new("https://openbao.jc-system.svc.cluster.local:8200/").expect("a URL");
    assert_eq!(
        client.address(),
        "https://openbao.jc-system.svc.cluster.local:8200"
    );
}

#[test]
fn a_ca_file_that_is_not_a_certificate_is_refused_rather_than_ignored() {
    // Silently falling back to the platform bundle would turn a configuration mistake into a
    // handshake that works in dev and fails in the cluster that needs the extra root.
    let dir = std::env::temp_dir().join("jcctl-bao-ca-test");
    std::fs::create_dir_all(&dir).expect("a temp directory");
    let path = dir.join("not-a-certificate.pem");
    std::fs::write(&path, b"this is not a certificate").expect("the file");
    let error = HttpBao::with_ca_file("https://openbao.example.test", &path)
        .expect_err("not a certificate");
    assert!(error.to_string().contains("PEM"), "{error}");
    let _ = std::fs::remove_file(&path);
}
