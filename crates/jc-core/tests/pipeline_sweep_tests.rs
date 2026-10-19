//! The pipeline security sweep (T-3833): what a runner `DataSource` may not do in the shared
//! runner, one test per finding. Each refusal names the field a person wrote.

use jc_core::kinds::data_source::DataSourceSpec;

fn source(yaml: &str) -> DataSourceSpec {
    serde_norway::from_str(yaml).expect("parses")
}

fn refused(yaml: &str) -> String {
    source(yaml).validate().expect_err("refused").to_string()
}

/// Inputs that run a program, read the runner's own process or the cluster API, or reach
/// another stream inside the runner: none of them reads a feed.
#[test]
fn inputs_that_reach_the_runner_itself_are_refused() {
    for (input, config) in [
        ("subprocess", "name: /bin/sh\n  args: [-c, env]"),
        ("stdin", "codec: lines"),
        ("kubernetes_watch", "resource: secrets"),
        ("inproc", "other-project-stream"),
        ("dynamic", "inputs: {}"),
        ("resource", "other-projects-input"),
    ] {
        let msg = refused(&format!("type: {input}\ninput:\n  {config}\n"));
        assert!(msg.contains("spec.type"), "{input}: {msg}");
    }
}

/// A wrapper input carries inputs of its own: each is held to the rules of a top-level one.
#[test]
fn an_input_nested_in_a_wrapper_is_checked_as_a_top_level_one() {
    let msg =
        refused("type: sequence\ninput:\n  inputs:\n    - subprocess:\n        name: /bin/sh\n");
    assert!(msg.contains("subprocess"), "{msg}");

    let msg = refused(
        "type: broker\ninput:\n  inputs:\n    - file:\n        paths: [/var/run/secrets/kubernetes.io/serviceaccount/token]\n",
    );
    assert!(msg.contains("/data/"), "{msg}");

    source("type: broker\ninput:\n  inputs:\n    - file:\n        paths: [/data/a.csv]\n")
        .validate()
        .expect("a nested file input under /data/ is accepted");
}

/// `fsevent` watches paths like the file readers do, so it keeps to `/data/` too.
#[test]
fn fsevent_watches_only_the_files_volume() {
    let msg = refused("type: fsevent\ninput:\n  paths: [/etc]\n");
    assert!(msg.contains("spec.input.paths"), "{msg}");
    source("type: fsevent\ninput:\n  paths: [/data/drop]\n")
        .validate()
        .expect("/data/ is accepted");
}

/// An input's Bloblang (`generate.mapping`, a `${! … }` in any field) reads the message, never
/// the runner's environment or files, as a step's does.
#[test]
fn an_inputs_bloblang_reads_neither_env_nor_files() {
    let msg = refused("type: generate\ninput:\n  mapping: root = env(\"JC_CLIENT_SECRET\")\n");
    assert!(msg.contains("env(\"JC_CLIENT_SECRET\")"), "{msg}");

    let msg = refused("type: generate\ninput:\n  mapping: root = file(\"/data/other.csv\")\n");
    assert!(msg.contains("file(…)"), "{msg}");

    let msg = refused(
        "type: http_client\ninput:\n  url: https://feed.example/\n  headers:\n    X-Leak: '${! env(\"DS_OTHER_KEY\") }'\n",
    );
    assert!(msg.contains("env(\"DS_OTHER_KEY\")"), "{msg}");

    source("type: generate\ninput:\n  mapping: 'root = {\"at\": now()}'\n  interval: 1m\n")
        .validate()
        .expect("a mapping over the message alone is accepted");
}

/// An input's own `processors` are pipeline steps in all but name: the refused processors stay
/// refused there.
#[test]
fn an_inputs_processors_are_held_to_the_step_rules() {
    let msg = refused(
        "type: http_client\ninput:\n  url: https://feed.example/\n  processors:\n    - command:\n        name: /bin/sh\n",
    );
    assert!(msg.contains("command"), "{msg}");

    let msg = refused(
        "type: http_client\ninput:\n  url: https://feed.example/\n  processors:\n    - try:\n        - subprocess:\n            name: /bin/sh\n",
    );
    assert!(msg.contains("subprocess"), "{msg}");
}
