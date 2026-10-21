//! T-4845: `spec.history` checked against the space's model (PL-106) and compiled into the one
//! Bloblang step that sets each planned `observedAt` (PL-107), run here in the Bento the runner
//! pins: a time from a slot (string and typed literal), a slot that gives no time, `received`,
//! an attribute with several instances, an absent attribute and an array of entities.

use std::process::Command;

use jc_core::kinds::{Pipeline, PipelineSpec};
use jc_manifest::history::{check, step};
use jc_manifest::model_schema::ModelSchema;
use serde_json::{json, Value};

/// `components/pipeline-runner/images.yaml` of joinedcontext-deployment.
const BENTO: &str = "ghcr.io/warpstreamlabs/bento:1.22.0@sha256:ef674bebae45793c982c350d8f37c2c7661133c1dc65a5517fb977b36dc09841";

fn model() -> ModelSchema {
    let schema = json!({ "$defs": {
        "Vehicle": { "type": "object", "properties": {
            "id": {"type": "string"}, "type": {"type": "string"},
            "location": {"type": "object"}, "speed": {"type": "number"},
            "delay": {"type": "number"}, "observedOn": {"type": "string", "format": "date-time"},
            "routeName": {"type": "string"}
        }}
    }});
    ModelSchema::compile("helsinki", "1.0.0", &schema, &["Vehicle".to_owned()], false)
}

fn spec(history: &str) -> PipelineSpec {
    let yaml = format!(
        r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata: {{ name: hfp, namespace: helsinki }}
spec:
  class: resident
  sources:
    - dataSourceRef: {{ kind: DataSource, name: feed }}
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-city-write
  history:
{history}"#
    );
    let pipeline = Pipeline::from_yaml(&yaml).expect("parses");
    pipeline.validate().expect("valid");
    pipeline.spec
}

const PLAN: &str = "    - type: Vehicle
      attributes:
        - { names: [location, speed], keep: changes, heartbeat: PT15M, observedAt: observedOn }
        - { names: [delay], keep: every, observedAt: received }
        - { names: [routeName], keep: latest }
";

#[test]
fn the_model_check_names_the_slot_and_the_rule() {
    check(&spec(PLAN), &model()).expect("a plan of the model's slots");
    let refusal = |plan: &str| {
        check(&spec(plan), &model())
            .expect_err("refused")
            .to_string()
    };
    let said = refusal("    - type: Bus\n      attributes: [{ names: [a], keep: every }]\n");
    assert!(
        said.contains("spec.history[0].type") && said.contains("no class `Bus`"),
        "{said}"
    );
    let said =
        refusal("    - type: Vehicle\n      attributes: [{ names: [colour], keep: every }]\n");
    assert!(
        said.contains("`colour` is not a slot of `Vehicle`"),
        "{said}"
    );
    let said = refusal(
        "    - type: Vehicle\n      attributes: [{ names: [speed], keep: every, observedAt: routeName }]\n",
    );
    assert!(
        said.contains("attributes[0].observedAt") && said.contains("not a date-time slot"),
        "{said}"
    );
}

#[test]
fn a_plan_without_a_time_renders_no_step() {
    let plan = "    - type: Vehicle\n      attributes: [{ names: [speed], keep: latest }]\n";
    assert_eq!(step(&spec(plan)).expect("renders"), None);
}

#[test]
fn a_name_that_skipped_validation_never_reaches_the_step() {
    let mut forged = spec(PLAN);
    forged.history[0].attributes[0].names = vec!["a\" }\nroot = deleted()\n#".to_owned()];
    assert!(step(&forged)
        .expect_err("refused")
        .to_string()
        .contains("not a slot name"));
    let mut forged = spec(PLAN);
    forged.history[0].attributes[0].observed_at = Some("x.y\n".to_owned());
    assert!(step(&forged).is_err());
}

/// Runs `mapping` in Bento over each record, one message per record; returns each output.
fn run(mapping: &str, records: &[Value]) -> Vec<Value> {
    let lines: Vec<String> = records.iter().map(Value::to_string).collect();
    let config = json!({
        "input": {"generate": {"count": 1, "interval": "", "mapping": format!("root = {}", json!(lines.join("\n")))}},
        "pipeline": {"processors": [
            {"unarchive": {"format": "lines"}},
            {"mapping": "root = content().parse_json()"},
            {"mapping": mapping}
        ]},
        "output": {"stdout": {}},
        "logger": {"level": "none"}
    });
    let run = Command::new("docker")
        .args(["run", "--rm", "-e"])
        .arg(format!("CONFIG={config}"))
        .args([
            "--entrypoint",
            "sh",
            BENTO,
            "-c",
            r#"printf "%s" "$CONFIG" > /tmp/c.json && exec /bento -c /tmp/c.json"#,
        ])
        .output()
        .expect("this test runs the runner's Bento in Docker; install Docker to run it");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8(run.stdout)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("not a row: {line:.300}")))
        .collect()
}

#[test]
fn the_generated_step_sets_each_planned_time_in_the_runners_bento() {
    let text = step(&spec(PLAN)).expect("compiles").expect("a step");
    let rows = run(
        &text,
        &[
            json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle",
                "observedOn": {"type": "Property", "value": "2026-10-20T13:00:00.250+03:00"},
                "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [24.9, 60.1]}},
                "speed": [{"type": "Property", "value": 7, "observedAt": "2020-01-01T00:00:00Z"},
                          {"type": "Property", "value": 8, "datasetId": "urn:ngsi-ld:Dataset:gps"}],
                "delay": {"type": "Property", "value": 30, "observedAt": "2026-10-20T10:00:00Z"},
                "routeName": {"type": "Property", "value": "550"}}),
            json!({"id": "urn:ngsi-ld:Vehicle:2", "type": "Vehicle",
                "observedOn": {"type": "Property", "value": {"@type": "DateTime", "@value": "2026-10-20T10:05:00Z"}},
                "speed": {"type": "Property", "value": 3}}),
            json!({"id": "urn:ngsi-ld:Vehicle:3", "type": "Vehicle",
                "observedOn": {"type": "Property", "value": "not a time"},
                "speed": {"type": "Property", "value": 4, "observedAt": "2026-10-20T10:00:00Z"}}),
            json!([{"id": "urn:ngsi-ld:Vehicle:4", "type": "Vehicle",
                    "observedOn": {"type": "Property", "value": "2026-10-20T10:09:00Z"},
                    "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [1, 2]}}},
                   {"id": "urn:ngsi-ld:Other:1", "type": "Other", "speed": {"type": "Property", "value": 1}}]),
        ],
    );
    let by_id = |id: &str| {
        rows.iter()
            .find(|doc| doc["id"] == id)
            .or_else(|| rows.iter().find(|doc| doc[0]["id"] == id))
            .unwrap_or_else(|| panic!("{id} in {rows:?}"))
            .clone()
    };

    let one = by_id("urn:ngsi-ld:Vehicle:1");
    assert_eq!(
        one["location"]["observedAt"], "2026-10-20T10:00:00.25Z",
        "{one}"
    );
    assert_eq!(
        one["speed"][0]["observedAt"], "2026-10-20T10:00:00.25Z",
        "{one}"
    );
    assert_eq!(
        one["speed"][1]["observedAt"], "2026-10-20T10:00:00.25Z",
        "{one}"
    );
    assert_eq!(
        one["speed"][1]["datasetId"], "urn:ngsi-ld:Dataset:gps",
        "{one}"
    );
    assert_eq!(
        one["delay"].get("observedAt"),
        None,
        "received drops the mapping's time: {one}"
    );
    assert_eq!(one["delay"]["value"], 30);
    assert_eq!(one["routeName"].get("observedAt"), None, "{one}");

    let two = by_id("urn:ngsi-ld:Vehicle:2");
    assert_eq!(
        two["speed"]["observedAt"], "2026-10-20T10:05:00Z",
        "a typed literal: {two}"
    );
    assert_eq!(
        two.get("location"),
        None,
        "an absent attribute stays absent: {two}"
    );

    let three = by_id("urn:ngsi-ld:Vehicle:3");
    assert_eq!(
        three["speed"].get("observedAt"),
        None,
        "no time from the slot, none kept: {three}"
    );
    assert_eq!(three["speed"]["value"], 4, "the record is kept: {three}");

    let batch = by_id("urn:ngsi-ld:Vehicle:4");
    assert_eq!(
        batch[0]["location"]["observedAt"], "2026-10-20T10:09:00Z",
        "{batch}"
    );
    assert_eq!(
        batch[1]["speed"].get("observedAt"),
        None,
        "another type passes: {batch}"
    );
}
