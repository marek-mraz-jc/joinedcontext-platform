//! A pipeline as a way out of its project's runner (T-1701, PL-16, PL-18, PL-50, PL-52).
//!
//! A project's runner holds every pipeline of that project in one Bento process: the
//! `pipeline-secrets` environment with each source's credential under its own `envVar`, the
//! runner's own `JC_CLIENT_SECRET` for the gateway, and the `/data` volume every file source
//! reads. A `DataSource` cannot name any of that — a `${VAR}` has to name an entry of its own
//! `spec.secrets` — so a mapping or a processor step must not be the second door into it.
//!
//! Each case here is one step of the attack: read the environment, read the runner's files,
//! hide the name behind an expression, write the same thing in a processor instead of a
//! mapping, and name a processor that runs a program or opens a file. Every one asserts the
//! refusal and the reason a person can act on, and the mapping the platform documents
//! (`env("JC_ORG_DOMAIN")`, `env("JC_SPACE")`) keeps working, which is what makes these green
//! for the right reason rather than green because nothing passes.

use jc_core::kinds::Pipeline;

/// A pipeline whose one step is the mapping under test.
fn with_mapping(mapping: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata:
  name: air-index
  namespace: helsinki
spec:
  class: resident
  period: 30s
  sources:
    - dataSourceRef: {{ kind: DataSource, name: air-feed }}
  steps:
    - kind: bloblang
      bloblang: {mapping}
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-air
"#
    )
}

/// A pipeline whose one step is the processor under test, written as inline YAML.
fn with_processor(processor: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata:
  name: air-index
  namespace: helsinki
spec:
  class: resident
  period: 30s
  sources:
    - dataSourceRef: {{ kind: DataSource, name: air-feed }}
  steps:
    - processor:
        {processor}
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-air
"#
    )
}

fn refusal(yaml: &str) -> String {
    match Pipeline::from_yaml(yaml) {
        Err(error) => error.to_string(),
        Ok(pipeline) => pipeline.validate().expect_err("refused").to_string(),
    }
}

fn accepted(yaml: &str) {
    Pipeline::from_yaml(yaml)
        .expect("parses")
        .validate()
        .expect("valid");
}

/// PL-16, PL-50: the runner's environment is the project's credentials, and a mapping that reads
/// one writes it wherever the pipeline writes.
#[test]
fn a_mapping_that_reads_the_runners_environment_is_refused() {
    for name in [
        "JC_CLIENT_SECRET",
        "MQTT_PASSWORD",
        "DS_OTHER_FEED_TOKEN",
        "PATH",
        "JC_SPACE_NEIGHBOUR",
    ] {
        let message = refusal(&with_mapping(&format!("'root.leak = env(\"{name}\")'")));
        assert!(
            message.contains(&format!("env(\"{name}\")")),
            "{name}: {message}"
        );
        assert!(
            message.contains("JC_ORG_DOMAIN") && message.contains("PL-50"),
            "the refusal says what is readable instead: {message}"
        );
    }
}

/// PL-57, PF-84: the two the reconciler injects for a mapping are the two a mapping may read, or
/// the rule would refuse the entity ids every pipeline in the documentation mints.
#[test]
fn the_variables_the_reconciler_injects_stay_readable() {
    for mapping in [
        r#"'root.id = "urn:ngsi-ld:AirQualityObserved:" + env("JC_ORG_DOMAIN") + ":" + env("JC_SPACE") + ":" + this.id'"#,
        r#"'root.space = env("JC_SPACE_2")'"#,
        r#"'root.space = env( "JC_SPACE_10" )'"#,
        "'root = this'",
    ] {
        accepted(&with_mapping(mapping));
    }
    // The shape the Banská Bystrica indicator pipelines are written in, which is what a rule
    // written too narrowly would refuse: three reads, a method that shares a name with none of
    // them, and a comment.
    let indicator = r#"|
        let domain = env("JC_ORG_DOMAIN")
        let space = env("JC_SPACE")
        let source_space = env("JC_SOURCE_SPACE")   # the space the value was read from
        let ran = now().ts_format("2006-01-02T15:04:05Z07:00", "UTC")
        root.derivedFrom = "urn:ngsi-ld:Endpoint:%v:%v:%v".format($domain, $source_space, $source_space)
        root.id = "urn:ngsi-ld:KeyPerformanceIndicator:%v:%v:water".format($domain, $space)
        root.observedAt = $ran"#;
    accepted(&with_mapping(indicator));
}

/// PL-16, PL-18: `/data` is the project's shared files volume and the container's own filesystem
/// carries its service account token; a mapping reads the message and nothing else.
#[test]
fn a_mapping_that_reads_the_runners_files_or_host_is_refused() {
    for (call, needle) in [
        (
            r#"file("/var/run/secrets/kubernetes.io/serviceaccount/token")"#,
            "file(…)",
        ),
        (r#"file_json("/data/other-project.json")"#, "file_json(…)"),
        (r#"hostname()"#, "hostname(…)"),
    ] {
        let message = refusal(&with_mapping(&format!("'root.leak = {call}'")));
        assert!(message.contains(needle), "{call}: {message}");
        assert!(
            message.contains("reads the message"),
            "the refusal says what a mapping may read: {message}"
        );
    }
}

/// PL-16: a name the manifest does not spell cannot be checked before it runs, so it is refused
/// rather than read — `env(this.pick)` is how an allow-list of literals is walked around.
#[test]
fn an_environment_name_that_is_not_a_literal_is_refused() {
    for call in [
        r#"env(this.pick)"#,
        r#"env("JC_" + "CLIENT_SECRET")"#,
        r#"env(meta("name"))"#,
    ] {
        let message = refusal(&with_mapping(&format!("'root.leak = {call}'")));
        assert!(message.contains("env(…)"), "{call}: {message}");
        assert!(
            message.contains("literal string"),
            "the refusal says what it wanted: {message}"
        );
    }
}

/// PL-52: a `mapping` processor's body and a `${! … }` interpolation are Bloblang as much as an
/// inline mapping is, so the same door is closed wherever it is written.
#[test]
fn a_processor_step_reads_the_runner_no_more_than_a_mapping_does() {
    let cases = [
        (
            r#"mapping: 'root.leak = env("JC_CLIENT_SECRET")'"#,
            "env(\"JC_CLIENT_SECRET\")",
        ),
        (
            r#"log: { message: '${! env("MQTT_PASSWORD") }' }"#,
            "env(\"MQTT_PASSWORD\")",
        ),
        (
            r#"mutation: 'root.leak = file("/data/other-project.csv")'"#,
            "file(…)",
        ),
        (
            r#"branch: { processors: [ { mapping: 'root = env("JC_CLIENT_SECRET")' } ] }"#,
            "env(\"JC_CLIENT_SECRET\")",
        ),
    ];
    for (processor, needle) in cases {
        let message = refusal(&with_processor(processor));
        assert!(message.contains(needle), "{processor}: {message}");
    }
}

/// T-5223: the scan reads every character instead of guessing where Bloblang's strings and
/// comments end. A triple-quoted string (`""" " """`) once hid a real `env("…")` from a lexer
/// that knew only `"` and `'`, and Bento 1.22.0 printed the secret. Each text below either runs
/// the call on that image or only mentions it; both are refused, a mention for being
/// indistinguishable from a call to anything short of Bento's own parser.
#[test]
fn a_host_call_hidden_by_quotes_comments_or_expressions_is_refused() {
    for (mapping, needle) in [
        (
            r#"'root = """ " """ + env("SECRET_X")'"#,
            "env(\"SECRET_X\")",
        ),
        (
            r#"'root = """ # """ + env("SECRET_X")'"#,
            "env(\"SECRET_X\")",
        ),
        (
            "|\n        root = \"\"\"\n        \" \"\"\" + file(\"/data/other.csv\")",
            "file(…)",
        ),
        (r#"'root = "a" + env("JC_" + "CLIENT_SECRET")'"#, "env(…)"),
        (r#"'root = env("""SECRET_X""")'"#, "env(…)"),
        (r#"'root = env(meta("name"))'"#, "env(…)"),
        (
            r#"'root = this.(a -> env("SECRET_X"))'"#,
            "env(\"SECRET_X\")",
        ),
        ("|\n        root = env(\n          \"JC_SPACE\")", "env(…)"),
        (
            "'root = this.file_rel_json(\"x.json\")'",
            "file_rel_json(…)",
        ),
        (
            "'root = this # env(\"MQTT_PASSWORD\") is not readable here'",
            "env(\"MQTT_PASSWORD\")",
        ),
        (r#"'root.note = "call env(\"X\") at your peril"'"#, "env(…)"),
    ] {
        let message = refusal(&with_mapping(mapping));
        assert!(message.contains(needle), "{mapping}: {message}");
    }
    // A name that only shares letters with a host function, and a method of another name, are
    // still not calls.
    for mapping in [
        r#"'root.method = this.name.format("%v")'"#,
        r#"'root.p = this.profile("x").environment(1)'"#,
        "'root.file = this.file'",
        r#"'root.space = env("JC_SPACE") # env("JC_SPACE") again'"#,
    ] {
        accepted(&with_mapping(mapping));
    }
}

/// T-5223 (security critic F2): `import "…"` and `from "…"` load a mapping from the runner's
/// disk; Bento 1.22.0 ran both from a line's start, so every such line is refused.
#[test]
fn a_mapping_that_loads_another_from_the_runners_disk_is_refused() {
    for mapping in [
        "|\n        import \"/data/other-project/maps.blobl\"\n        root = this.apply(\"m\")",
        "|\n        root.a = 1\n          from \"/var/run/secrets/kubernetes.io/serviceaccount/token\"",
        "'from\"/data/x.blobl\"'",
    ] {
        let message = refusal(&with_mapping(mapping));
        assert!(
            message.contains("runner's disk") && message.contains("PL-50"),
            "{mapping}: {message}"
        );
    }
    let message = refusal(&with_processor(
        "branch: { request_map: \"import \\\"/data/m.blobl\\\"\\nroot = this\" }",
    ));
    assert!(message.contains("runner's disk"), "{message}");
    for mapping in [
        r#"'root.note = "measured from " + this.station'"#,
        r#"'root.imported = this.import_date'"#,
    ] {
        accepted(&with_mapping(mapping));
    }
}

/// T-5223: the `python` processor reaches no file, network or variable, but nothing caps its CPU
/// or memory in the runner every project shares, so it is refused with that reason, at the top
/// of a step and nested.
#[test]
fn the_python_processor_is_refused_until_the_runner_caps_it() {
    for processor in [
        "python: { script: 'while True: pass' }",
        "try: [ { python: { script: 'root = this' } } ]",
    ] {
        let message = refusal(&with_processor(processor));
        assert!(
            message.contains("processor `python`") && message.contains("no CPU or memory cap"),
            "{processor}: {message}"
        );
    }
}

/// PL-52: the runner's catalogue is not the platform's list. `command` and `subprocess` run a
/// program in the shared runner, `file` reads, writes and deletes its files, and `wasm` is the
/// compute kind (PL-34) — none of the four is a step a manifest may name.
#[test]
fn a_processor_that_runs_a_program_or_opens_a_file_is_not_a_step() {
    for name in ["command", "subprocess", "wasm", "file"] {
        let message = refusal(&with_processor(&format!("{name}: {{}}")));
        assert!(
            message.contains(&format!("unknown processor `{name}`")),
            "{name}: {message}"
        );
    }
    // The list is still a list and not an empty one: `mapping` is the processor every pipeline
    // in the documentation uses, and a refusal of it would make this suite pass for nothing.
    accepted(&with_processor("mapping: 'root = this'"));
}

/// PL-52 (T-2557): a processor that holds processors does not carry a refused one inside it,
/// however deep, and a field that only shares a refused name (`redis`'s `command`) is not one.
#[test]
fn a_refused_processor_nested_inside_another_is_refused() {
    for nested in [
        "try: [ { command: { name: sh } } ]",
        "catch: [ { subprocess: { name: sh } } ]",
        "branch: { processors: [ { file: { path: /data/x, codec: all-bytes } } ] }",
        "switch: [ { check: 'true', processors: [ { wasm: { module_path: /x.wasm } } ] } ]",
        "for_each: [ { try: [ { command: { name: sh } } ] } ]",
        "workflow: { branches: { a: { processors: [ { subprocess: { name: sh } } ] } } }",
        "retry: { processors: [ { label: x, command: { name: sh } } ] }",
    ] {
        let message = refusal(&with_processor(nested));
        assert!(message.contains("processor `"), "{nested}: {message}");
    }
    accepted(&with_processor(
        "branch: { processors: [ { redis: { url: 'redis://cache:6379', command: get, args_mapping: 'root = [ this.id ]' } } ] }",
    ));
    accepted(&with_processor("try: [ { mapping: 'root = this' } ]"));
}

/// PL-16 (T-2557): the host check reads a nested processor's Bloblang as it reads a step's own.
#[test]
fn a_nested_mapping_that_reads_the_runners_environment_is_refused() {
    let message = refusal(&with_processor(
        "try: [ { mapping: 'root.x = env(\"JC_CLIENT_SECRET\")' } ]",
    ));
    assert!(message.contains("JC_CLIENT_SECRET"), "{message}");
}

/// PL-52 (T-2557): the check a `bento.yaml` processor goes through is the one a step goes
/// through, exported for the renderer that reads `bento.yaml`.
#[test]
fn the_exported_processor_check_refuses_what_a_step_refuses() {
    use jc_core::kinds::pipeline::validate_processor;
    for refused in [
        serde_json::json!({ "command": { "name": "sh" } }),
        serde_json::json!({ "try": [ { "subprocess": { "name": "sh" } } ] }),
        serde_json::json!({ "mapping": "root = env(\"MQTT_PASSWORD\")" }),
        serde_json::json!({ "mapping": "root = this", "bloblang": "root = this" }),
        serde_json::json!({ "label": "only-a-label" }),
        serde_json::json!("mapping"),
    ] {
        assert!(
            validate_processor("pipeline.processors", &refused).is_err(),
            "{refused}"
        );
    }
    for fine in [
        serde_json::json!({ "mapping": "root = this" }),
        serde_json::json!({ "label": "enrich", "mapping": "root.d = env(\"JC_ORG_DOMAIN\")" }),
        serde_json::json!({ "redis": { "url": "redis://c:6379", "command": "get" } }),
    ] {
        validate_processor("pipeline.processors", &fine).expect("accepted");
    }
}
