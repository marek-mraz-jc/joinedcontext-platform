//! T-0118: `kind: Mapping` (DM-33, DM-34, DM-35, DM-38, DM-39, DM-42).

use jc_core::envelope::TypedRef;
use jc_core::error::Error;
use jc_core::kinds::mapping::{
    DataModelRef, MappingTest, NativeBlock, NativeLanguage, VocabularyAlignment,
};
use jc_core::kinds::Mapping;

/// Verbatim from docs/Architecture/11-data-models.md section 7.3.
const GOLDEN: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Mapping
metadata:
  name: sdm-airquality-to-bb
  namespace: bb-ovzdusie
  title: { sk: "SDM AirQualityObserved → BB kvalita ovzdušia", en: "SDM AirQualityObserved → BB air quality" }
spec:
  contextSpaceRef: ovzdusie
  source: { kind: DataModel, name: sdm-airqualityobserved, version: "1" }
  target: { kind: DataModel, name: bb-air-quality, version: "2" }
  transformation:
    id: https://bb.example.sk/mappings/sdm-airquality-to-bb
    class_derivations:
      BBAirQuality:
        populated_from: AirQualityObserved
        slot_derivations:
          id:          { populated_from: id }
          pm10:        { populated_from: pm10 }
          pm25:        { populated_from: pm2p5 }
          temperatureC:
            populated_from: temperature
            unit_conversion: { target_unit: Cel }
          observedAt:  { populated_from: dateObserved }
          label:
            expr: "{stationName} + ' (' + {areaServed} + ')'"
          qualityBand:
            populated_from: airQualityLevel
            value_mappings: { good: A, moderate: B, unhealthy: C }
  vocabularyAlignment: { sssomRef: { kind: Mapping, name: sdm-to-bb-terms } }
  tests:
    - input:  examples/sdm-example.jsonld
      expect: examples/bb-example.jsonld
"#;

#[test]
fn golden_mapping_parses_validates_and_roundtrips() {
    let mapping = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    mapping.validate().expect("golden Mapping validates");

    assert_eq!(
        mapping.resource_path().expect("resource path"),
        "projects/bb-ovzdusie/spaces/ovzdusie/datamodels/mappings/sdm-airquality-to-bb.yaml"
    );
    assert_eq!(mapping.spec.source.version, "1");
    assert_eq!(mapping.spec.target.version, "2");
    assert!(!mapping.spec.requires_elevated_lane());
    assert_eq!(
        mapping.spec.compiled_bloblang_path("sdm-airquality-to-bb"),
        "generated/sdm-airquality-to-bb.blobl"
    );

    let serialized = mapping.to_yaml().expect("serialize");
    assert_eq!(mapping, Mapping::from_yaml(&serialized).expect("re-import"));
}

#[test]
fn transformation_is_kept_verbatim_dm33() {
    let mapping = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    let derivations = mapping.spec.transformation["class_derivations"]
        .as_object()
        .expect("class_derivations survives as an object");
    assert!(derivations.contains_key("BBAirQuality"));

    // The LinkML-Map grammar is not modelled in Rust: every construct passes through untouched.
    let slots = mapping.spec.transformation["class_derivations"]["BBAirQuality"]
        ["slot_derivations"]
        .as_object()
        .expect("slot_derivations survives");
    assert_eq!(
        slots["temperatureC"]["unit_conversion"]["target_unit"],
        serde_json::json!("Cel")
    );
    assert_eq!(
        slots["qualityBand"]["value_mappings"]["good"],
        serde_json::json!("A")
    );
    assert!(slots["label"]["expr"].is_string());
}

#[test]
fn transformation_must_carry_class_derivations_dm33() {
    let mut mapping = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");

    mapping.spec.transformation = serde_json::json!("not an object");
    assert!(matches!(
        mapping.validate().expect_err("scalar transformation"),
        Error::Name {
            field: "spec.transformation",
            ..
        }
    ));

    mapping.spec.transformation = serde_json::json!({ "id": "https://example.sk/m" });
    assert!(
        mapping.validate().is_err(),
        "no class_derivations must fail"
    );

    mapping.spec.transformation = serde_json::json!({ "class_derivations": {} });
    assert!(
        mapping.validate().is_err(),
        "empty class_derivations must fail"
    );
}

#[test]
fn at_least_one_golden_test_is_required_dm39() {
    let mut mapping = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    mapping.spec.tests.clear();
    assert!(matches!(
        mapping
            .validate()
            .expect_err("a mapping without a golden test"),
        Error::Name {
            field: "spec.tests",
            ..
        }
    ));

    let mut duplicate = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    let first = duplicate.spec.tests[0].clone();
    duplicate.spec.tests.push(first);
    assert!(
        duplicate.validate().is_err(),
        "duplicate golden test must fail"
    );

    let mut escaping = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    escaping.spec.tests = vec![MappingTest::new("../../etc/passwd", "examples/out.jsonld")];
    assert!(matches!(
        escaping.validate().expect_err("escaping example path"),
        Error::Name {
            field: "spec.tests.input",
            ..
        }
    ));

    let mut absolute = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    absolute.spec.tests = vec![MappingTest::new("examples/in.jsonld", "/tmp/out.jsonld")];
    assert!(matches!(
        absolute.validate().expect_err("absolute expect path"),
        Error::Name {
            field: "spec.tests.expect",
            ..
        }
    ));
}

#[test]
fn native_blocks_raise_the_lane_dm38() {
    let mut mapping = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    assert!(!mapping.spec.requires_elevated_lane());

    mapping.spec.native = vec![NativeBlock::new(
        "qualityBand",
        NativeLanguage::Bloblang,
        r#"root = if this.pm10 > 50 { "C" } else { "A" }"#,
    )];
    mapping
        .validate()
        .expect("a native block is legal, just louder");
    assert!(mapping.spec.requires_elevated_lane());

    // Bloblang is the only runtime language (DM-37).
    let with_js = GOLDEN.replace(
        "  tests:",
        "  native:\n    - targetSlot: qualityBand\n      language: javascript\n      source: \"x\"\n  tests:",
    );
    assert!(Mapping::from_yaml(&with_js).is_err());

    // one block per target slot
    let mut duplicate = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    duplicate.spec.native = vec![
        NativeBlock::new("qualityBand", NativeLanguage::Bloblang, "root = \"A\""),
        NativeBlock::new("qualityBand", NativeLanguage::Bloblang, "root = \"B\""),
    ];
    assert!(matches!(
        duplicate.validate().expect_err("two blocks on one slot"),
        Error::Name {
            field: "spec.native.targetSlot",
            ..
        }
    ));

    let mut empty_source = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    empty_source.spec.native = vec![NativeBlock::new(
        "qualityBand",
        NativeLanguage::Bloblang,
        "",
    )];
    assert!(empty_source.validate().is_err());
}

#[test]
fn model_references_are_validated_dm33() {
    let mut same = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    same.spec.source = same.spec.target.clone();
    assert!(matches!(
        same.validate().expect_err("a mapping onto itself"),
        Error::Name {
            field: "spec.target",
            ..
        }
    ));

    // Same model, different major version, is a migration mapping and is legal (DM-23, DM-41).
    let mut migration = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    migration.spec.source = DataModelRef::new("bb-air-quality", "1");
    assert!(migration.validate().is_ok());

    for bad_version in ["", "2.1.0", "v2", "01", "latest"] {
        let mut bad = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
        bad.spec.source = DataModelRef::new("sdm-airqualityobserved", bad_version);
        assert!(
            bad.validate().is_err(),
            "version `{bad_version}` must be refused"
        );
    }

    let wrong_kind = GOLDEN.replace(
        "{ kind: DataModel, name: sdm-airqualityobserved",
        "{ kind: Endpoint, name: sdm-airqualityobserved",
    );
    let mapping = Mapping::from_yaml(&wrong_kind).expect("parses");
    assert!(matches!(
        mapping
            .validate()
            .expect_err("a Mapping does not map an Endpoint"),
        Error::Kind {
            expected: "DataModel",
            ..
        }
    ));
}

#[test]
fn vocabulary_alignment_points_at_an_sssom_mapping_dm42() {
    let mapping = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    let alignment = mapping
        .spec
        .vocabulary_alignment
        .as_ref()
        .expect("golden carries an alignment");
    assert_eq!(alignment.sssom_ref.name, "sdm-to-bb-terms");

    let mut wrong = mapping;
    wrong.spec.vocabulary_alignment = Some(VocabularyAlignment {
        sssom_ref: TypedRef {
            kind: "DataModel".to_string(),
            name: "sdm-to-bb-terms".to_string(),
            namespace: None,
        },
    });
    assert!(matches!(
        wrong.validate().expect_err("an alignment set is a Mapping"),
        Error::Name {
            field: "spec.vocabularyAlignment.sssomRef.kind",
            ..
        }
    ));
}

#[test]
fn context_space_and_unknown_fields() {
    let mut bad_space = Mapping::from_yaml(GOLDEN).expect("valid golden YAML");
    bad_space.spec.context_space_ref = "Invalid_Space_Name".to_string();
    assert!(matches!(
        bad_space.validate().expect_err("invalid space name"),
        Error::Name {
            field: "spec.contextSpaceRef",
            ..
        }
    ));

    let with_secret = format!("{GOLDEN}  secretRef:\n    name: mapping-credentials\n");
    assert!(Mapping::from_yaml(&with_secret).is_err());
}
