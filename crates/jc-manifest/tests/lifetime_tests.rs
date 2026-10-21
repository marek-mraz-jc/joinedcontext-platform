//! T-4838: `spec.lifetime` checked against the space's model (PL-81) and compiled into the one
//! Bloblang step (PL-82), which is run here in the Bento the runner pins over the fixtures of the
//! task: bus (observedAt + PT10M, and one without), alert (validTo, notAfter P30D, and one without
//! validTo, which ends at notAfter: T-5018), a
//! fixed date, an attribute rolling PT5M, and that attribute absent from the record.

use std::process::Command;

use chrono::{DateTime, TimeZone, Utc};
use jc_core::kinds::{Lifetime, Pipeline, PipelineSpec};
use jc_manifest::lifetime::{check, step};
use jc_manifest::model_schema::ModelSchema;
use serde_json::{json, Value};

/// `components/pipeline-runner/images.yaml` of joinedcontext-deployment.
const BENTO: &str = "ghcr.io/warpstreamlabs/bento:1.22.0@sha256:ef674bebae45793c982c350d8f37c2c7661133c1dc65a5517fb977b36dc09841";

fn model() -> ModelSchema {
    let schema = json!({ "$defs": {
        "Bus": { "type": "object", "properties": {
            "id": {"type": "string"}, "type": {"type": "string"},
            "location": {"type": "object"}, "speed": {"type": "number"},
            "routeName": {"type": "string"}
        }},
        "Alert": { "type": "object", "properties": {
            "id": {"type": "string"}, "type": {"type": "string"},
            "validTo": {"type": "string", "format": "date-time"},
            "description": {"type": "string"}, "location": {"type": "object"}
        }},
        "Event": { "type": "object", "properties": {
            "id": {"type": "string"}, "type": {"type": "string"}, "name": {"type": "string"}
        }}
    }});
    ModelSchema::compile(
        "helsinki",
        "1.0.0",
        &schema,
        &["Bus".to_owned(), "Alert".to_owned(), "Event".to_owned()],
        false,
    )
}

fn spec(lifetime: &str) -> PipelineSpec {
    let yaml = format!(
        r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata: {{ name: city, namespace: helsinki }}
spec:
  class: resident
  sources:
    - dataSourceRef: {{ kind: DataSource, name: feed }}
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-city-write
  lifetime:
{lifetime}"#
    );
    let pipeline = Pipeline::from_yaml(&yaml).expect("parses");
    pipeline.validate().expect("valid");
    pipeline.spec
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 20, 18, 0, 0)
        .single()
        .expect("instant")
}

const ALL: &str = "    - type: Bus
      entity: { from: observedAt, plus: PT10M }
      attributes:
        - { names: [location], rolling: PT5M }
    - type: Alert
      entity: { from: validTo, notAfter: P30D }
    - type: Event
      entity: { at: \"2030-06-01T12:00:00+02:00\" }
";

#[test]
fn every_mode_passes_the_model_check() {
    check(&spec(ALL), &model(), now()).expect("passes");
}

#[test]
fn the_model_check_refuses_with_the_entrys_path_and_rule() {
    for (lifetime, path, words) in [
        ("    - type: Tram\n      entity: { rolling: PT5M }\n", "spec.lifetime[0].type", "declares no class `Tram`"),
        ("    - type: Bus\n      attributes:\n        - { names: [location, colour], rolling: PT5M }\n", "spec.lifetime[0].attributes[0].names", "`colour` is not a slot of `Bus`"),
        ("    - type: Alert\n      entity: { from: description }\n", "spec.lifetime[0].entity.from", "not a date-time slot"),
        ("    - type: Alert\n      entity: { from: nowhere }\n", "spec.lifetime[0].entity.from", "not a date-time slot"),
        ("    - type: Event\n      entity: { at: \"2026-10-20T19:59:59+02:00\" }\n", "spec.lifetime[0].entity.at", "already passed"),
        ("    - type: Bus\n      attributes:\n        - { names: [location], at: \"2026-01-01T00:00:00Z\" }\n", "spec.lifetime[0].attributes[0].at", "already passed"),
    ] {
        let reason = check(&spec(lifetime), &model(), now()).expect_err(path).to_string();
        assert!(reason.contains(path) && reason.contains(words), "{lifetime}: {reason}");
    }
}

#[test]
fn no_lifetime_compiles_to_no_step() {
    let mut empty = spec(ALL);
    empty.lifetime.clear();
    assert_eq!(step(&empty).expect("compiles"), None);
}

#[test]
fn the_step_is_the_same_for_the_same_lifetime_in_any_order() {
    let ordered = spec(
        "    - type: Alert\n      entity: { rolling: PT1H }\n    - type: Bus\n      attributes:\n        - { names: [location, speed], rolling: PT5M }\n",
    );
    let mut shuffled = ordered.clone();
    shuffled.lifetime.reverse();
    shuffled.lifetime[0].attributes[0].names.reverse();
    let text = step(&ordered).expect("compiles").expect("a step");
    assert_eq!(
        step(&shuffled).expect("compiles").as_deref(),
        Some(text.as_str())
    );
    assert!(text.find("\"Alert\"") < text.find("\"Bus\""), "{text}");
    assert!(
        text.find("exists(\"location\")") < text.find("exists(\"speed\")"),
        "{text}"
    );
}

#[test]
fn a_name_that_skipped_validation_never_reaches_the_step() {
    let mut forged = spec(
        "    - type: Bus\n      attributes:\n        - { names: [location], rolling: PT5M }\n",
    );
    forged.lifetime[0].attributes[0].names = vec!["a\" }\nroot = deleted()\n#".to_owned()];
    assert!(step(&forged)
        .expect_err("refused")
        .to_string()
        .contains("not a slot name"));
    let mut forged = spec("    - type: Bus\n      entity: { from: observedAt }\n");
    forged.lifetime[0].entity.as_mut().expect("entity").from = Some("x.y\n".to_owned());
    assert!(step(&forged).is_err());
}

/// Runs `mapping` in Bento over each record, one message per record; returns each message's
/// output document and its `jc_problem`.
fn run(mapping: &str, records: &[Value]) -> Vec<(Value, Option<String>)> {
    let lines: Vec<String> = records.iter().map(Value::to_string).collect();
    let config = json!({
        "input": {"generate": {"count": 1, "interval": "", "mapping": format!("root = {}", json!(lines.join("\n")))}},
        "pipeline": {"processors": [
            {"unarchive": {"format": "lines"}},
            {"mapping": "root = content().parse_json()"},
            {"mapping": mapping},
            {"mapping": "root = {\"doc\": this, \"problem\": @jc_problem.or(null)}"}
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
        .map(|line| {
            let row: Value =
                serde_json::from_str(line).unwrap_or_else(|_| panic!("not a row: {line:.300}"));
            (
                row["doc"].clone(),
                row["problem"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn instant(text: &Value) -> i64 {
    let text = text
        .as_str()
        .unwrap_or_else(|| panic!("an instant: {text}"));
    assert!(
        text.ends_with('Z') && text.len() == 20,
        "RFC 3339 UTC to the second: {text}"
    );
    DateTime::parse_from_rfc3339(text)
        .expect("RFC 3339")
        .timestamp()
}

#[test]
fn the_generated_step_writes_each_mode_in_the_runners_bento() {
    let text = step(&spec(ALL)).expect("compiles").expect("a step");
    let before = Utc::now().timestamp();
    let rows = run(
        &text,
        &[
            json!({"id": "urn:ngsi-ld:Bus:1", "type": "Bus",
                "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [24.9, 60.1]}, "observedAt": "2026-10-20T10:00:00.250Z"},
                "speed": {"type": "Property", "value": 7, "observedAt": "2026-10-20T10:01:00Z"}}),
            json!({"id": "urn:ngsi-ld:Bus:2", "type": "Bus", "speed": {"type": "Property", "value": 3, "observedAt": "2026-10-20T11:00:00+03:00"}}),
            json!({"id": "urn:ngsi-ld:Bus:3", "type": "Bus", "routeName": {"type": "Property", "value": "55"}}),
            json!({"id": "urn:ngsi-ld:Alert:1", "type": "Alert",
                "validTo": {"type": "Property", "value": {"@type": "DateTime", "@value": "2026-10-21T00:00:00+02:00"}}}),
            json!({"id": "urn:ngsi-ld:Alert:2", "type": "Alert", "validTo": {"type": "Property", "value": "2099-01-01T00:00:00Z"}}),
            json!({"id": "urn:ngsi-ld:Alert:3", "type": "Alert", "description": {"type": "Property", "value": "no end"}}),
            json!({"id": "urn:ngsi-ld:Event:1", "type": "Event"}),
            json!([{"id": "urn:ngsi-ld:Event:2", "type": "Event"}, {"id": "urn:ngsi-ld:Other:1", "type": "Other"}]),
        ],
    );
    let after = Utc::now().timestamp();
    let by_id = |id: &str| {
        rows.iter()
            .find(|(doc, _)| doc["id"] == id || doc[0]["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {rows:?}"))
            .clone()
    };

    // Bus: the newest observedAt plus ten minutes; its location rolls five minutes from the write.
    let (bus, problem) = by_id("urn:ngsi-ld:Bus:1");
    assert_eq!(bus["expiresAt"], "2026-10-20T10:11:00Z", "{bus}");
    let location = instant(&bus["location"]["expiresAt"]);
    assert!((before + 300..=after + 300).contains(&location), "{bus}");
    assert_eq!(bus["speed"].get("expiresAt"), None, "{bus}");
    assert_eq!(problem, None);
    // The attribute absent from the record: nothing is added, the entity lifetime still holds.
    let (bus, problem) = by_id("urn:ngsi-ld:Bus:2");
    assert_eq!(bus["expiresAt"], "2026-10-20T08:10:00Z", "{bus}");
    assert_eq!(bus.get("location"), None, "{bus}");
    assert_eq!(problem, None);

    // No observedAt and no notAfter: no end, the record kept and counted.
    let (bus, problem) = by_id("urn:ngsi-ld:Bus:3");
    assert_eq!(bus.get("expiresAt"), None, "{bus}");
    assert_eq!(bus.get("_jc_lifetime_from"), None, "{bus}");
    assert_eq!(problem.as_deref(), Some("lifetime-from"));

    // Alert: validTo when it is sooner than thirty days, else thirty days from the write.
    let (alert, _) = by_id("urn:ngsi-ld:Alert:1");
    assert_eq!(alert["expiresAt"], "2026-10-20T22:00:00Z", "{alert}");
    let (alert, _) = by_id("urn:ngsi-ld:Alert:2");
    let capped = instant(&alert["expiresAt"]);
    assert!(
        (before + 2_592_000..=after + 2_592_000).contains(&capped),
        "{alert}"
    );
    // No validTo (T-5018): the record ends at the write plus notAfter, and still counts as a
    // lifetime-from problem so a feed that drops validTo shows in the run's problems.
    let (alert, problem) = by_id("urn:ngsi-ld:Alert:3");
    let fallback = instant(&alert["expiresAt"]);
    assert!(
        (before + 2_592_000..=after + 2_592_000).contains(&fallback),
        "{alert}"
    );
    assert_eq!(alert["description"]["value"], "no end", "{alert}");
    assert_eq!(alert.get("_jc_lifetime_from"), None, "{alert}");
    assert_eq!(problem.as_deref(), Some("lifetime-from"));

    // Event: the fixed date, in UTC; in an array, the other type passes unchanged.
    let (event, problem) = by_id("urn:ngsi-ld:Event:1");
    assert_eq!(event["expiresAt"], "2030-06-01T10:00:00Z", "{event}");
    assert_eq!(problem, None);
    let (batch, _) = by_id("urn:ngsi-ld:Event:2");
    assert_eq!(batch[0]["expiresAt"], "2030-06-01T10:00:00Z", "{batch}");
    assert_eq!(
        batch[1],
        json!({"id": "urn:ngsi-ld:Other:1", "type": "Other"})
    );
}

#[test]
fn an_attribute_lifetime_reaches_every_instance_and_its_from_counts_a_miss() {
    let lifetime: Vec<Lifetime> = spec(
        "    - type: Alert\n      attributes:\n        - { names: [location], from: validTo, plus: PT1H }\n",
    )
    .lifetime;
    let mut with = spec("    - type: Alert\n      entity: { rolling: PT5M }\n");
    with.lifetime = lifetime;
    let text = step(&with).expect("compiles").expect("a step");
    let rows = run(
        &text,
        &[
            json!({"id": "urn:ngsi-ld:Alert:1", "type": "Alert",
                "validTo": {"type": "Property", "value": "2026-10-21T00:00:00Z"},
                "location": [{"type": "GeoProperty", "value": {}, "datasetId": "urn:ngsi-ld:Dataset:a"}, {"type": "GeoProperty", "value": {}}]}),
            json!({"id": "urn:ngsi-ld:Alert:2", "type": "Alert", "location": {"type": "GeoProperty", "value": {}}}),
        ],
    );
    let (first, problem) = rows
        .iter()
        .find(|(d, _)| d["id"] == "urn:ngsi-ld:Alert:1")
        .expect("Alert:1")
        .clone();
    for instance in first["location"].as_array().expect("instances") {
        assert_eq!(instance["expiresAt"], "2026-10-21T01:00:00Z", "{first}");
    }
    assert_eq!(first.get("expiresAt"), None, "{first}");
    assert_eq!(problem, None);
    let (second, problem) = rows
        .iter()
        .find(|(d, _)| d["id"] == "urn:ngsi-ld:Alert:2")
        .expect("Alert:2")
        .clone();
    assert_eq!(
        second["location"],
        json!({"type": "GeoProperty", "value": {}}),
        "{second}"
    );
    assert_eq!(problem.as_deref(), Some("lifetime-from"));
}
