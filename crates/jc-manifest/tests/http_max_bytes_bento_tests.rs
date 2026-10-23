//! T-5355: the rendered body bound of an `http` DataSource (PL-113) run in the Bento the runner
//! pins: a body of `maxBytes` passes to the author's steps unchanged, one byte more is refused
//! with its size in the run log and never reaches them. Only the input is replaced, by a
//! `generate` that answers the bodies a server would.

use std::process::Command;

use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::DataSourceSpec;
use jc_manifest::bento::{render, InputContext};
use serde_json::{json, Value};

/// `components/pipeline-runner/images.yaml` of joinedcontext-deployment.
const BENTO: &str = "ghcr.io/warpstreamlabs/bento:1.22.0@sha256:ef674bebae45793c982c350d8f37c2c7661133c1dc65a5517fb977b36dc09841";

#[test]
fn a_body_over_max_bytes_is_refused_before_the_authors_steps() {
    let source = ResourceEnvelope::<DataSourceSpec>::from_yaml(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: DataSource\n\
         metadata: { name: feed, namespace: helsinki }\n\
         spec:\n  type: http\n  http:\n    url: https://example.org/feed.json\n    maxBytes: 16\n",
    )
    .expect("parses");
    source.validate().expect("valid");
    let context = InputContext::new("feed", "helsinki", "feed-rows");
    let authored = "pipeline:\n  processors:\n    - mapping: 'root = {\"bytes\": content().length(), \"body\": content().string()}'\n";
    let rendered: Value =
        serde_norway::from_str(&render(authored, &source.spec, &context).expect("renders"))
            .expect("yaml");
    // Bodies of 16 and 17 bytes, one of them not JSON: the bound reads bytes, never parses.
    let config = json!({
        "input": {"generate": {"count": 2, "interval": "",
            "mapping": "root = if count(\"bodies\") == 1 { \"0123456789abcdef\" } else { \"0123456789abcdefg\" }"}},
        "pipeline": rendered["pipeline"],
        "output": {"stdout": {}},
        "logger": {"level": "ERROR", "format": "logfmt"}
    });
    let run = Command::new("docker")
        .args(["run", "--rm", "-e"])
        .arg(format!("CONFIG={config}"))
        .args([
            "--entrypoint",
            "sh",
            BENTO,
            "-c",
            r#"printf "%s" "$CONFIG" > /tmp/c.json && exec /bento -c /tmp/c.json 2>&1"#,
        ])
        .output()
        .expect("this test runs the runner's Bento in Docker; install Docker to run it");
    let out = String::from_utf8(run.stdout).expect("utf-8");
    assert!(run.status.success(), "{out}");
    let rows: Vec<Value> = out
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert_eq!(rows, [json!({"bytes": 16, "body": "0123456789abcdef"})]);
    assert!(
        out.contains("http read refused: failed assignment (line 1): the response body is 17 bytes, over spec.http.maxBytes 16"),
        "{out}"
    );
}
