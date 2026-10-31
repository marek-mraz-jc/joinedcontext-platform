//! T-3162: no stream reaches the runner's own pod or its environment through a URL a person
//! typed (PL-07, PL-16, MF-39). One runner pod serves every project, and its stream API
//! (`:4195`) and token sidecar (`:4180`) answer on loopback, where no NetworkPolicy or mesh rule
//! sees the call.

use jc_core::kinds::pipeline::validate_processor;
use jc_core::kinds::DataSourceSpec;
use serde_json::{json, Value};

/// Hosts that are the runner's own pod or a private address, in the forms a resolver accepts.
const INTERNAL: &[&str] = &[
    "http://127.0.0.1:4195/streams/x",
    "http://localhost:4180/token",
    "http://[::1]/",
    "http://LOCALHOST./",
    "http://api.localhost/",
    "http://0.0.0.0:4195/",
    "http://10.42.0.17:4195/streams",
    "http://169.254.169.254/latest/meta-data",
    "http://192.168.1.1/",
    "http://172.16.0.1/",
    "http://100.64.0.1/",
    "http://2130706433/",
    "http://127.1/",
    "http://0x7f.0.0.1/",
    "http://[::ffff:127.0.0.1]/",
    "http://[fd00::1]/",
    "http://[fe80::1]/",
    "http://user:pw@127.0.0.1/",
    "http://%31%32%37.0.0.1/",
];

fn http_source(url: &str) -> Result<(), String> {
    let spec: DataSourceSpec = serde_json::from_value(json!({
        "type": "http",
        "http": { "url": url }
    }))
    .map_err(|e| e.to_string())?;
    spec.validate().map_err(|e| e.to_string())
}

fn step(processor: Value) -> Result<(), String> {
    validate_processor("pipeline.processors", &processor).map_err(|e| e.to_string())
}

#[test]
fn a_data_source_url_to_the_runner_or_a_private_address_is_refused_with_a_reason() {
    for url in INTERNAL {
        let refused = http_source(url).expect_err(url);
        assert!(
            refused.contains("PL-07"),
            "{url} was refused without the reason: {refused}"
        );
    }
    for url in [
        "https://opendata.banskabystrica.sk/aq.json",
        "http://feed.example:8080/x?a=1",
    ] {
        http_source(url).unwrap_or_else(|e| panic!("{url}: {e}"));
    }
}

#[test]
fn every_typed_connection_url_is_checked() {
    let mqtt: DataSourceSpec = serde_json::from_value(json!({
        "type": "mqtt",
        "mqtt": { "urls": ["tcp://127.0.0.1:1883"], "topics": ["a/#"] }
    }))
    .expect("parses");
    assert!(mqtt.validate().is_err());
    let ws: DataSourceSpec = serde_json::from_value(json!({
        "type": "websocket",
        "webSocket": { "url": "ws://localhost:4195/ws" }
    }))
    .expect("parses");
    assert!(ws.validate().is_err());
}

#[test]
fn a_data_source_url_reads_no_runner_variable_but_the_gateway_and_token_addresses() {
    for url in [
        "https://feed.example/?k=${JC_CLIENT_SECRET_BBSK}",
        "https://feed.example/${DS_OTHER_PASSWORD}",
        "https://feed.example/${! env(\"JC_CLIENT_SECRET\") }",
        "https://${JC_CLIENT_SECRET}.feed.example/",
    ] {
        let refused = http_source(url).expect_err(url);
        assert!(refused.contains("PL-16"), "{url}: {refused}");
    }
    // The seed reads the gateway in-cluster this way (helsinki-datasource-vehicles-live).
    http_source("http://${JC_GATEWAY_HOST}/api/endpoint/abc/ngsi-ld/v1/entities?type=Vehicle")
        .expect("the gateway host is not a credential");
}

#[test]
fn a_runner_input_that_reaches_the_runner_is_refused_and_a_cluster_service_is_not() {
    let source = |input: Value| -> Result<(), String> {
        let spec: DataSourceSpec =
            serde_json::from_value(json!({ "type": "http_client", "input": input }))
                .map_err(|e| e.to_string())?;
        spec.validate().map_err(|e| e.to_string())
    };
    let refused = source(json!({ "url": "http://127.0.0.1:4195/streams", "verb": "GET" }))
        .expect_err("loopback");
    assert!(refused.contains("spec.input.url"), "{refused}");
    // A Service by name is the NetworkPolicy's and the mesh's to decide (the demo NATS feed).
    let nats: DataSourceSpec = serde_json::from_value(json!({
        "type": "nats",
        "input": { "urls": ["nats://demo-nats.demo.svc.cluster.local:4222"], "subject": "c.>" }
    }))
    .expect("parses");
    nats.validate().expect("a cluster service by name");
}

#[test]
fn a_step_that_calls_the_runner_or_a_private_address_is_refused_however_deep() {
    for url in INTERNAL {
        let top = step(json!({ "http": { "url": url, "verb": "PUT" } })).expect_err(url);
        assert!(top.contains("PL-07"), "{url}: {top}");
        step(json!({ "branch": { "processors": [ { "try": [ { "http": { "url": url } } ] } ] } }))
            .expect_err(url);
    }
    step(json!({ "redis": { "url": "redis://127.0.0.1:6379", "command": "get", "args_mapping": "root = [this.id]" } }))
        .expect_err("redis on loopback");
    step(json!({ "http": { "url": "https://data.example/second-source?id=${! this.id }" } }))
        .expect("a public second source, the path taken from the record");
    // The reaper seed writes to the gateway through the runner's own variable.
    step(json!({ "http": { "url": "${JC_GATEWAY_URL}/api/endpoint/x/ngsi-ld/v1/entityOperations/delete", "verb": "POST" } }))
        .expect("the gateway address from the runner");
}

#[test]
fn a_step_whose_host_comes_from_the_message_is_refused() {
    for processor in [
        json!({ "http": { "url": "${! this.link }" } }),
        json!({ "http": { "url": "  ${!meta(\"target\")}" } }),
        json!({ "http": { "url": "http://${! this.host }:4195/streams" } }),
        json!({ "nats_request_reply": { "urls": ["${! this.broker }"], "subject": "x" } }),
    ] {
        let refused = step(processor.clone()).expect_err(&processor.to_string());
        assert!(refused.contains("PL-07"), "{processor}: {refused}");
    }
}

/// T-4312: the token Services by their cluster names, in every form a resolver accepts.
const TOKEN_SERVICES: &[&str] = &[
    "http://pipeline-tokens:4180/token",
    "http://pipeline-tokens.pipeline-runner/token",
    "http://pipeline-tokens.pipeline-runner.svc:4180/token",
    "http://Pipeline-Tokens.pipeline-runner.svc.cluster.local.:4180/token",
    "http://pipeline-runner-tokens.any-namespace.svc.cluster.local:4180/",
    "http://wasm-host-tokens.wasm-host.svc.cluster.local:4180/token",
    "http://client:secret@pipeline-tokens.pipeline-runner.svc.cluster.local:4180/token",
];

#[test]
fn a_pipeline_or_data_source_naming_a_token_service_is_refused() {
    for url in TOKEN_SERVICES {
        let refused = http_source(url).expect_err(url);
        assert!(refused.contains("PL-07"), "{url}: {refused}");
        let refused = step(json!({ "http": { "url": url, "verb": "POST" } })).expect_err(url);
        assert!(refused.contains("PL-07"), "{url}: {refused}");
    }
    // A Service whose name only contains the word stays the NetworkPolicy's to decide.
    for url in [
        "http://my-pipeline-tokens-feed.demo.svc.cluster.local/x",
        "http://feed.pipeline-tokens.example/x",
    ] {
        http_source(url).unwrap_or_else(|e| panic!("{url}: {e}"));
    }
}

/// F-R2PLT3-012: a written-in address is judged by the outbound guard's one rule, so the ranges
/// the old local list missed (multicast, benchmarking, reserved, documentation) are refused too.
#[test]
fn every_address_off_the_internet_is_refused_by_the_one_rule() {
    use jc_core::kinds::data_source::refused_target;
    for url in [
        "http://224.0.0.1/",
        "http://198.18.0.1/feed",
        "http://240.0.0.1/",
        "http://192.0.0.8/",
        "http://[ff02::1]/",
        "http://[64:ff9b::a00:1]/",
        "http://10.43.0.10/",
        "http://[::ffff:127.0.0.1]/",
    ] {
        assert!(refused_target(url).is_some(), "{url} was accepted");
    }
    for url in ["https://opendata.example.org/air.json", "http://1.1.1.1/"] {
        assert_eq!(refused_target(url), None, "{url}");
    }
}
