//! T-0421: `plan` and `apply` against a gateway that answers the CIM 009 space surface
//! (CC-04, CC-16, CC-18, CC-50, CC-72).
//!
//! The stub is a gateway, not a mock of one: it holds entities per space, answers a
//! retrieval by id with `404` when it holds none, and applies an `entityOperations/upsert`
//! the way a broker behind a gateway would, so the real client and the real create-or-replace
//! decision are what these exercise.

mod common;

use jcctl::commands::seed;
use jcctl::entities::Action;
use jcctl::gateway::{Broker, BrokerError, Gateway};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "eyJhbGciOiJSUzI1NiJ9.the-reconcilers-serviceaccount-token";

/// One request the stub answered.
#[derive(Debug, Clone)]
struct Call {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Debug, Default)]
struct Platform {
    /// Entities by space, then by id, as the broker holds them.
    spaces: BTreeMap<String, BTreeMap<String, Value>>,
    calls: Vec<Call>,
    /// When set, every write is refused the way a Policy refusal reads.
    refuse_writes: bool,
}

impl Platform {
    fn writes(&self) -> Vec<&Call> {
        self.calls
            .iter()
            .filter(|call| call.method == "POST")
            .collect()
    }

    /// `/cs/{space}/ngsi-ld/v1/{tail}` → the answer, as a broker behind the gateway gives it.
    fn answer(&mut self, call: &Call) -> (u16, Value) {
        let Some(rest) = call.path.strip_prefix("/cs/") else {
            return (404, json!({ "title": "no such path" }));
        };
        let Some((space, tail)) = rest.split_once("/ngsi-ld/v1/") else {
            return (404, json!({ "title": "no such path" }));
        };
        let space = space.to_owned();

        if call.method == "GET" {
            let id = percent_decode(tail.strip_prefix("entities/").unwrap_or_default());
            return match self.spaces.get(&space).and_then(|held| held.get(&id)) {
                Some(entity) => (200, entity.clone()),
                None => (404, json!({ "title": "Resource not found" })),
            };
        }
        if call.method == "POST" && tail == "entityOperations/upsert" {
            if self.refuse_writes {
                return (
                    403,
                    json!({ "title": "no Policy of this space grants create" }),
                );
            }
            let held = self.spaces.entry(space).or_default();
            for entity in call.body.as_array().cloned().unwrap_or_default() {
                let id = entity["id"].as_str().unwrap_or_default().to_owned();
                // What a broker adds of its own, and what the comparison must ignore.
                let mut stored = entity.clone();
                stored["createdAt"] = json!("2026-09-17T09:00:00Z");
                stored["modifiedAt"] = json!("2026-09-17T09:00:00Z");
                held.insert(id, stored);
            }
            return (204, Value::Null);
        }
        (404, json!({ "title": "no such path" }))
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 1024];
    while let Ok(read) = stream.read(&mut buffer) {
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if body.len() >= length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

fn spawn(platform: Arc<Mutex<Platform>>) -> String {
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
            let authorization = lines.find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            });
            let call = Call {
                method,
                path,
                authorization,
                body: serde_json::from_str(body).unwrap_or(Value::Null),
            };

            let (status, answer) = {
                let mut platform = platform.lock().expect("the platform");
                platform.calls.push(call.clone());
                platform.answer(&call)
            };
            let body = if answer.is_null() {
                String::new()
            } else {
                answer.to_string()
            };
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
    format!("http://127.0.0.1:{port}")
}

fn platform() -> (Arc<Mutex<Platform>>, Gateway) {
    let platform = Arc::new(Mutex::new(Platform::default()));
    let base = spawn(Arc::clone(&platform));
    (platform, gateway_at(&base))
}

/// A client holding the token the way the CLI builds one: out of a file on disk, which in the
/// cluster is the projected ServiceAccount token.
fn gateway_at(base: &str) -> Gateway {
    let dir = common::temp_dir("seed-token");
    let file = dir.join("token");
    std::fs::write(&file, format!("{TOKEN}\n")).expect("the token file");
    let token = Gateway::token_from(&file).expect("the token reads");
    Gateway::new(base, token).expect("a client")
}

fn air(local: &str, index: u32) -> String {
    json!({
        "id": format!("urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:{local}"),
        "type": "AirQualityObserved",
        "airQualityIndex": { "type": "Property", "value": index }
    })
    .to_string()
}

/// A checkout seeding two entities into `ovzdusie` and one into `doprava`.
fn checkout(name: &str) -> PathBuf {
    let root = common::temp_dir(name);
    common::write(
        &root,
        "projects/mesto/spaces/ovzdusie/entities/seed/stations.json",
        &format!("[{},{}]", air("1", 42), air("2", 17)),
    );
    common::write(
        &root,
        "projects/mesto/spaces/doprava/entities/seed/counter.json",
        &json!({
            "id": "urn:ngsi-ld:TrafficFlowObserved:bb.sk:doprava:1",
            "type": "TrafficFlowObserved",
            "intensity": { "type": "Property", "value": 120 }
        })
        .to_string(),
    );
    root
}

#[test]
fn apply_replays_every_seed_entity_and_a_second_run_writes_nothing() {
    let (platform, gateway) = platform();
    let repo = checkout("seed-replay");

    let first = seed::apply(&repo, &gateway).expect("the replay runs");
    assert_eq!(first.changes.len(), 3);
    assert!(
        first
            .changes
            .iter()
            .all(|change| change.action == Action::Create),
        "an empty broker holds none of them: {:?}",
        first.changes
    );

    {
        let held = platform.lock().expect("the platform");
        assert_eq!(
            held.writes().len(),
            2,
            "one upsert per space, not per entity"
        );
        for call in held.writes() {
            assert!(
                call.path.ends_with("/ngsi-ld/v1/entityOperations/upsert"),
                "the write is the CIM 009 batch operation: {}",
                call.path
            );
            assert_eq!(
                call.authorization.as_deref(),
                Some(format!("Bearer {TOKEN}").as_str()),
                "every call carries the ServiceAccount token (CC-04)"
            );
        }
        assert_eq!(
            held.spaces["ovzdusie"].len(),
            2,
            "the space holds what the repository declared"
        );
        assert_eq!(held.spaces["doprava"].len(), 1);
    }

    let second = seed::apply(&repo, &gateway).expect("the second run");
    assert!(
        second
            .changes
            .iter()
            .all(|change| change.action == Action::Unchanged),
        "the broker already holds every entity as declared: {:?}",
        second.changes
    );
    assert!(second.is_clean());
    assert_eq!(
        platform.lock().expect("the platform").writes().len(),
        2,
        "an unchanged repository issues no writing call (CC-18)"
    );
}

#[test]
fn plan_reports_what_apply_would_do_and_writes_nothing() {
    let (platform, gateway) = platform();
    let repo = checkout("seed-plan");

    let before = seed::plan(&repo, &gateway).expect("the plan runs");
    assert_eq!(before.pending(), 3);
    assert!(
        platform.lock().expect("the platform").writes().is_empty(),
        "plan is read-only (CC-15)"
    );

    seed::apply(&repo, &gateway).expect("the replay runs");
    let after = seed::plan(&repo, &gateway).expect("the plan runs again");
    assert!(after.is_clean(), "{}", after.render());
    assert!(after.render().contains("unchanged ovzdusie"));
}

#[test]
fn an_entity_changed_in_the_repository_is_the_only_one_written_again() {
    let (platform, gateway) = platform();
    let repo = checkout("seed-changed");
    seed::apply(&repo, &gateway).expect("the first replay");

    common::write(
        &repo,
        "projects/mesto/spaces/ovzdusie/entities/seed/stations.json",
        &format!("[{},{}]", air("1", 42), air("2", 99)),
    );

    let report = seed::apply(&repo, &gateway).expect("the second replay");
    let updated: Vec<&str> = report
        .changes
        .iter()
        .filter(|change| change.action != Action::Unchanged)
        .map(|change| change.id.as_str())
        .collect();
    assert_eq!(
        updated,
        vec!["urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:2"],
        "only the entity whose declared value moved"
    );

    let held = platform.lock().expect("the platform");
    let last = held.writes().last().copied().expect("a second write");
    assert_eq!(
        last.body.as_array().map(Vec::len),
        Some(1),
        "the space with nothing to do is not called at all"
    );
    assert_eq!(
        held.spaces["ovzdusie"]["urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:2"]
            ["airQualityIndex"]["value"],
        json!(99)
    );
}

#[test]
fn a_platform_that_refuses_the_write_fails_the_run_and_says_what_it_said() {
    let (platform, gateway) = platform();
    platform.lock().expect("the platform").refuse_writes = true;
    let repo = checkout("seed-refused");

    let error = seed::apply(&repo, &gateway).expect_err("a refused write is a failed run");
    let said = error.to_string();
    assert!(said.contains("403"), "{said}");
    assert!(
        said.contains("no Policy of this space grants create"),
        "{said}"
    );
    assert!(
        !said.contains(TOKEN),
        "the refusal repeats the token: {said}"
    );
}

#[test]
fn a_gateway_that_is_not_there_is_named_without_its_credential() {
    let gateway = gateway_at("http://127.0.0.1:1");
    let error = gateway
        .entity(
            "ovzdusie",
            "urn:ngsi-ld:AirQualityObserved:bb.sk:ovzdusie:1",
        )
        .expect_err("nothing listens on port 1");
    assert!(
        matches!(error, BrokerError::Unavailable { .. }),
        "{error:?}"
    );
    assert!(!error.to_string().contains(TOKEN), "{error}");
}

#[test]
fn an_address_that_carries_a_credential_or_no_host_is_refused_before_any_call() {
    let dir = common::temp_dir("seed-token-checks");
    let file = dir.join("token");
    std::fs::write(&file, TOKEN).expect("the token file");
    let token = || Gateway::token_from(&file).expect("the token reads");

    for bad in [
        "http://reconciler:s3cret@gateway:9090",
        "ftp://gateway:9090",
        "not a url",
    ] {
        let error = Gateway::new(bad, token()).expect_err(&format!("{bad} is not usable"));
        assert!(!error.to_string().contains(TOKEN), "{error}");
    }

    std::fs::write(&file, "   \n").expect("an empty token file");
    let empty = Gateway::token_from(&file).expect("the file reads");
    assert!(Gateway::new("http://gateway:9090", empty).is_err());
}
