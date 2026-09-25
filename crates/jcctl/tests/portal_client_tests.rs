//! T-2374, MF-14: the kubectl-shaped verbs as a client of the Portal resource API.
//!
//! A stub Portal keeps resources by path, answers a write with a `Change` as API/01 §5 shapes it
//! and records every request, so each case asserts what `jcctl` sent, what it printed and its
//! exit code: reads read, every write is a proposal, and the token never leaves the header.
mod common;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "eyJhbGciOiJFUzI1NiJ9.a-persons-token-for-the-portal";
const BASE: &str = "/api/v1/projects/helsinki/endpoints";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    authorization: String,
    body: String,
}

#[derive(Default)]
struct Stub {
    /// Answers by path: the JSON a `GET` returns.
    resources: BTreeMap<String, Value>,
    /// A path whose write is refused with this status and detail.
    refuse: Option<(String, u16, String)>,
    seen: Vec<Seen>,
}

fn endpoint(name: &str, audience: &str) -> Value {
    json!({
        "apiVersion": "joinedcontext.com/v1alpha1",
        "kind": "Endpoint",
        "metadata": { "name": name, "namespace": "helsinki", "title": "Air quality" },
        "spec": {
            "contextSpaceRef": "air",
            "audience": audience,
            "auth": { "secretRef": { "name": "air-upstream", "key": "token" } }
        },
        "status": { "phase": "Live" }
    })
}

fn answer(stub: &Mutex<Stub>, method: &str, target: &str) -> (u16, String) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let stub = stub.lock().expect("the stub");
    if let Some((refused, status, detail)) = stub.refuse.clone() {
        if refused == path && method != "GET" {
            return (
                status,
                json!({ "title": "Forbidden", "status": status, "detail": detail }).to_string(),
            );
        }
    }
    let change = |lane: &str| {
        json!({
            "apiVersion": "joinedcontext.com/v1alpha1",
            "kind": "Change",
            "metadata": { "name": "chg-0000019c", "namespace": "helsinki" },
            "status": { "lane": lane, "phase": "PendingApproval" }
        })
        .to_string()
    };
    match method {
        "GET" if path == BASE => {
            let items: Vec<Value> = stub
                .resources
                .iter()
                .filter(|(p, _)| p.starts_with(&format!("{BASE}/")))
                .map(|(_, v)| v.clone())
                .collect();
            // Two pages, so the client has to follow `continue`.
            let page = if query.contains("continue=page-2") {
                json!({ "kind": "List", "metadata": {}, "items": items[1..].to_vec() })
            } else {
                json!({ "kind": "List", "metadata": { "continue": "page-2" }, "items": items[..1].to_vec() })
            };
            (200, page.to_string())
        }
        "GET" => match stub.resources.get(path) {
            Some(v) => (200, v.to_string()),
            None => (
                404,
                json!({ "status": 404, "detail": "not found" }).to_string(),
            ),
        },
        "POST" | "PUT" => (202, change("green")),
        "DELETE" => (202, change("red")),
        _ => (405, String::new()),
    }
}

/// Starts the stub Portal; returns its base URL and the shared state.
fn portal(resources: &[Value]) -> (String, Arc<Mutex<Stub>>) {
    let stub = Arc::new(Mutex::new(Stub::default()));
    for r in resources {
        let name = r["metadata"]["name"].as_str().expect("a name");
        stub.lock()
            .expect("the stub")
            .resources
            .insert(format!("{BASE}/{name}"), r.clone());
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let shared = Arc::clone(&stub);
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
            let mut words = head.lines().next().unwrap_or("").split(' ');
            let method = words.next().unwrap_or("").to_owned();
            let target = words.next().unwrap_or("").to_owned();
            let authorization = head
                .lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(n, _)| n.eq_ignore_ascii_case("authorization"))
                })
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default();
            let (status, answer) = answer(&shared, &method, &target);
            shared.lock().expect("the stub").seen.push(Seen {
                method,
                path: target,
                authorization,
                body,
            });
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
    (format!("http://127.0.0.1:{port}"), stub)
}

fn token_file(test: &str) -> PathBuf {
    let dir = common::temp_dir(&format!("portal-client-{test}"));
    let file = dir.join("token");
    std::fs::write(&file, format!("{TOKEN}\n")).expect("the token file");
    file
}

fn manifest_file(test: &str, text: &str) -> PathBuf {
    let dir = common::temp_dir(&format!("portal-client-{test}-file"));
    let file = dir.join("manifests.yaml");
    std::fs::write(&file, text).expect("the manifest file");
    file
}

fn jcctl(server: &str, token: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args(args)
        .args(["--server", server, "--token-file", &token.to_string_lossy()])
        .env_remove("JC_SERVER")
        .env_remove("JC_TOKEN_FILE")
        .output()
        .expect("jcctl runs");
    for text in [&output.stdout, &output.stderr] {
        assert!(
            !String::from_utf8_lossy(text).contains(TOKEN),
            "the token reached the output"
        );
    }
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn seen(stub: &Mutex<Stub>) -> Vec<Seen> {
    stub.lock().expect("the stub").seen.clone()
}

const NEW_AND_CHANGED: &str = "apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  \
    name: fresh-air\n  namespace: helsinki\nspec:\n  contextSpaceRef: air\n  audience: organization\n---\n\
    apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: public-air\n  \
    namespace: helsinki\nspec:\n  contextSpaceRef: air\n  audience: public\nstatus:\n  phase: Live\n---\n\
    apiVersion: joinedcontext.com/v1alpha1\nkind: Endpoint\nmetadata:\n  name: same-air\n  \
    namespace: helsinki\nspec:\n  contextSpaceRef: air\n  audience: organization\n";

#[test]
fn get_lists_every_page_by_name_with_the_token_in_the_header_only() {
    let (server, stub) = portal(&[
        endpoint("public-air", "organization"),
        endpoint("same-air", "organization"),
    ]);
    let out = jcctl(
        &server,
        &token_file("list"),
        &["get", "endpoints", "--project", "helsinki"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "endpoints/public-air\nendpoints/same-air\n");
    let seen = seen(&stub);
    assert_eq!(seen.len(), 2, "the second page was fetched");
    assert!(seen[1].path.contains("continue=page-2"));
    assert!(seen
        .iter()
        .all(|s| s.authorization == format!("Bearer {TOKEN}")));
    assert!(seen.iter().all(|s| !s.path.contains(TOKEN)));
}

#[test]
fn get_yaml_prints_the_manifest_as_served_with_the_secret_ref_and_no_value() {
    let (server, _) = portal(&[endpoint("public-air", "organization")]);
    let out = jcctl(
        &server,
        &token_file("yaml"),
        &[
            "get",
            "endpoints",
            "public-air",
            "--project",
            "helsinki",
            "-o",
            "yaml",
        ],
    );
    assert!(out.status.success());
    let printed: Value = serde_norway::from_str(&stdout(&out)).expect("YAML");
    assert_eq!(printed, endpoint("public-air", "organization"));
    let json = jcctl(
        &server,
        &token_file("json"),
        &[
            "get",
            "endpoints",
            "public-air",
            "--project",
            "helsinki",
            "-o",
            "json",
        ],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&stdout(&json)).expect("JSON"),
        endpoint("public-air", "organization")
    );
}

#[test]
fn describe_shows_identity_spec_and_status() {
    let (server, _) = portal(&[endpoint("public-air", "organization")]);
    let out = jcctl(
        &server,
        &token_file("describe"),
        &[
            "describe",
            "endpoints",
            "public-air",
            "--project",
            "helsinki",
        ],
    );
    assert!(out.status.success());
    let text = stdout(&out);
    for line in [
        "Kind:     Endpoint",
        "Name:     public-air",
        "Project:  helsinki",
        "Title:    Air quality",
        "Spec:",
        "  audience: organization",
        "Status:",
        "  phase: Live",
    ] {
        assert!(text.contains(line), "{line} missing from\n{text}");
    }
}

#[test]
fn a_name_is_escaped_into_one_path_segment() {
    let (server, stub) = portal(&[]);
    let out = jcctl(
        &server,
        &token_file("escape"),
        &["get", "endpoints", "a/../../x", "--project", "helsinki"],
    );
    assert!(!out.status.success());
    assert_eq!(seen(&stub)[0].path, format!("{BASE}/a%2F..%2F..%2Fx"));
}

#[test]
fn an_unknown_plural_is_refused_before_any_request() {
    let (server, stub) = portal(&[]);
    let out = jcctl(
        &server,
        &token_file("plural"),
        &["get", "endpoint", "--project", "helsinki"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no kind is served as 'endpoint'"));
    assert!(seen(&stub).is_empty());
}

#[test]
fn apply_proposes_a_new_and_a_changed_manifest_and_leaves_a_matching_one_alone() {
    let (server, stub) = portal(&[
        endpoint("public-air", "organization"),
        endpoint("same-air", "organization"),
    ]);
    let file = manifest_file("apply", NEW_AND_CHANGED);
    let out = jcctl(
        &server,
        &token_file("apply"),
        &["apply", "-f", &file.to_string_lossy()],
    );
    assert!(out.status.success(), "{}", stdout(&out));
    assert_eq!(
        stdout(&out),
        "endpoints/fresh-air: created, chg-0000019c PendingApproval (green lane)\n\
         endpoints/public-air: replaced, chg-0000019c PendingApproval (green lane)\n\
         endpoints/same-air: unchanged\n"
    );
    let writes: Vec<Seen> = seen(&stub)
        .into_iter()
        .filter(|s| s.method != "GET")
        .collect();
    assert_eq!(writes.len(), 2);
    assert_eq!(
        (writes[0].method.as_str(), writes[0].path.as_str()),
        ("POST", BASE)
    );
    assert_eq!(writes[1].method, "PUT");
    assert_eq!(writes[1].path, format!("{BASE}/public-air"));
    let sent: Value = serde_json::from_str(&writes[1].body).expect("a JSON body");
    assert_eq!(sent["spec"]["audience"], "public");
    assert!(
        sent.get("status").is_none(),
        "status is the server's and is never sent"
    );
}

#[test]
fn a_refused_manifest_is_reported_redacted_and_the_next_one_still_goes() {
    let (server, stub) = portal(&[endpoint("public-air", "organization")]);
    stub.lock().expect("the stub").refuse = Some((
        BASE.to_owned(),
        403,
        format!("propose on Endpoint is missing for Bearer {TOKEN}"),
    ));
    let file = manifest_file("refused", NEW_AND_CHANGED);
    let out = jcctl(
        &server,
        &token_file("refused"),
        &["apply", "-f", &file.to_string_lossy()],
    );
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("endpoints/fresh-air: the Portal refused (403): propose on Endpoint is missing for Bearer [redacted]"), "{text}");
    assert!(text.contains("endpoints/public-air: replaced"));
}

#[test]
fn diff_names_each_member_and_exits_2_without_writing() {
    let (server, stub) = portal(&[
        endpoint("public-air", "organization"),
        endpoint("same-air", "organization"),
    ]);
    let file = manifest_file("diff", NEW_AND_CHANGED);
    let out = jcctl(
        &server,
        &token_file("diff"),
        &["diff", "-f", &file.to_string_lossy()],
    );
    assert_eq!(out.status.code(), Some(2));
    let text = stdout(&out);
    assert!(text.contains("endpoints/fresh-air: not in the Portal, apply would create it"));
    assert!(
        text.contains("endpoints/public-air: spec.audience: \"organization\" -> \"public\""),
        "{text}"
    );
    assert!(text.contains("endpoints/same-air: unchanged"));
    assert!(seen(&stub).iter().all(|s| s.method == "GET"));
}

#[test]
fn diff_of_a_file_the_portal_already_holds_exits_0() {
    let (server, _) = portal(&[endpoint("same-air", "organization")]);
    let text = NEW_AND_CHANGED
        .rsplit("---\n")
        .next()
        .expect("the last document");
    let file = manifest_file("same", text);
    let out = jcctl(
        &server,
        &token_file("same"),
        &["diff", "-f", &file.to_string_lossy()],
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn delete_proposes_on_the_deletion_lane() {
    let (server, stub) = portal(&[endpoint("public-air", "organization")]);
    let file = manifest_file(
        "delete",
        &serde_norway::to_string(&endpoint("public-air", "organization")).expect("YAML"),
    );
    let out = jcctl(
        &server,
        &token_file("delete"),
        &["delete", "-f", &file.to_string_lossy()],
    );
    assert!(out.status.success());
    assert_eq!(
        stdout(&out),
        "endpoints/public-air: deleted, chg-0000019c PendingApproval (red lane)\n"
    );
    let seen = seen(&stub);
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("DELETE", format!("{BASE}/public-air").as_str())
    );
}

#[test]
fn a_manifest_in_another_project_than_asked_is_refused_before_any_request() {
    let (server, stub) = portal(&[]);
    let file = manifest_file("project", NEW_AND_CHANGED);
    let out = jcctl(
        &server,
        &token_file("project"),
        &["apply", "-f", &file.to_string_lossy(), "--project", "espoo"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is in project helsinki, not espoo"));
    assert!(seen(&stub).is_empty());
}

#[test]
fn no_identity_or_a_url_with_credentials_is_refused_before_any_request() {
    let (server, stub) = portal(&[]);
    let missing = Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args([
            "get",
            "endpoints",
            "--project",
            "helsinki",
            "--server",
            &server,
        ])
        .env_remove("JC_TOKEN_FILE")
        .output()
        .expect("jcctl runs");
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--token-file"));
    let with_password = server.replace("http://", "http://user:pw@");
    let bad = jcctl(
        &with_password,
        &token_file("credentials"),
        &["get", "endpoints", "--project", "helsinki"],
    );
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("carries credentials"));
    assert!(seen(&stub).is_empty());
}

#[test]
fn the_usage_lists_the_client_verbs_and_a_verb_with_a_wrong_shape_prints_it() {
    let out = Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args(["describe", "endpoints", "--project", "helsinki"])
        .output()
        .expect("jcctl runs");
    assert_eq!(out.status.code(), Some(1));
    let usage = String::from_utf8_lossy(&out.stderr);
    for verb in [
        "jcctl get <plural>",
        "jcctl describe <plural> <name>",
        "jcctl apply -f <file>",
        "jcctl diff -f <file>",
        "jcctl delete -f <file>",
    ] {
        assert!(usage.contains(verb), "{verb} missing from the usage");
    }
    let output_on_a_write = Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args(["apply", "-f", "x.yaml", "-o", "yaml"])
        .output()
        .expect("jcctl runs");
    assert!(String::from_utf8_lossy(&output_on_a_write.stderr).starts_with("usage:"));
}
