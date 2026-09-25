//! T-2846, PF-45: `jcctl login` runs RFC 8628's device flow against a stub issuer.
//!
//! The stub answers the device authorization once and then the token endpoint from a script, so
//! each case pins one outcome: the token written with mode 600, the interval raised by
//! `slow_down`, and a declined or expired sign-in that writes nothing. Sleeps are recorded, not
//! slept.
mod common;

use jcctl::login::{issuer, login, LoginError};
use serde_json::json;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOKEN: &str = "eyJhbGciOiJFUzI1NiJ9.a-persons-device-flow-token";

/// Starts an issuer whose token endpoint answers `script` in order; returns its URL and the
/// request bodies it received.
fn stub(
    interval: Option<u64>,
    script: Vec<(u16, serde_json::Value)>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let mut script: VecDeque<_> = script.into();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut raw = Vec::new();
            let mut buffer = [0_u8; 4096];
            let (head, body) = loop {
                let Ok(read) = stream.read(&mut buffer) else {
                    break (String::new(), String::new());
                };
                if read == 0 {
                    break (String::new(), String::new());
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
                        break (head.to_owned(), rest.to_owned());
                    }
                }
            };
            let path = head.split(' ').nth(1).unwrap_or("").to_owned();
            log.lock().expect("the log").push(format!("{path} {body}"));
            let (status, answer) = if path.ends_with("/auth/device") {
                let mut device = json!({
                    "device_code": "dev-code-1",
                    "user_code": "WDJB-MJHT",
                    "verification_uri": "https://idm.example/realms/city/device",
                    "verification_uri_complete": "https://idm.example/realms/city/device?user_code=WDJB-MJHT",
                    "expires_in": 600
                });
                if let Some(interval) = interval {
                    device["interval"] = json!(interval);
                }
                (200, device)
            } else {
                script
                    .pop_front()
                    .unwrap_or((400, json!({ "error": "invalid_grant" })))
            };
            let answer = answer.to_string();
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{answer}",
                    answer.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://127.0.0.1:{port}/realms/city"), seen)
}

fn pending() -> (u16, serde_json::Value) {
    (400, json!({ "error": "authorization_pending" }))
}

fn granted() -> (u16, serde_json::Value) {
    (
        200,
        json!({ "access_token": TOKEN, "expires_in": 300, "refresh_token": "never-kept" }),
    )
}

fn target(test: &str) -> PathBuf {
    common::temp_dir(&format!("login-{test}"))
        .join("jcctl")
        .join("token")
}

fn run(
    url: &str,
    path: &std::path::Path,
) -> (
    Result<jcctl::login::LoggedIn, LoginError>,
    Vec<String>,
    Vec<Duration>,
) {
    let mut told = Vec::new();
    let mut slept = Vec::new();
    let result = login(
        &issuer(url).expect("a usable issuer"),
        path,
        |line| told.push(line.to_owned()),
        |d| slept.push(d),
    );
    (result, told, slept)
}

#[test]
fn a_confirmed_sign_in_writes_the_access_token_alone_with_mode_600() {
    let (url, seen) = stub(Some(0), vec![pending(), granted()]);
    let path = target("granted");
    let (result, told, _) = run(&url, &path);
    let done = result.expect("signed in");
    assert_eq!(done.expires_in, Some(300));
    assert_eq!(
        std::fs::read_to_string(&path).expect("the token file"),
        TOKEN
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir = std::fs::metadata(path.parent().expect("a parent"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir, 0o700);
    }
    assert_eq!(
        told,
        ["To sign in, open https://idm.example/realms/city/device?user_code=WDJB-MJHT and confirm the code WDJB-MJHT"]
    );
    assert!(told.iter().all(|line| !line.contains(TOKEN)));
    let seen = seen.lock().expect("the log").clone();
    assert!(seen[0].starts_with("/realms/city/protocol/openid-connect/auth/device client_id=jcctl"));
    assert!(seen[1].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
    assert!(seen[1].contains("device_code=dev-code-1"));
    // Only the token: the refresh token is never kept.
    let dir = path.parent().expect("a parent");
    let files: Vec<_> = std::fs::read_dir(dir)
        .expect("the dir")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(files, ["token"]);
}

#[test]
fn a_second_sign_in_replaces_the_token() {
    let path = target("again");
    let (first, _) = stub(Some(0), vec![granted()]);
    run(&first, &path).0.expect("signed in");
    std::fs::write(&path, "an-old-token").expect("an old token");
    let (second, _) = stub(Some(0), vec![granted()]);
    run(&second, &path).0.expect("signed in again");
    assert_eq!(
        std::fs::read_to_string(&path).expect("the token file"),
        TOKEN
    );
}

#[test]
fn slow_down_raises_the_interval_by_five_seconds_and_no_interval_means_five() {
    let (url, _) = stub(
        None,
        vec![(400, json!({ "error": "slow_down" })), pending(), granted()],
    );
    let (result, _, slept) = run(&url, &target("slow"));
    result.expect("signed in");
    assert_eq!(
        slept,
        [
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(10)
        ]
    );
}

#[test]
fn a_declined_or_expired_sign_in_writes_nothing() {
    for (error, says) in [("access_denied", "declined"), ("expired_token", "expired")] {
        let (url, _) = stub(Some(0), vec![pending(), (400, json!({ "error": error }))]);
        let path = target(error);
        let (result, _, _) = run(&url, &path);
        let message = result.expect_err("no token").to_string();
        assert!(message.contains(says), "{message}");
        assert!(!path.exists(), "{error} wrote a token");
    }
}

#[test]
fn a_client_without_the_grant_is_refused_in_the_providers_words() {
    let (url, _) = stub(
        Some(0),
        vec![(
            400,
            json!({ "error": "unauthorized_client", "error_description": "Client is not allowed to initiate OAuth 2.0 Device Authorization Grant." }),
        )],
    );
    let path = target("unauthorized");
    let (result, _, _) = run(&url, &path);
    assert_eq!(
        result.expect_err("no token"),
        LoginError::Refused(
            "Client is not allowed to initiate OAuth 2.0 Device Authorization Grant.".to_owned()
        )
    );
    assert!(!path.exists());
}

#[test]
fn the_issuer_must_be_https_except_on_loopback_and_carry_no_credentials() {
    assert!(issuer("https://idm.example.org/realms/city").is_ok());
    assert!(issuer("http://127.0.0.1:8080/realms/city").is_ok());
    assert!(issuer("http://[::1]:8080/realms/city").is_ok());
    assert!(issuer("http://localhost/realms/city").is_ok());
    for bad in [
        "http://idm.example.org/realms/city",
        "https://user:pw@idm.example.org/realms/city",
        "ftp://idm.example.org",
        "not a url",
    ] {
        assert!(matches!(issuer(bad), Err(LoginError::Address(_))), "{bad}");
    }
}

#[test]
fn the_command_needs_an_issuer_and_takes_no_client_flags() {
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_jcctl"))
            .args(args)
            .env_remove("JC_IDM")
            .output()
            .expect("jcctl runs")
    };
    let missing = run(&["login"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--idm"));
    let plain = run(&["login", "--idm", "http://idm.example.org/realms/city"]);
    assert!(String::from_utf8_lossy(&plain.stderr).contains("only fetched over https"));
    let stray = run(&[
        "login",
        "--idm",
        "https://idm.example.org/realms/city",
        "--project",
        "x",
    ]);
    assert!(String::from_utf8_lossy(&stray.stderr).starts_with("usage:"));
    assert!(String::from_utf8_lossy(&stray.stderr).contains("jcctl login --idm <issuer url>"));
}
