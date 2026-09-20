//! `jcctl model` against a Model Tools that answers from a script (T-0406, DM-02, DM-19, DM-32).
//!
//! The fake speaks the same HTTP the image speaks — one answer per connection, always
//! `Content-Length`, then close — so these exercise the real client, the real repository walk
//! and the real exit codes rather than a mock of them.

mod common;

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

const PIN: &str = "linkml-1.11.1";
const MODEL_PATH: &str = "projects/ovzdusie/spaces/ovzdusie/datamodels/parking-spot.yaml";
const MODEL_DIR: &str = "projects/ovzdusie/spaces/ovzdusie/datamodels";

const MODEL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: parking-spot
  namespace: ovzdusie
spec:
  contextSpaceRef: ovzdusie
  linkml: parking-spot.linkml.yaml
  version: 1.0.0
  lifecycle: draft
  classes: ["ParkingSpot"]
  artifacts:
    jsonSchema: json-schema/parking-spot.v1.json
    context: context/parking-spot.v1.jsonld
    docs: docs/parking-spot.md
    example: examples/parking-spot.example.jsonld
"#;

const SOURCE: &str = "id: https://banskabystrica.sk/parking\nname: parking\n";

/// What a Model Tools that renders the whole DM-02 set answers.
fn full_answer() -> String {
    json!({
        "generatorVersion": PIN,
        "jsonSchema": {"$schema": "http://json-schema.org/draft-07/schema#", "title": "ParkingSpot"},
        "context": {"@context": {"status": "https://banskabystrica.sk/parking/status"}},
        "docs": "# ParkingSpot\n\nOne parking spot.\n",
        "example": {"id": "urn:ngsi-ld:ParkingSpot:banskabystrica.sk:parking:1"},
        "shacl": "@prefix sh: <http://www.w3.org/ns/shacl#> .\n",
        "owl": "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n",
        "errors": []
    })
    .to_string()
}

/// A Model Tools that answers `routes` and 404s everything else. The thread is detached; the
/// test binary is what ends it.
fn spawn(routes: Vec<(&'static str, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("a bound address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (status, body) = match routes.iter().find(|(route, _)| *route == path) {
                Some((_, body)) => (200, body.clone()),
                None => (404, json!({"errors": [format!("no {path}")]}).to_string()),
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Reads one request: the head, then as many body bytes as `Content-Length` announced.
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

/// A repository holding one DataModel, its source and the pin.
fn repo(test: &str, pin: &str) -> std::path::PathBuf {
    let dir = common::demo_repo(test);
    common::write(&dir, MODEL_PATH, MODEL);
    common::write(
        &dir,
        &format!("{MODEL_DIR}/parking-spot.linkml.yaml"),
        SOURCE,
    );
    common::write(
        &dir,
        "platform-settings.yaml",
        &format!("retention:\n  history: 90d\nmodelTools:\n  image: ghcr.io/x@sha256:0\n  generatorVersion: {pin}\n"),
    );
    dir
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jcctl"))
        .args(args)
        .env_remove("JC_MODEL_TOOLS_URL")
        .output()
        .expect("jcctl runs")
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is one JSON document ({err}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn artifact(dir: &Path, relative: &str) -> String {
    std::fs::read_to_string(dir.join(MODEL_DIR).join(relative))
        .unwrap_or_else(|err| panic!("{relative}: {err}"))
}

#[test]
fn generate_writes_the_four_committed_artifacts() {
    let dir = repo("model-generate", PIN);
    let url = spawn(vec![
        (
            "/healthz",
            json!({"status": "ok", "generatorVersion": PIN}).to_string(),
        ),
        ("/generate", full_answer()),
    ]);

    let output = run(&[
        "model",
        "generate",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--url",
        &url,
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = report(&output);
    assert_eq!(json["generatorVersion"], PIN);
    assert_eq!(json["models"][0]["written"].as_array().unwrap().len(), 4);
    assert_eq!(json["stale"], 0);
    assert_eq!(json["failed"], 0);

    // JSON artifacts are written pretty-printed and Markdown as it came, both ending in one
    // newline, so a review reads a diff and not a reflowed line.
    assert!(artifact(&dir, "json-schema/parking-spot.v1.json")
        .contains("\n  \"title\": \"ParkingSpot\""));
    assert!(artifact(&dir, "context/parking-spot.v1.jsonld").starts_with("{\n  \"@context\""));
    assert_eq!(
        artifact(&dir, "docs/parking-spot.md"),
        "# ParkingSpot\n\nOne parking spot.\n"
    );
    assert!(
        artifact(&dir, "examples/parking-spot.example.jsonld").contains("urn:ngsi-ld:ParkingSpot")
    );

    // SHACL and OWL are rendered and are not part of the committed set (DM-02, DM-44).
    assert!(!dir.join(MODEL_DIR).join("shacl").exists());
}

#[test]
fn generate_is_idempotent_and_rewrites_nothing_it_already_matches() {
    let dir = repo("model-generate-twice", PIN);
    let url = spawn(vec![
        ("/healthz", json!({"generatorVersion": PIN}).to_string()),
        ("/generate", full_answer()),
    ]);
    let args = [
        "model",
        "generate",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--url",
        &url,
    ];

    assert!(run(&args).status.success());
    let second = run(&args);
    assert!(second.status.success());
    assert!(
        report(&second)["models"][0]["written"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a second run writes nothing"
    );
}

#[test]
fn diff_names_the_stale_artifact_and_exits_two() {
    let dir = repo("model-diff", PIN);
    let url = spawn(vec![
        ("/healthz", json!({"generatorVersion": PIN}).to_string()),
        ("/generate", full_answer()),
    ]);
    let repo_dir = dir.to_str().unwrap().to_owned();

    assert!(
        run(&["model", "generate", "--repo-dir", &repo_dir, "--url", &url])
            .status
            .success()
    );
    let clean = run(&["model", "diff", "--repo-dir", &repo_dir, "--url", &url]);
    assert!(
        clean.status.success(),
        "a fresh repository has no stale artifact"
    );

    // What a hand-edited artifact looks like: DM-01 says these are generated, never edited.
    std::fs::write(
        dir.join(MODEL_DIR).join("docs/parking-spot.md"),
        "# edited by hand\n",
    )
    .unwrap();

    let output = run(&["model", "diff", "--repo-dir", &repo_dir, "--url", &url]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a stale artifact is a pending change"
    );
    let json = report(&output);
    assert_eq!(json["stale"], 1);
    assert_eq!(
        json["models"][0]["stale"][0],
        format!("{MODEL_DIR}/docs/parking-spot.md")
    );
    // `diff` reports; it does not fix.
    assert_eq!(artifact(&dir, "docs/parking-spot.md"), "# edited by hand\n");
}

#[test]
fn a_model_tools_of_another_version_than_the_pin_is_refused() {
    let dir = repo("model-pin", PIN);
    let url = spawn(vec![
        (
            "/healthz",
            json!({"generatorVersion": "linkml-1.9.0"}).to_string(),
        ),
        ("/generate", full_answer()),
    ]);

    let output = run(&[
        "model",
        "generate",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--url",
        &url,
    ]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(PIN) && stderr.contains("linkml-1.9.0"),
        "{stderr}"
    );
    assert!(stderr.contains("DM-19"), "{stderr}");
    assert!(
        !dir.join(MODEL_DIR).join("docs/parking-spot.md").exists(),
        "nothing is written before the pin is checked"
    );
}

#[test]
fn a_repository_with_no_pin_cannot_be_generated_into() {
    let dir = repo("model-no-pin", PIN);
    std::fs::remove_file(dir.join("platform-settings.yaml")).unwrap();
    let url = spawn(vec![(
        "/healthz",
        json!({"generatorVersion": PIN}).to_string(),
    )]);

    let output = run(&[
        "model",
        "generate",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--url",
        &url,
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("DM-19"));
}

#[test]
fn a_declared_artifact_the_service_does_not_render_fails_the_run() {
    // A Model Tools older than the four DM-02 artifacts, or one whose renderer for an artifact
    // failed: the declared file must fail the run by name rather than be committed empty.
    let dir = repo("model-missing", PIN);
    let url = spawn(vec![
        ("/healthz", json!({"generatorVersion": PIN}).to_string()),
        (
            "/generate",
            json!({
                "generatorVersion": PIN,
                "jsonSchema": {"title": "ParkingSpot"},
                "context": {"@context": {}},
                "shacl": "",
                "owl": "",
                "errors": []
            })
            .to_string(),
        ),
    ]);

    let output = run(&[
        "model",
        "generate",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--url",
        &url,
    ]);
    assert_eq!(output.status.code(), Some(1));
    let json = report(&output);
    assert_eq!(json["failed"], 1);
    assert_eq!(
        json["models"][0]["missing"],
        json!([
            format!("{MODEL_DIR}/docs/parking-spot.md"),
            format!("{MODEL_DIR}/examples/parking-spot.example.jsonld")
        ])
    );
    // The two it did render are still written: a partial set is visible, not silently absent.
    assert!(dir
        .join(MODEL_DIR)
        .join("json-schema/parking-spot.v1.json")
        .exists());
}

#[test]
fn validate_reports_what_does_not_compile_and_writes_nothing() {
    let dir = repo("model-validate", PIN);
    let url = spawn(vec![
        // Another version on purpose: `validate` reads no artifact and writes none, so it is
        // not the command a wrong generator can corrupt anything with.
        (
            "/healthz",
            json!({"generatorVersion": "linkml-1.9.0"}).to_string(),
        ),
        (
            "/generate",
            json!({
                "generatorVersion": "linkml-1.9.0",
                "errors": ["parking-spot.linkml.yaml: slot `status` has no range"]
            })
            .to_string(),
        ),
    ]);

    let output = run(&[
        "model",
        "validate",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--url",
        &url,
    ]);
    assert_eq!(output.status.code(), Some(1));
    let json = report(&output);
    assert_eq!(
        json["models"][0]["errors"][0],
        "parking-spot.linkml.yaml: slot `status` has no range"
    );
    assert!(!dir.join(MODEL_DIR).join("json-schema").exists());
}

#[test]
fn a_model_that_does_not_compile_writes_no_half_set() {
    let dir = repo("model-broken", PIN);
    let url = spawn(vec![
        ("/healthz", json!({"generatorVersion": PIN}).to_string()),
        (
            "/generate",
            json!({
                "generatorVersion": PIN,
                "jsonSchema": {"title": "half"},
                "errors": ["the source does not parse"]
            })
            .to_string(),
        ),
    ]);

    let output = run(&[
        "model",
        "generate",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--url",
        &url,
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(!dir.join(MODEL_DIR).join("json-schema").exists());
}

#[test]
fn import_writes_the_linkml_a_catalogue_model_becomes() {
    let dir = common::temp_dir("model-import");
    let out = dir.join("air-quality.linkml.yaml");
    let url = spawn(vec![
        ("/healthz", json!({"generatorVersion": PIN}).to_string()),
        (
            "/import-sdm",
            json!({
                "generatorVersion": PIN,
                "linkml": "id: https://smart-data-models.github.io/air\nname: air\n",
                "jsonSchema": {},
                "errors": []
            })
            .to_string(),
        ),
    ]);

    let output = run(&[
        "model",
        "import",
        "dataModel.Environment/AirQualityObserved",
        "--out",
        out.to_str().unwrap(),
        "--url",
        &url,
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "id: https://smart-data-models.github.io/air\nname: air\n"
    );
    assert_eq!(
        report(&output)["model"],
        "dataModel.Environment/AirQualityObserved"
    );
}

#[test]
fn an_unreachable_model_tools_is_an_error_and_not_an_empty_report() {
    let dir = repo("model-unreachable", PIN);
    // Port 1 is reserved and nothing listens on it.
    let output = run(&[
        "model",
        "diff",
        "--repo-dir",
        dir.to_str().unwrap(),
        "--url",
        "http://127.0.0.1:1",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot reach Model Tools"));
}
