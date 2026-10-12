//! A Pipeline's routes by condition (PL-73): parsed, checked and counted among its outputs.

use jc_core::kinds::Pipeline;

const ROUTED: &str = r#"apiVersion: joinedcontext.com/v1alpha2
kind: Pipeline
metadata:
  name: air-routed
  namespace: helsinki
spec:
  class: resident
  period: 30s
  sources:
    - dataSourceRef: { kind: DataSource, name: air-sensors }
  steps:
    - kind: bloblang
      bloblang: "root = this"
  routes:
    - name: hot
      when: this.temperature.value > 30
      steps:
        - kind: bloblang
          bloblang: "root = this"
      outputs:
        - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki-alerts:ep-alerts-write
    - name: cold
      when: this.temperature.value < 0
      outputs:
        - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki-alerts:ep-frost-write
  outputs:
    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-air-write
"#;

fn refusal(yaml: &str) -> String {
    match Pipeline::from_yaml(yaml) {
        Err(error) => error.to_string(),
        Ok(pipeline) => pipeline.validate().expect_err("refused").to_string(),
    }
}

fn refused_with(yaml: &str, words: &str) {
    let message = refusal(yaml);
    assert!(message.contains(words), "expected {words:?} in {message}");
}

#[test]
fn a_routed_pipeline_counts_every_route_output_and_keeps_the_lane_apart() {
    let pipeline = Pipeline::from_yaml(ROUTED).expect("parses");
    pipeline.validate().expect("valid");
    let ids = |outputs: Vec<jc_core::kinds::PipelineOutput>| {
        outputs
            .into_iter()
            .map(|output| output.target_endpoint.local_id().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(pipeline.spec.outputs()),
        ["ep-air-write", "ep-alerts-write", "ep-frost-write"]
    );
    assert_eq!(ids(pipeline.spec.lane_outputs()), ["ep-air-write"]);
    assert_eq!(pipeline.spec.routes[0].steps.len(), 1);
    assert!(pipeline.spec.routes[1].steps.is_empty());
}

#[test]
fn routes_need_the_second_version() {
    refused_with(
        &ROUTED.replace("v1alpha2", "v1alpha1"),
        "joinedcontext.com/v1alpha2",
    );
}

#[test]
fn two_routes_with_one_name_are_refused() {
    refused_with(&ROUTED.replace("name: cold", "name: hot"), "share a name");
}

#[test]
fn a_route_name_is_a_dns_label() {
    refused_with(
        &ROUTED.replace("name: cold", "name: Cold_Route"),
        "Cold_Route",
    );
}

#[test]
fn a_route_without_a_condition_or_an_output_is_refused() {
    refused_with(
        &ROUTED.replace("when: this.temperature.value < 0", "when: \" \""),
        "condition",
    );
    refused_with(
        &ROUTED.replace(
            "      outputs:\n        - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki-alerts:ep-frost-write\n",
            "      outputs: []\n",
        ),
        "at least one output",
    );
}

#[test]
fn a_condition_reads_the_message_and_never_the_runner() {
    refused_with(
        &ROUTED.replace(
            "this.temperature.value < 0",
            "env(\"JC_CLIENT_SECRET\") != \"\"",
        ),
        "env(\"JC_CLIENT_SECRET\")",
    );
    refused_with(
        &ROUTED.replace(
            "this.temperature.value < 0",
            "file(\"/etc/passwd\") != \"\"",
        ),
        "file(",
    );
}

#[test]
fn nine_routes_are_refused() {
    let route = |n: usize| {
        format!(
            "    - name: r{n}\n      when: this.n == {n}\n      outputs:\n        - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-r{n}\n"
        )
    };
    let routes = |count: usize| (0..count).map(route).collect::<String>();
    let start = ROUTED.find("  routes:\n").expect("routes") + "  routes:\n".len();
    let end = ROUTED
        .find("  outputs:\n    - targetEndpoint: urn:ngsi-ld:Endpoint:hel.fi:helsinki:ep-air-write")
        .expect("lane");
    let with = |count: usize| format!("{}{}{}", &ROUTED[..start], routes(count), &ROUTED[end..]);
    Pipeline::from_yaml(&with(8))
        .expect("parses")
        .validate()
        .expect("eight routes");
    refused_with(&with(9), "at most 8 routes");
}

#[test]
fn a_route_routes_no_further_and_runs_no_container() {
    let steps = "      steps:\n        - kind: bloblang\n          bloblang: \"root = this\"\n";
    refused_with(
        &ROUTED.replace(
            steps,
            "      steps:\n        - processor:\n            switch: [{ check: 'this.a > 1', processors: [] }]\n",
        ),
        "route no further",
    );
    refused_with(
        &ROUTED.replace(steps, "      steps:\n        - kind: container\n"),
        "route no further",
    );
}

#[test]
fn a_route_output_is_held_to_an_endpoint() {
    refused_with(
        &ROUTED.replace(
            "urn:ngsi-ld:Endpoint:hel.fi:helsinki-alerts:ep-frost-write",
            "urn:ngsi-ld:Policy:hel.fi:helsinki-alerts:not-an-endpoint",
        ),
        "Endpoint",
    );
}

#[test]
fn an_unknown_route_field_is_refused() {
    refused_with(
        &ROUTED.replace(
            "    - name: cold\n",
            "    - name: cold\n      continue: true\n",
        ),
        "continue",
    );
}
