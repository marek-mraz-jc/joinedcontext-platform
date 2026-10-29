//! T-2699: a space has one data model (DM-61). `jcctl model merge` turns several into one and
//! keeps every IRI (DM-62); `jcctl validate` refuses a second model and accepts imports.

use jcctl::commands::validate;
use jcctl::model::merge::{merge, merge_sources, MergeError, Source};
use serde_norway::Value;
use std::path::{Path, PathBuf};

fn temp_repo(test_name: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "jcctl-merge-{test_name}-{}-{now}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp repo");
    dir
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
        .expect("create parent");
    std::fs::write(path, body).expect("write file");
}

const AIR: &str = r#"id: https://hel.fi/models/air
name: air
prefixes:
  linkml: https://w3id.org/linkml/
  sdm: https://smartdatamodels.org/
default_prefix: sdm
default_range: string
imports:
  - linkml:types
classes:
  AirQualityObserved:
    slots: [pm10, name]
slots:
  pm10:
    range: float
  name:
    slot_uri: schema:name
"#;

const KPI: &str = r#"id: https://hel.fi/models/kpi
name: kpi
prefixes:
  linkml: https://w3id.org/linkml/
  jc: https://joinedcontext.com/models/
default_prefix: jc
default_range: decimal
imports:
  - linkml:types
  - ngsi-ld-core
classes:
  KeyPerformanceIndicator:
    slots: [kpiValue, name]
    attributes:
      formula:
        range: string
  Territory:
    class_uri: sdm:Territory
slots:
  kpiValue: {}
  name:
    slot_uri: schema:name
    range: string
enums:
  Window:
    permissible_values:
      day: {}
"#;

fn source(model: &str, text: &str) -> Source {
    Source {
        model: model.into(),
        schema: serde_norway::from_str(text).expect("a LinkML document"),
    }
}

fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |node, step| node.get(*step))
}

fn text<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    at(value, path).and_then(Value::as_str)
}

#[test]
fn merge_keeps_every_iri_and_writes_out_the_ones_a_default_prefix_gave() {
    let merged = merge_sources("helsinki", &[source("air", AIR), source("kpi", KPI)])
        .expect("two models without a clash merge");

    assert_eq!(text(&merged, &["name"]), Some("helsinki"));
    assert_eq!(
        text(&merged, &["id"]),
        Some("https://hel.fi/models/helsinki")
    );
    assert_eq!(text(&merged, &["default_prefix"]), Some("sdm"));
    // The base's own classes keep their implicit IRI under the base's default prefix.
    assert_eq!(
        text(&merged, &["classes", "AirQualityObserved", "class_uri"]),
        None
    );
    // The other model's classes, slots, attributes and enums were `jc:` terms and stay so.
    assert_eq!(
        text(
            &merged,
            &["classes", "KeyPerformanceIndicator", "class_uri"]
        ),
        Some("jc:KeyPerformanceIndicator")
    );
    assert_eq!(
        text(&merged, &["classes", "Territory", "class_uri"]),
        Some("sdm:Territory"),
        "an IRI written out is never rewritten"
    );
    assert_eq!(
        text(&merged, &["slots", "kpiValue", "slot_uri"]),
        Some("jc:kpiValue")
    );
    assert_eq!(
        text(&merged, &["slots", "kpiValue", "range"]),
        Some("decimal")
    );
    assert_eq!(
        text(
            &merged,
            &[
                "classes",
                "KeyPerformanceIndicator",
                "attributes",
                "formula",
                "slot_uri"
            ]
        ),
        Some("jc:formula")
    );
    assert_eq!(
        text(&merged, &["enums", "Window", "enum_uri"]),
        Some("jc:Window")
    );
    // The same slot defined the same way in both is kept once.
    assert_eq!(
        text(&merged, &["slots", "name", "slot_uri"]),
        Some("schema:name")
    );
    assert_eq!(
        text(&merged, &["prefixes", "jc"]),
        Some("https://joinedcontext.com/models/")
    );
    let imports: Vec<&str> = at(&merged, &["imports"])
        .and_then(Value::as_sequence)
        .expect("imports")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(imports, ["linkml:types", "ngsi-ld-core"]);
}

#[test]
fn merge_refuses_a_clash_naming_both_models() {
    let other = AIR
        .replace("name: air", "name: other")
        .replace("range: float", "range: integer");
    let refused = merge_sources("helsinki", &[source("air", AIR), source("other", &other)])
        .expect_err("pm10 means two things");
    assert_eq!(
        refused,
        MergeError::Clash {
            what: "slot",
            name: "pm10".into(),
            first: "air".into(),
            second: "other".into(),
        }
    );
    let message = refused.to_string();
    assert!(
        message.contains("`air`") && message.contains("`other`"),
        "{message}"
    );

    let prefix = KPI.replace(
        "jc: https://joinedcontext.com/models/",
        "jc: https://joinedcontext.com/models/\n  sdm: https://example.org/not-sdm/",
    );
    let refused = merge_sources("helsinki", &[source("air", AIR), source("kpi", &prefix)])
        .expect_err("one prefix, two IRIs");
    assert!(
        matches!(refused, MergeError::Clash { what: "prefix", ref name, .. } if name == "sdm"),
        "{refused}"
    );
}

#[test]
fn merge_refuses_a_default_prefix_nobody_declares_and_an_empty_list() {
    let undeclared = KPI.replace("default_prefix: jc", "default_prefix: nowhere");
    let refused = merge_sources(
        "helsinki",
        &[source("air", AIR), source("kpi", &undeclared)],
    )
    .expect_err("an IRI under an undeclared prefix would not resolve");
    assert!(refused.to_string().contains("nowhere"), "{refused}");
    assert!(merge_sources("helsinki", &[]).is_err());
}

const ORG: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Organization
metadata:
  name: hel
  namespace: org
spec:
  domain: hel.fi
  locales: ["en"]
  defaultLocale: en
"#;

const PROJECT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Project
metadata:
  name: helsinki
  namespace: org
spec:
  organizationRef: hel
"#;

fn space(model: Option<&str>) -> String {
    let named = model
        .map(|m| format!("\n  dataModelRef: {{ kind: DataModel, name: {m} }}"))
        .unwrap_or_default();
    format!(
        "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: city\n  namespace: helsinki\nspec:\n  isSandbox: false{named}\n"
    )
}

fn model_manifest(name: &str, classes: &str) -> String {
    format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: {name}
  namespace: helsinki
spec:
  contextSpaceRef: city
  linkml: ./{name}.linkml.yaml
  version: 1.2.0
  lifecycle: draft
  classes: [{classes}]
  artifacts:
    jsonSchema: ./{name}.v1.schema.json
"#
    )
}

const MODELS: &str = "projects/helsinki/spaces/city/datamodels";

fn repo(test_name: &str, models: &[(&str, &str, &str)], named: Option<&str>) -> PathBuf {
    let dir = temp_repo(test_name);
    write(&dir, "org.yaml", ORG);
    write(&dir, "projects/helsinki/project.yaml", PROJECT);
    write(
        &dir,
        "projects/helsinki/spaces/city/space.yaml",
        &space(named),
    );
    for (name, classes, source) in models {
        write(
            &dir,
            &format!("{MODELS}/{name}.yaml"),
            &model_manifest(name, classes),
        );
        write(&dir, &format!("{MODELS}/{name}.linkml.yaml"), source);
        write(&dir, &format!("{MODELS}/{name}.v1.schema.json"), "{}");
    }
    dir
}

fn messages(findings: &[validate::Finding]) -> Vec<String> {
    findings.iter().map(|f| f.message.clone()).collect()
}

#[test]
fn validate_refuses_a_second_model_in_a_space_and_names_the_merge() {
    let dir = repo(
        "second",
        &[
            ("air", "AirQualityObserved", AIR),
            ("kpi", "KeyPerformanceIndicator, Territory", KPI),
        ],
        Some("air"),
    );
    let report = validate::run(&dir);
    let refused = messages(&report.findings);
    assert!(
        refused
            .iter()
            .any(|m| m.contains("second model of space `city`")
                && m.contains("jcctl model merge")
                && m.contains("--space city")),
        "{refused:#?}"
    );
}

#[test]
fn validate_accepts_one_model_with_imports_and_warns_about_a_space_that_names_none() {
    let dir = repo(
        "one",
        &[("kpi", "KeyPerformanceIndicator, Territory", KPI)],
        Some("kpi"),
    );
    let report = validate::run(&dir);
    assert!(report.is_valid(), "{:#?}", report.findings);
    assert!(
        !messages(&report.warnings)
            .iter()
            .any(|m| m.contains("dataModelRef")),
        "{:#?}",
        report.warnings
    );

    let dir = repo(
        "unnamed",
        &[("kpi", "KeyPerformanceIndicator, Territory", KPI)],
        None,
    );
    let report = validate::run(&dir);
    assert!(report.is_valid(), "{:#?}", report.findings);
    assert!(
        messages(&report.warnings)
            .iter()
            .any(|m| m.contains("names no data model") && m.contains("name: kpi")),
        "{:#?}",
        report.warnings
    );

    let dir = repo("modelless", &[], None);
    let warned = messages(&validate::run(&dir).warnings);
    assert!(
        warned.iter().any(|m| m.contains("has no data model")),
        "{warned:#?}"
    );
}

#[test]
fn validate_refuses_a_reference_to_a_model_the_space_does_not_hold() {
    let dir = repo(
        "dangling",
        &[("kpi", "KeyPerformanceIndicator, Territory", KPI)],
        Some("air"),
    );
    let refused = messages(&validate::run(&dir).findings);
    assert!(
        refused
            .iter()
            .any(|m| m.contains("names DataModel `air`") && m.contains("its model is `kpi`")),
        "{refused:#?}"
    );
}

#[test]
fn validate_refuses_an_import_that_reaches_out_of_the_models_folder() {
    let reaching = KPI.replace(
        "  - ngsi-ld-core",
        "  - ngsi-ld-core\n  - ../../../other/datamodels/fleet\n  - https://hel.fi/api/endpoint/fleet/schema/v2/model.linkml.yaml",
    );
    let dir = repo(
        "reach",
        &[("kpi", "KeyPerformanceIndicator, Territory", &reaching)],
        Some("kpi"),
    );
    let refused = messages(&validate::run(&dir).findings);
    assert_eq!(
        refused
            .iter()
            .filter(|m| m.contains("from outside its folder"))
            .count(),
        1,
        "only the relative climb is refused, a published URL is an import: {refused:#?}"
    );
    assert!(refused
        .iter()
        .any(|m| m.contains("../../../other/datamodels/fleet")));
}

#[test]
fn merge_writes_one_model_removes_the_others_and_the_result_validates() {
    let dir = repo(
        "migrate",
        &[
            ("air", "AirQualityObserved", AIR),
            ("kpi", "KeyPerformanceIndicator, Territory", KPI),
        ],
        None,
    );
    let report = merge(&dir, "helsinki", "city", None).expect("the merge runs");
    assert_eq!(report.model, "city");
    assert_eq!(report.merged, ["air", "kpi"]);
    for gone in [
        "air.yaml",
        "air.linkml.yaml",
        "air.v1.schema.json",
        "kpi.yaml",
        "kpi.linkml.yaml",
    ] {
        assert!(
            !dir.join(MODELS).join(gone).exists(),
            "{gone} is still there"
        );
    }
    let manifest: Value = serde_norway::from_str(
        &std::fs::read_to_string(dir.join(MODELS).join("city.yaml")).expect("merged manifest"),
    )
    .expect("yaml");
    assert_eq!(
        text(&manifest, &["spec", "linkml"]),
        Some("./city.linkml.yaml")
    );
    assert_eq!(text(&manifest, &["spec", "version"]), Some("1.2.0"));
    assert_eq!(
        text(&manifest, &["spec", "artifacts", "jsonSchema"]),
        Some("./city.v1.schema.json")
    );
    let classes: Vec<&str> = at(&manifest, &["spec", "classes"])
        .and_then(Value::as_sequence)
        .expect("classes")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        classes,
        ["AirQualityObserved", "KeyPerformanceIndicator", "Territory"]
    );

    write(
        &dir,
        "projects/helsinki/spaces/city/space.yaml",
        &space(Some("city")),
    );
    let report = validate::run(&dir);
    assert!(
        !messages(&report.findings)
            .iter()
            .any(|m| m.contains("DM-61")),
        "{:#?}",
        report.findings
    );

    let again = merge(&dir, "helsinki", "city", None).expect("one model is left alone");
    assert!(again.written.is_empty() && again.removed.is_empty());
}

#[test]
fn merge_refuses_a_clash_and_touches_nothing() {
    let other = AIR.replace("range: float", "range: integer");
    let dir = repo(
        "clash",
        &[
            ("air", "AirQualityObserved", AIR),
            ("other", "AirQualityObserved", &other),
        ],
        None,
    );
    let refused = merge(&dir, "helsinki", "city", None).expect_err("pm10 clashes");
    assert!(refused.to_string().contains("`pm10`"), "{refused}");
    assert!(dir.join(MODELS).join("air.yaml").exists());
    assert!(dir.join(MODELS).join("other.linkml.yaml").exists());
    assert!(!dir.join(MODELS).join("city.yaml").exists());

    assert!(merge(&dir, "helsinki", "nowhere", None).is_err());
    assert!(merge(&dir, "helsinki", "city", Some("Not A Label")).is_err());
}

#[test]
fn a_model_no_space_owns_is_no_second_model_of_a_space_dm74() {
    let dir = repo("levels", &[("air", "AirQualityObserved", AIR)], Some("air"));
    let spaceless = |namespace: &str| {
        model_manifest("shared", "KeyPerformanceIndicator")
            .replace("namespace: helsinki", &format!("namespace: {namespace}"))
            .replace("  contextSpaceRef: city\n", "")
    };
    // An organization model and a project model beside the space's one model.
    write(&dir, "datamodels/shared/shared.yaml", &spaceless("org"));
    write(&dir, "datamodels/shared/shared.linkml.yaml", KPI);
    write(
        &dir,
        "projects/helsinki/datamodels/shared/shared.yaml",
        &spaceless("helsinki"),
    );
    write(
        &dir,
        "projects/helsinki/datamodels/shared/shared.linkml.yaml",
        KPI,
    );
    let report = validate::run(&dir);
    assert!(report.is_valid(), "{:#?}", report.findings);

    // An organization model that claims a space is refused, naming the field.
    write(
        &dir,
        "datamodels/shared/shared.yaml",
        &spaceless("org").replace("spec:\n", "spec:\n  contextSpaceRef: city\n"),
    );
    let refused = messages(&validate::run(&dir).findings);
    assert!(
        refused.iter().any(|m| m.contains("contextSpaceRef")),
        "{refused:#?}"
    );
}
