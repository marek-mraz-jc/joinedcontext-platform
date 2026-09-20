//! Testing a candidate pipeline before it is proposed (T-0590, PL-43, MF-38, Architecture/08 §7).
//!
//! A mapping is tested where it will run: as an ephemeral stream on the project's Bento runner,
//! never as a subprocess of the Portal. This module is the pure half of that: the harness the
//! runner is handed, and the trace read back from what the harness sent to the capture route.
//! The `DataSource` input becomes a `generate` input emitting the sample once (or an
//! `http_client` input for a sample URL, so the fetch obeys the runner's own egress policy,
//! PL-23), the author's processors stay what they are, and the output becomes an `http_client`
//! POST of one envelope per message: the input the message was, what the mapping made of it,
//! and the error when a processor failed. Nothing here resolves a `secretRef` or reaches the
//! target endpoint (MF-38).

use jc_core::kinds::{ComputeKind, PipelineSpec, Step};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How many messages one test may produce: enough to see every column, small enough to read.
pub const MAX_MESSAGES: usize = 20;
/// The largest inline sample (API/01 §7a).
pub const MAX_SAMPLE_BYTES: usize = 5 * 1024 * 1024;
/// The stream id prefix on the runner: `pipeline-test-{id}`.
pub const STREAM_PREFIX: &str = "pipeline-test-";
/// What a failed fetch of a sample URL prefixes its own error with, so the reader of a trace
/// can tell "the feed did not answer" from "the mapping failed" (T-1192).
pub const FETCH_FAILED: &str = "fetch: ";
/// What a fetch that answered nothing reports, which is a failure the same way a 404 is.
pub const EMPTY_BODY: &str = "the feed answered an empty body";

/// What the sample is, so the harness knows how to split it into messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SampleFormat {
    /// One message as it is, bytes untouched.
    #[default]
    Text,
    /// A header row and one message per data row, every cell a string.
    Csv,
    /// A JSON document; an array becomes one message per element.
    Json,
}

/// The sample of one test: inline text, or a URL the runner fetches itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    /// The sample inline, at most `MAX_SAMPLE_BYTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// An http(s) URL the runner fetches the sample from instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// How the sample is split into messages.
    #[serde(default)]
    pub format: SampleFormat,
}

/// Why a harness cannot be built from this manifest and sample.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HarnessError {
    /// Neither `text` nor `url` was given.
    #[error("sample: give either text or url")]
    NoSample,
    /// Both `text` and `url` were given.
    #[error("sample: text and url together; give one")]
    TwoSamples,
    /// The inline sample is past `MAX_SAMPLE_BYTES`.
    #[error("sample: {0} bytes is over the {MAX_SAMPLE_BYTES} byte limit")]
    TooLarge(usize),
    /// The sample URL is not http or https.
    #[error("sample: url '{0}' is not http or https")]
    BadUrl(String),
    /// The pipeline's compute stage is not a Bento processor this harness can run.
    #[error("compute kind {0} is not tested as a Bento processor")]
    NotABentoProcessor(ComputeKind),
}

/// The mapping that marks step `at` as the one being run, for a message that has not failed yet.
/// `root` is left alone, so the message travels untouched — the same shape as `jc_input` above.
fn step_marker(at: usize) -> String {
    format!("meta jc_step = if errored() {{ meta(\"jc_step\").or(\"\") }} else {{ \"{at}\" }}")
}

/// The Bento stream config that tests `spec` on `sample`, posting every message to `capture_url`.
pub fn harness(
    spec: &PipelineSpec,
    sample: &Sample,
    capture_url: &str,
) -> Result<Value, HarnessError> {
    // A URL sample is fetched by a processor rather than read by an input, which the branch
    // below explains; the prelude that does it is pushed once the processor list exists.
    let mut fetch_once = false;
    let input = match (&sample.text, &sample.url) {
        (Some(_), Some(_)) => return Err(HarnessError::TwoSamples),
        (None, None) => return Err(HarnessError::NoSample),
        (Some(text), None) => {
            if text.len() > MAX_SAMPLE_BYTES {
                return Err(HarnessError::TooLarge(text.len()));
            }
            // Base64 keeps every byte of the sample out of Bloblang's own string syntax.
            let encoded = base64_encode(text.as_bytes());
            json!({ "generate": {
                "count": 1,
                "interval": "",
                "mapping": format!("root = \"{encoded}\".decode(\"base64\")"),
            }})
        }
        (None, Some(url)) => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(HarnessError::BadUrl(url.clone()));
            }
            // Not an `http_client` input: that one polls without pause, and a 1.2 MB feed
            // reached the capture route many times a second until the Portal was OOM-killed
            // (T-1192). One `generate` message fetched by an `http` processor is one fetch.
            fetch_once = true;
            json!({ "generate": { "count": 1, "interval": "", "mapping": "root = \"\"" } })
        }
    };

    let mut processors = Vec::new();
    if fetch_once {
        let url = sample.url.as_deref().unwrap_or_default();
        processors.push(json!({ "http": {
            "url": url, "verb": "GET", "timeout": "3s", "retries": 0
        }}));
        // An empty body is a failure with a name, rather than a message every later processor
        // fails on for a reason that says nothing about the feed.
        processors.push(json!({ "mapping": format!(
            "root = if !errored() && content().length() == 0 {{ throw(\"{EMPTY_BODY}\") }} else {{ content() }}"
        )}));
        // The fetch's own error is kept in metadata, where the envelope reads it before the
        // error of whatever the empty message failed at next.
        processors.push(json!({ "catch": [{ "mapping": format!(
            "meta jc_fetch_error = \"{FETCH_FAILED}\" + error()\nroot = \"\""
        )}]}));
    }
    match sample.format {
        SampleFormat::Text => {}
        SampleFormat::Csv | SampleFormat::Json => {
            let parse = match sample.format {
                SampleFormat::Csv => "content().string().parse_csv()",
                _ => "content().parse_json()",
            };
            // The page stays one message, the way the reconciler renders a live stream: the
            // author's mapping runs over what the source answered, and the split into one
            // entity per element happens after it (PL-47, PL-48). Splitting first would test a
            // mapping against a shape production never gives it — an aggregate over the page
            // would read one entity and answer zero (T-0911).
            processors.push(json!({ "mapping": format!(
                "let rows = {parse}\nroot = if $rows.type() == \"array\" {{ $rows.slice(0, {MAX_MESSAGES}) }} else {{ $rows }}"
            )}));
        }
    }
    processors.push(json!({ "mapping": "meta jc_input = content().string()" }));
    // Every step in order, in either shape (PL-54): a processor step as the runner runs it.
    // Before each one the harness stamps that step's index, unless the message has already
    // failed — a failed message runs through the rest of the chain, so the number that survives
    // is the step the failure happened at rather than the last step to run (PL-43, PL-56).
    for (at, step) in spec.steps().into_iter().enumerate() {
        processors.push(json!({ "mapping": step_marker(at) }));
        match step {
            Step::Processor(step) => processors.push(json!(step.processor)),
            Step::Compute(compute) => match compute.kind {
                ComputeKind::Bloblang => {
                    if let Some(mapping) = &compute.bloblang {
                        processors.push(json!({ "mapping": mapping }));
                    }
                }
                kind => return Err(HarnessError::NotABentoProcessor(kind)),
            },
        }
    }
    // `let out = this` would read a failed message as JSON and fail again on a body that is
    // not JSON; the runner then posts the raw body to the capture route, is refused, and
    // retries until the stream is deleted (T-1192, Bento 1.21). Read only when it did not
    // fail, and report a failed fetch's own error before the one it caused downstream.
    processors.push(json!({ "mapping": concat!(
        "let failed = errored()\n",
        "let out = if $failed { null } else { this }\n",
        "root = {}\n",
        "root.input = meta(\"jc_input\")\n",
        "root.output = if $failed { null } else { $out }\n",
        "root.error = if $failed { meta(\"jc_fetch_error\").or(error()) } else { null }\n",
        "root.step = if $failed { meta(\"jc_step\").or(null) } else { null }"
    )}));
    // The envelope replaces the failed message, so the flag must not follow it to the output.
    processors.push(json!({ "catch": [] }));
    // A mapping that yields an array is one entity per element (PL-48), the same split the
    // reconciler renders for a live stream (PL-47); capped so a feed of thousands answers in
    // the same three seconds. A failed message has no array and passes through whole.
    processors.push(json!({ "mapping": format!(concat!(
        "root = if this.output.type() == \"array\" {{ ",
        "this.output.slice(0, {}).map_each(o -> {{ \"input\": this.input, \"output\": o, \"error\": null }}) ",
        "}} else {{ [this] }}"
    ), MAX_MESSAGES) }));
    processors.push(json!({ "unarchive": { "format": "json_array" } }));

    Ok(json!({
        "input": input,
        "pipeline": { "processors": processors },
        "output": { "http_client": {
            "url": capture_url,
            "verb": "POST",
            "timeout": "3s",
            "retries": 0,
            "max_in_flight": 1,
            // The capture route on the Portal's internal listener answers this project's runner and
            // nobody else (AG-52, T-2271), so the harness names itself the way every other pipeline
            // of this runner does: `client_credentials` on the runner's own Keycloak client, whose
            // audience mapper binds the token to that listener. The three variables are the runner
            // pod's own; a token never appears in this configuration, which the Portal stores.
            "oauth2": {
                "enabled": true,
                "client_key": "${JC_CLIENT_ID}",
                "client_secret": "${JC_CLIENT_SECRET}",
                "token_url": "${JC_TOKEN_URL}",
            },
        }},
    }))
}

/// One captured message, as the harness posted it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Captured {
    /// The raw input message, as text.
    #[serde(default)]
    pub input: Option<String>,
    /// What the mapping produced, when it succeeded.
    #[serde(default)]
    pub output: Option<Value>,
    /// The processor error, when it failed.
    #[serde(default)]
    pub error: Option<String>,
    /// The step it failed at, as the harness stamped it: an index into `spec.steps` as text,
    /// because Bento metadata is text. Absent when the message did not fail.
    #[serde(default)]
    pub step: Option<String>,
}

/// The input stage of the trace: what the runner read and the first message as data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputStage {
    /// Messages the runner read from the sample.
    pub events: usize,
    /// Their size in bytes, summed.
    pub bytes: usize,
    /// The first message as data, or as text when it is not JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Value>,
}

/// Whether one mapped message is an NGSI-LD entity the platform would accept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validation {
    /// Which mapped message, in capture order.
    pub index: usize,
    /// Whether the message is an entity the platform would accept.
    pub ok: bool,
    /// Why not, one line each.
    pub problems: Vec<String>,
}

/// One thing that went wrong, at the stage it went wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestError {
    /// `lint` (the runner refused the harness), `mapping` (a processor failed) or `runner`.
    pub stage: String,
    /// The step of `spec.steps` the message failed at (PL-52), for a `mapping` error. A `lint`
    /// error belongs to the document and a `runner` error to the stream, so both leave it None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<usize>,
    /// The Bloblang line the runner named, when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The runner's message, as it wrote it.
    pub message: String,
}

/// The trace of one test (Architecture/08 §7).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TestTrace {
    /// What the runner read.
    pub input: InputStage,
    /// Every mapped message, in capture order.
    pub mapping: Vec<Value>,
    /// One verdict per mapped message.
    pub validation: Vec<Validation>,
    /// What went wrong, at the stage it went wrong.
    pub errors: Vec<TestError>,
}

/// The trace of what the harness sent back.
pub fn trace(captured: &[Captured]) -> TestTrace {
    let mut trace = TestTrace::default();
    trace.input.events = captured.len();
    for (index, message) in captured.iter().enumerate() {
        let input = message.input.as_deref().unwrap_or_default();
        trace.input.bytes += input.len();
        if index == 0 {
            trace.input.sample = Some(
                serde_json::from_str(input).unwrap_or_else(|_| Value::String(input.to_owned())),
            );
        }
        if let Some(error) = &message.error {
            trace.errors.push(TestError {
                stage: "mapping".into(),
                step: message.step.as_deref().and_then(|at| at.parse().ok()),
                line: line_of(error),
                message: error.clone(),
            });
            continue;
        }
        let output = message.output.clone().unwrap_or(Value::Null);
        let problems = problems_of(&output);
        trace.validation.push(Validation {
            index: trace.mapping.len(),
            ok: problems.is_empty(),
            problems,
        });
        trace.mapping.push(output);
    }
    trace
}

/// The runner's refusal of the harness as lint errors, one per line that names a problem.
pub fn lint_errors(refusal: &str) -> Vec<TestError> {
    let lines: Vec<&str> = refusal
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return vec![TestError {
            stage: "lint".into(),
            step: None,
            line: None,
            message: "the runner refused the harness".into(),
        }];
    }
    lines
        .into_iter()
        .map(|line| TestError {
            stage: "lint".into(),
            step: None,
            line: line_of(line),
            message: line.to_owned(),
        })
        .collect()
}

/// The line a Bento error names (`line 3 char 7`, `(line 3)`, `:3:7:`), when it names one.
///
/// A harness line is a mapping line plus nothing: the author's mapping is its own processor,
/// so the number Bento reports is the number in the editor.
pub fn line_of(message: &str) -> Option<u32> {
    let after = message.find("line ")? + "line ".len();
    let digits: String = message[after..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// What keeps `value` from being an NGSI-LD entity the gateway would take (PF-42).
pub fn problems_of(value: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(object) = value.as_object() else {
        problems.push("the mapping did not produce an object".into());
        return problems;
    };
    let entity_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if entity_type.is_empty() {
        problems.push("no 'type'".into());
    }
    match object.get("id").and_then(Value::as_str) {
        None => problems.push("no 'id'".into()),
        Some(id) if !id.starts_with("urn:ngsi-ld:") => {
            problems.push(format!("id '{id}' does not start with urn:ngsi-ld:"));
        }
        Some(id)
            if !entity_type.is_empty()
                && !id.starts_with(&format!("urn:ngsi-ld:{entity_type}:")) =>
        {
            problems.push(format!("id '{id}' does not name the type '{entity_type}'"));
        }
        Some(_) => {}
    }
    problems
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(bloblang: Option<&str>) -> PipelineSpec {
        let mut value = json!({
            "class": "resident",
            "source": { "dataSourceRef": { "kind": "DataSource", "name": "shmu-csv" } },
            "targetEndpoint": "urn:ngsi-ld:Endpoint:hel.fi:helsinki:helsinki-all"
        });
        if let Some(mapping) = bloblang {
            value["compute"] = json!({ "kind": "bloblang", "bloblang": mapping });
        }
        serde_json::from_value(value).expect("a pipeline spec")
    }

    #[test]
    fn a_csv_sample_reaches_the_mapping_whole_and_its_output_is_posted_per_row() {
        let sample = Sample {
            text: Some("station_id,pm10\n01,18.2\n".into()),
            url: None,
            format: SampleFormat::Csv,
        };
        let config = harness(
            &spec(Some("root.id = this.station_id")),
            &sample,
            "http://portal:9090/internal/pipeline-tests/abc",
        )
        .expect("a harness");
        assert_eq!(config["input"]["generate"]["count"], 1);
        assert!(config["input"]["generate"]["mapping"]
            .as_str()
            .is_some_and(|m| m.ends_with(".decode(\"base64\")") && !m.contains("station_id")));
        let processors = config["pipeline"]["processors"]
            .as_array()
            .expect("processors");
        assert!(processors[0]["mapping"]
            .as_str()
            .is_some_and(|m| m.contains("parse_csv")));
        // The rows reach the author's mapping as one page, the way a live stream gives them
        // (PL-47, T-0911): parse, record the input, map.
        assert!(processors[1]["mapping"]
            .as_str()
            .is_some_and(|m| m.contains("meta jc_input")));
        // Then the step's own marker, then the step (PL-56).
        assert_eq!(processors[2]["mapping"], step_marker(0));
        assert_eq!(processors[3]["mapping"], "root.id = this.station_id");
        assert!(
            processors
                .iter()
                .take(4)
                .all(|p| p.get("unarchive").is_none()),
            "nothing splits the page before the mapping: {processors:?}"
        );
        let n = processors.len();
        assert_eq!(processors[n - 3]["catch"], json!([]));
        assert!(processors[n - 2]["mapping"].as_str().is_some_and(|m| m
            .contains("this.output.type() == \"array\"")
            && m.contains("slice(0, 20)")));
        assert_eq!(processors[n - 1]["unarchive"]["format"], "json_array");
        assert_eq!(
            config["output"]["http_client"]["url"],
            "http://portal:9090/internal/pipeline-tests/abc"
        );
        assert_eq!(config["output"]["http_client"]["verb"], "POST");
        // T-2271: the capture is a route of the internal listener, so the harness mints the runner's
        // own token for it — the secret is a variable of the runner pod and never a value here.
        let oauth2 = &config["output"]["http_client"]["oauth2"];
        assert_eq!(oauth2["enabled"], true);
        assert_eq!(oauth2["client_key"], "${JC_CLIENT_ID}");
        assert_eq!(oauth2["client_secret"], "${JC_CLIENT_SECRET}");
        assert_eq!(oauth2["token_url"], "${JC_TOKEN_URL}");
        assert!(
            !serde_json::to_string(&config)
                .expect("json")
                .contains("Bearer "),
            "no token is ever written into a harness"
        );
        assert!(
            config.get("resources").is_none(),
            "no secret, no resource: MF-38"
        );
    }

    #[test]
    fn a_url_sample_is_fetched_once_and_text_is_one_message() {
        let sample = Sample {
            text: None,
            url: Some("https://feeds.example/air.json".into()),
            format: SampleFormat::Text,
        };
        let config = harness(&spec(None), &sample, "http://portal/c").expect("a harness");
        // The input emits one empty message and a processor does the fetch. An `http_client`
        // input polls without pause, and a test is one fetch of one feed (T-1192).
        assert!(
            config["input"].get("http_client").is_none(),
            "a polling input reached the capture route many times a second: {config}"
        );
        assert_eq!(config["input"]["generate"]["count"], 1);
        let processors = config["pipeline"]["processors"]
            .as_array()
            .expect("processors");
        assert_eq!(
            processors[0]["http"]["url"],
            "https://feeds.example/air.json"
        );
        assert_eq!(processors[0]["http"]["retries"], 0);
        assert!(
            processors[1]["mapping"]
                .as_str()
                .expect("a mapping")
                .contains(EMPTY_BODY),
            "a feed that answers nothing fails with a name: {processors:?}"
        );
        assert!(
            processors[2]["catch"][0]["mapping"]
                .as_str()
                .expect("a mapping")
                .contains(FETCH_FAILED),
            "the fetch's own error is kept where the envelope reads it: {processors:?}"
        );
        assert_eq!(
            processors.len(),
            8,
            "the three of the fetch, then capture, envelope, catch, the array split and its \
             unarchive: nothing else (PL-48)"
        );
    }

    /// T-1192: what the Portal used to patch onto the harness after the fact, now written here.
    ///
    /// Both were bugs that only show on a URL sample that fails: a message read as JSON when
    /// it has already errored fails again on a body that is not JSON, the runner posts the raw
    /// body to the capture route, is refused, and retries until the stream is deleted.
    #[test]
    fn a_failed_message_is_not_read_as_json_and_a_failed_fetch_keeps_its_own_error() {
        let sample = Sample {
            text: None,
            url: Some("https://feeds.example/air.json".into()),
            format: SampleFormat::Text,
        };
        let config = harness(&spec(None), &sample, "http://portal/c").expect("a harness");
        let envelope = config["pipeline"]["processors"]
            .as_array()
            .expect("processors")
            .iter()
            .find_map(|processor| {
                processor["mapping"]
                    .as_str()
                    .filter(|mapping| mapping.starts_with("let failed = errored()"))
            })
            .expect("the envelope");
        assert!(
            envelope.contains("let out = if $failed { null } else { this }"),
            "a failed message is never read as JSON: {envelope}"
        );
        assert!(
            envelope.contains("meta(\"jc_fetch_error\").or(error())"),
            "the feed's own error is reported, not the one it caused: {envelope}"
        );
    }

    #[test]
    fn what_cannot_be_a_harness_is_refused_with_the_reason() {
        let none = Sample::default();
        assert_eq!(
            harness(&spec(None), &none, "http://p").unwrap_err(),
            HarnessError::NoSample
        );
        let both = Sample {
            text: Some("x".into()),
            url: Some("http://a".into()),
            format: SampleFormat::Text,
        };
        assert_eq!(
            harness(&spec(None), &both, "http://p").unwrap_err(),
            HarnessError::TwoSamples
        );
        let ftp = Sample {
            text: None,
            url: Some("ftp://a".into()),
            format: SampleFormat::Text,
        };
        assert!(matches!(
            harness(&spec(None), &ftp, "http://p").unwrap_err(),
            HarnessError::BadUrl(_)
        ));
        let big = Sample {
            text: Some("x".repeat(MAX_SAMPLE_BYTES + 1)),
            url: None,
            format: SampleFormat::Text,
        };
        assert!(matches!(
            harness(&spec(None), &big, "http://p").unwrap_err(),
            HarnessError::TooLarge(_)
        ));
        let wasm: PipelineSpec = serde_json::from_value(json!({
            "class": "resident",
            "compute": { "kind": "wasm", "module": "m", "function": "f" },
            "targetEndpoint": "urn:ngsi-ld:Endpoint:hel.fi:helsinki:helsinki-all"
        }))
        .expect("spec");
        let text = Sample {
            text: Some("x".into()),
            url: None,
            format: SampleFormat::Text,
        };
        assert!(matches!(
            harness(&wasm, &text, "http://p").unwrap_err(),
            HarnessError::NotABentoProcessor(ComputeKind::Wasm)
        ));
    }

    #[test]
    fn the_trace_reads_input_mapping_validation_and_errors_from_what_came_back() {
        let captured = vec![
            Captured {
                input: Some(r#"{"station_id":"01","pm10":"18.2"}"#.into()),
                output: Some(
                    json!({ "id": "urn:ngsi-ld:AirQualityObserved:hel.fi:aq:01", "type": "AirQualityObserved" }),
                ),
                error: None,
                step: None,
            },
            Captured {
                input: Some(r#"{"station_id":"02"}"#.into()),
                output: Some(json!({ "id": "station-02", "type": "AirQualityObserved" })),
                error: None,
                step: None,
            },
            Captured {
                input: Some("garbage".into()),
                output: None,
                error: Some("failed assignment (line 2): expected number, got string".into()),
                step: Some("2".into()),
            },
        ];
        let trace = trace(&captured);
        assert_eq!(trace.input.events, 3);
        let expected: usize = captured
            .iter()
            .map(|c| c.input.as_deref().unwrap_or_default().len())
            .sum();
        assert_eq!(trace.input.bytes, expected);
        assert_eq!(
            trace.input.sample,
            Some(json!({ "station_id": "01", "pm10": "18.2" }))
        );
        assert_eq!(trace.mapping.len(), 2);
        assert_eq!(
            trace.validation[0],
            Validation {
                index: 0,
                ok: true,
                problems: vec![]
            }
        );
        assert!(!trace.validation[1].ok);
        assert!(trace.validation[1].problems[0].contains("urn:ngsi-ld:"));
        assert_eq!(
            trace.errors,
            vec![TestError {
                stage: "mapping".into(),
                step: Some(2),
                line: Some(2),
                message: "failed assignment (line 2): expected number, got string".into()
            }]
        );
    }

    #[test]
    fn the_step_a_message_failed_at_is_the_one_stamped_before_it_ran() {
        // The marker only writes when the message has not failed, so a step behind the failure
        // cannot take the blame for it — which is the whole reason the guard is there.
        let marker = step_marker(3);
        assert!(marker.starts_with("meta jc_step = if errored() {"));
        assert!(marker.contains("meta(\"jc_step\").or(\"\")"));
        assert!(marker.ends_with("else { \"3\" }"));
        assert!(
            !marker.contains("root"),
            "the message travels untouched: {marker}"
        );

        // Every step has its own marker, and the envelope carries the number back.
        let second: PipelineSpec = serde_json::from_value(json!({
            "class": "resident",
            "sources": [{ "dataSourceRef": { "kind": "DataSource", "name": "shmu-csv" } }],
            "steps": [
                { "processor": { "jq": { "query": ".id" } } },
                { "kind": "bloblang", "bloblang": "root.id = this.id" },
                { "processor": { "log": { "message": "seen" } } }
            ],
            "outputs": [{ "targetEndpoint": "urn:ngsi-ld:Endpoint:hel.fi:helsinki:helsinki-all" }]
        }))
        .expect("a v1alpha2 spec");
        let config = harness(
            &second,
            &Sample {
                text: Some("{}".into()),
                url: None,
                format: SampleFormat::Json,
            },
            "http://portal:9090/internal/pipeline-tests/abc",
        )
        .expect("a harness");
        let processors = config["pipeline"]["processors"]
            .as_array()
            .expect("processors");
        let markers: Vec<&str> = processors
            .iter()
            .filter_map(|p| p["mapping"].as_str())
            .filter(|m| m.starts_with("meta jc_step ="))
            .collect();
        assert_eq!(
            markers,
            vec![step_marker(0), step_marker(1), step_marker(2)]
        );
        let envelope = processors
            .iter()
            .filter_map(|p| p["mapping"].as_str())
            .find(|m| m.contains("root.step"))
            .expect("the envelope carries the step");
        assert!(envelope
            .contains("root.step = if $failed { meta(\"jc_step\").or(null) } else { null }"));

        // A pipeline with no step stamps nothing: there is no step to blame, and the envelope
        // reads metadata that was never written, which `.or(null)` answers as no step.
        let bare = harness(
            &spec(None),
            &Sample {
                text: Some("{}".into()),
                url: None,
                format: SampleFormat::Json,
            },
            "http://p",
        )
        .expect("a harness");
        assert!(!bare["pipeline"]["processors"]
            .as_array()
            .expect("processors")
            .iter()
            .any(|p| p["mapping"]
                .as_str()
                .is_some_and(|m| m.starts_with("meta jc_step ="))));
    }

    #[test]
    fn only_a_mapping_error_names_a_step_and_an_unreadable_one_names_none() {
        let failed = |step: Option<&str>| {
            trace(&[Captured {
                input: Some("{}".into()),
                output: None,
                error: Some("failed assignment: no".into()),
                step: step.map(str::to_owned),
            }])
            .errors
        };
        assert_eq!(failed(Some("0"))[0].step, Some(0));
        assert_eq!(failed(Some("12"))[0].step, Some(12));
        // Metadata is text: anything that is not an index is no step at all, never a panic and
        // never a wrong step painted red.
        for bad in ["", "x", "-1", "1.5", "18446744073709551616"] {
            assert_eq!(failed(Some(bad))[0].step, None, "{bad}");
        }
        assert_eq!(failed(None)[0].step, None);
        // A lint error belongs to the document and a runner error to the stream.
        assert!(lint_errors("line 3 char 1: no")
            .iter()
            .all(|e| e.step.is_none()));
    }

    #[test]
    fn a_refusal_becomes_lint_errors_with_their_lines() {
        let errors = lint_errors("stream 'pipeline-test-x' failed to create:\n  line 12 char 3: expected string, got number\n");
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[1].line, Some(12));
        assert_eq!(errors[1].stage, "lint");
        assert_eq!(lint_errors("")[0].line, None);
        assert_eq!(line_of("no number here"), None);
    }

    #[test]
    fn an_entity_needs_a_type_and_an_id_that_names_it() {
        assert!(
            problems_of(&json!({ "id": "urn:ngsi-ld:Bike:hel.fi:b:1", "type": "Bike" })).is_empty()
        );
        assert_eq!(
            problems_of(&json!({ "id": "urn:ngsi-ld:Bike:hel.fi:b:1" })),
            vec!["no 'type'"]
        );
        assert_eq!(problems_of(&json!({ "type": "Bike" })), vec!["no 'id'"]);
        assert!(
            problems_of(&json!({ "id": "urn:ngsi-ld:Car:x:y:1", "type": "Bike" }))[0]
                .contains("does not name the type")
        );
        assert_eq!(
            problems_of(&json!([1])),
            vec!["the mapping did not produce an object"]
        );
    }
    #[test]
    fn a_second_version_pipeline_runs_every_step_in_order_and_refuses_one_that_is_not_bento() {
        let second: PipelineSpec = serde_json::from_value(json!({
            "class": "resident",
            "sources": [{ "dataSourceRef": { "kind": "DataSource", "name": "shmu-csv" } }],
            "steps": [
                { "kind": "bloblang", "bloblang": "root.id = this.station_id" },
                { "processor": { "log": { "message": "seen" } } }
            ],
            "outputs": [{ "targetEndpoint": "urn:ngsi-ld:Endpoint:hel.fi:helsinki:helsinki-all" }]
        }))
        .expect("a v1alpha2 spec");
        let sample = Sample {
            text: Some("station_id,pm10\n01,18.2\n".into()),
            url: None,
            format: SampleFormat::Csv,
        };
        let config = harness(
            &second,
            &sample,
            "http://portal:9090/internal/pipeline-tests/abc",
        )
        .expect("a harness");
        let processors = config["pipeline"]["processors"]
            .as_array()
            .expect("processors");
        assert_eq!(processors[2]["mapping"], step_marker(0));
        assert_eq!(processors[3]["mapping"], "root.id = this.station_id");
        assert_eq!(processors[4]["mapping"], step_marker(1));
        assert_eq!(processors[5]["log"]["message"], "seen");

        let mut wasm = second.clone();
        wasm.steps = vec![Step::Compute(
            serde_json::from_value(json!({
                "kind": "wasm", "module": "./compute", "function": "run"
            }))
            .expect("a wasm step"),
        )];
        assert!(matches!(
            harness(
                &wasm,
                &sample,
                "http://portal:9090/internal/pipeline-tests/abc"
            ),
            Err(HarnessError::NotABentoProcessor(ComputeKind::Wasm))
        ));
    }
}
