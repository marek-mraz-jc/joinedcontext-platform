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

/// `awk`, `javascript` and `template` ship with the runner and read its files or environment
/// (measured on the pinned image: `awk`'s `getline`, `ENVIRON` and `system()`, `javascript`'s
/// `require` and `bento.v0_fetch`, `template`'s Sprig `env`): refused at the top of a step and
/// nested, and never offered in the refusal's list of accepted processors.
#[test]
fn processors_that_read_the_runner_are_refused_in_a_step() {
    use jc_core::kinds::pipeline::validate_processor;
    for processor in [
        serde_json::json!({ "awk": { "codec": "text", "program": "BEGIN { system(\"id\") }" } }),
        serde_json::json!({ "javascript": { "code": "require('/data/other.json')" } }),
        serde_json::json!({ "template": { "text": "{{ env \"JC_CLIENT_SECRET\" }}" } }),
        serde_json::json!({ "try": [{ "awk": { "program": "{ print ENVIRON[\"X\"] }" } }] }),
        serde_json::json!({ "branch": { "processors": [{ "javascript": { "code": "1" } }] } }),
    ] {
        let err = validate_processor("spec.steps.processor", &processor)
            .expect_err(&processor.to_string())
            .to_string();
        assert!(
            ["awk", "javascript", "template"]
                .iter()
                .any(|p| err.contains(p)),
            "{processor}: {err}"
        );
    }
    let unknown = validate_processor("spec.steps.processor", &serde_json::json!({ "nope": {} }))
        .expect_err("unknown")
        .to_string();
    assert!(
        unknown.contains("mapping") && !unknown.contains("awk") && !unknown.contains("javascript"),
        "{unknown}"
    );
}

/// `file_rel_json` reads a file like `file` does (the pinned image returns the file's JSON).
#[test]
fn a_mapping_cannot_read_a_file_with_file_rel_json() {
    use jc_core::kinds::pipeline::validate_processor;
    let err = validate_processor(
        "spec.steps.processor",
        &serde_json::json!({ "mapping": "root = file_rel_json(\"/data/other.json\")" }),
    )
    .expect_err("refused")
    .to_string();
    assert!(err.contains("file_rel_json"), "{err}");
}

/// One runner carries every project's streams, and its caches are declared once for all of
/// them: a step names a cache by its short name, which the Portal renders as
/// `<project>__<name>`, so a name already carrying a prefix would reach another project's cache
/// (T-5337).
#[test]
fn a_cache_label_with_a_project_prefix_is_refused() {
    use jc_core::kinds::pipeline::validate_processor;
    for processor in [
        serde_json::json!({ "cache": { "resource": "helsinki__gtfs", "operator": "get", "key": "k" } }),
        serde_json::json!({ "cached": { "cache": "praha__x", "key": "k", "processors": [] } }),
        serde_json::json!({ "dedupe": { "cache": "_gtfs", "key": "k" } }),
        serde_json::json!({ "branch": { "processors": [{ "cache": { "resource": "Gtfs-2" } }] } }),
        serde_json::json!({ "cache": { "resource": ["gtfs"] } }),
    ] {
        let err = validate_processor("spec.steps.processor", &processor)
            .expect_err(&processor.to_string())
            .to_string();
        assert!(err.contains("short name"), "{processor}: {err}");
    }
    for processor in [
        serde_json::json!({ "cache": { "resource": "gtfs", "operator": "set", "key": "k" } }),
        serde_json::json!({ "dedupe": { "cache": "trip_updates_2", "key": "k" } }),
    ] {
        if let Err(err) = validate_processor("spec.steps.processor", &processor) {
            panic!("{processor}: {err}");
        }
    }
}

/// Bloblang's `cache_*` functions name any runner cache by any label, past the per-project
/// names: a mapping reaches a cache only through a `cache` step (T-5337).
#[test]
fn a_mapping_cannot_reach_a_cache_by_function() {
    use jc_core::kinds::pipeline::validate_processor;
    for function in ["cache_get", "cache_set", "cache_add", "cache_delete"] {
        let err = validate_processor(
            "spec.steps.processor",
            &serde_json::json!({ "mapping": format!("root = {function}(\"helsinki__gtfs\", \"k\")") }),
        )
        .expect_err(function)
        .to_string();
        assert!(err.contains(function) && err.contains("cache"), "{err}");
    }
}

/// A file reader keeps to its own project's folder of the shared files volume (T-5337).
#[test]
fn a_file_reader_keeps_to_its_projects_folder() {
    use jc_core::Kind;
    let meta = jc_core::ObjectMeta {
        name: "stations".to_owned(),
        namespace: Some("helsinki".to_owned()),
        ..Default::default()
    };
    for other in [
        "type: csv\ninput:\n  paths: [/data/praha/stations.csv]\n",
        "type: file\ninput:\n  paths: [/data/stations.csv]\n",
        "type: broker\ninput:\n  inputs:\n    - parquet:\n        paths: [/data/helsinkix/a.parquet]\n",
        "type: fsevent\ninput:\n  paths: [/data/helsinki/../praha]\n",
    ] {
        let err = source(other)
            .validate_spec(&meta)
            .expect_err(other)
            .to_string();
        assert!(err.contains("/data/<project>/"), "{other}: {err}");
    }
    source("type: csv\ninput:\n  paths: [/data/helsinki/stations.csv]\n")
        .validate_spec(&meta)
        .expect("the project's own folder is accepted");
}
