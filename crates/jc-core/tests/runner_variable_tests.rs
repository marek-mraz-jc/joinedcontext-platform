//! T-3163: an author's step reads no other project's credential out of the runner (PL-16,
//! PL-07). The runner fills every `${NAME}` from its environment when a stream is loaded, and
//! that environment holds each project's `JC_CLIENT_SECRET_{PROJECT}` and every source's `DS_*`.

use jc_core::kinds::pipeline::validate_processor;
use jc_core::kinds::{DataSourceSpec, Pipeline};
use serde_json::{json, Value};

fn step(processor: Value) -> Result<(), String> {
    validate_processor("pipeline.processors", &processor).map_err(|e| e.to_string())
}

fn pipeline(secret_refs: &str, steps: &str) -> Result<(), String> {
    let yaml = format!(
        "apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata: {{ name: p, namespace: helsinki }}
spec:
  class: resident
  sources:
    - dataSourceRef: {{ kind: DataSource, name: feed }}
{secret_refs}  steps:
{steps}  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep
"
    );
    let parsed = Pipeline::from_yaml(&yaml).map_err(|e| e.to_string())?;
    parsed.validate().map_err(|e| e.to_string())
}

#[test]
fn a_step_naming_another_projects_credential_is_refused() {
    for processor in [
        json!({ "http": { "url": "${JC_GATEWAY_URL}/x", "oauth2": {
            "enabled": true, "client_key": "bbsk-pipelines",
            "client_secret": "${JC_CLIENT_SECRET_BBSK}", "token_url": "${JC_TOKEN_URL}" } } }),
        json!({ "mapping": "root.s = \"${DS_OTHER_PASSWORD}\"" }),
        json!({ "http": { "url": "https://feed.example/x", "headers": { "X-Key": "${DS_PRAHA_GOLEMIO_KEY:none}" } } }),
        json!({ "http": { "url": "https://feed.example/x", "headers": { "${JC_CLIENT_SECRET_PRAHA}": "1" } } }),
        json!({ "branch": { "processors": [ { "mapping": "root = \"${HOME}\"" } ] } }),
    ] {
        let refused = step(processor.clone()).expect_err(&processor.to_string());
        assert!(refused.contains("PL-16"), "{processor}: {refused}");
    }
}

#[test]
fn the_reaper_seed_and_bloblang_interpolation_still_pass() {
    // helsinki-pipeline-vehicles-reaper-bento.yaml, as the seed writes it.
    step(json!({ "http": {
        "url": "${JC_GATEWAY_URL}/api/endpoint/si6e/ngsi-ld/v1/entityOperations/delete",
        "verb": "POST",
        "oauth2": { "enabled": true, "client_key": "${JC_CLIENT_ID}",
            "client_secret": "${JC_CLIENT_SECRET}", "token_url": "${JC_TOKEN_URL}" }
    } }))
    .expect("the pipeline credential by its alias");
    step(json!({ "dedupe": { "cache": "c", "key": "${! json(\"id\") }" } })).expect("Bloblang");
    step(json!({ "mapping": "root.id = \"urn:ngsi-ld:A:\" + env(\"JC_SPACE_2\")" }))
        .expect("an injected space name");
}

#[test]
fn a_pipeline_step_reads_its_own_declared_variables_and_no_other() {
    let own = "  secretRefs:\n    - { name: extra, key: k, envVar: EXTRA }\n";
    pipeline(
        own,
        "    - processor:\n        http: { url: 'https://api.example/x?k=${EXTRA}' }\n",
    )
    .expect("its own variable");
    let refused = pipeline(
        own,
        "    - processor:\n        http: { url: 'https://api.example/x?k=${OTHER}' }\n",
    )
    .expect_err("another pipeline's variable");
    assert!(refused.contains("PL-16"), "{refused}");
    let refused = pipeline(
        "",
        "    - kind: bloblang\n      bloblang: 'root.s = \"${JC_CLIENT_SECRET_PRAHA}\"'\n",
    )
    .expect_err("a compute step");
    assert!(refused.contains("PL-16"), "{refused}");
}

#[test]
fn no_reference_takes_a_runner_variable_name() {
    let refused = pipeline(
        "  secretRefs:\n    - { name: extra, envVar: JC_CLIENT_SECRET_BBSK }\n",
        "    - kind: bloblang\n      bloblang: root = this\n",
    )
    .expect_err("a JC_ envVar");
    assert!(refused.contains("envVar"), "{refused}");

    let source: DataSourceSpec = serde_json::from_value(json!({
        "type": "http_client",
        "input": { "url": "https://feed.example/x", "headers": { "K": "${JC_CLIENT_SECRET_BBSK}" } },
        "secrets": [{ "name": "feed", "key": "k", "envVar": "JC_CLIENT_SECRET_BBSK" }]
    }))
    .expect("parses");
    assert!(source
        .validate()
        .unwrap_err()
        .to_string()
        .contains("envVar"));
}

#[test]
fn a_typed_connection_reads_no_runner_variable_anywhere() {
    let refused: DataSourceSpec = serde_json::from_value(json!({
        "type": "http",
        "http": { "url": "https://feed.example/x", "headers": { "X-Key": "${JC_CLIENT_SECRET_BBSK}" } }
    }))
    .expect("parses");
    assert!(refused
        .validate()
        .unwrap_err()
        .to_string()
        .contains("PL-16"));
    // A bare `$` is not an interpolation: MQTT's system and shared topics keep working.
    let mqtt: DataSourceSpec = serde_json::from_value(json!({
        "type": "mqtt",
        "mqtt": { "urls": ["tls://broker.example:8883"], "topics": ["$SYS/broker/load/#", "$share/g/a/+"] }
    }))
    .expect("parses");
    mqtt.validate().expect("bare $ in a topic");
}
