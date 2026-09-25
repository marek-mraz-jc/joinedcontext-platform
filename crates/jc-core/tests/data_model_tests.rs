//! T-0117: `kind: DataModel` (DM-01, DM-02, DM-03, DM-08, DM-22, DM-26, DM-48).

use chrono::{DateTime, Utc};
use jc_core::error::Error;
use jc_core::kinds::data_model::{
    DataModelLifecycle, DataModelOrigin, DataModelSource, RemoteSource, SemVer,
};
use jc_core::kinds::DataModel;

/// Verbatim from docs/Architecture/11-data-models.md section 4.1.
const GOLDEN: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: bb-air-quality
  namespace: bb-ovzdusie
  title: { sk: "Kvalita ovzdušia", en: "Air quality" }
spec:
  contextSpaceRef: ovzdusie
  linkml: ./bb-air-quality.linkml.yaml
  version: 2.1.0
  lifecycle: published
  classes: [AirQualityObserved]
  source:
    repository: https://github.com/smart-data-models/dataModel.Environment
    path: AirQualityObserved
    commit: 9f1c2b7d4e6a8c0b2d4f6a8c0e2b4d6f8a0c2e4b
  artifacts:
    jsonSchema: ./json-schema/bb-air-quality.v2.json
    context: ./context/bb-air-quality.v2.jsonld
    docs: ./docs/bb-air-quality.md
    example: ./examples/bb-air-quality.example.jsonld
"#;

fn at(s: &str) -> DateTime<Utc> {
    s.parse().expect("fixed timestamp")
}

#[test]
fn golden_data_model_parses_validates_and_roundtrips() {
    let dm = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");
    dm.validate().expect("golden DataModel validates");

    assert_eq!(
        dm.resource_path().expect("resource path"),
        "projects/bb-ovzdusie/spaces/ovzdusie/datamodels/bb-air-quality.yaml"
    );
    assert_eq!(dm.spec.version.major(), 2);
    assert_eq!(dm.spec.classes, vec!["AirQualityObserved".to_string()]);
    assert!(dm.spec.can_be_referenced());

    let serialized = dm.to_yaml().expect("serialize");
    assert_eq!(dm, DataModel::from_yaml(&serialized).expect("re-import"));
}

#[test]
fn semver_table() {
    for (raw, major, minor, patch) in [
        ("0.0.0", 0u32, 0u32, 0u32),
        ("1.0.0", 1, 0, 0),
        ("2.1.0", 2, 1, 0),
        ("10.20.30", 10, 20, 30),
    ] {
        let v = SemVer::new(raw).unwrap_or_else(|e| panic!("`{raw}` must parse: {e}"));
        assert_eq!((v.major(), v.minor(), v.patch()), (major, minor, patch));
        assert_eq!(v.as_str(), raw);
        assert_eq!(v.to_string(), raw);
    }

    for raw in [
        "",
        "1",
        "1.0",
        "1.0.0.0",
        "01.0.0",
        "1.02.0",
        "1.0.03",
        "v1.0.0",
        "1.0.0-alpha",
        "1.0.0+build",
        "a.b.c",
        " 1.0.0",
        "1.0.0 ",
    ] {
        assert!(SemVer::new(raw).is_err(), "`{raw}` must be rejected");
    }
}

#[test]
fn linkml_source_must_be_a_relative_linkml_yaml_file_dm01() {
    for bad in [
        "datamodels/bb-air-quality.yaml",
        "/datamodels/bb-air-quality.linkml.yaml",
        "../bb-air-quality.linkml.yaml",
        "a/../b.linkml.yaml",
        "",
    ] {
        let mut dm = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");
        dm.spec.linkml = bad.to_string();
        let err = dm
            .validate()
            .expect_err(&format!("`{bad}` must be refused"));
        assert!(
            matches!(
                err,
                Error::Name {
                    field: "linkml",
                    ..
                }
            ),
            "`{bad}` blamed the wrong field"
        );
    }

    let mut dm = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");
    dm.spec.linkml = "nested/dir/model.linkml.yaml".to_string();
    assert!(dm.validate().is_ok());
}

#[test]
fn published_requires_all_four_artifacts_dm02() {
    let base = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");

    for field in ["jsonSchema", "context", "docs", "example"] {
        let mut dm = base.clone();
        match field {
            "jsonSchema" => dm.spec.artifacts.json_schema = None,
            "context" => dm.spec.artifacts.context = None,
            "docs" => dm.spec.artifacts.docs = None,
            _ => dm.spec.artifacts.example = None,
        }
        let err = dm
            .validate()
            .expect_err(&format!("published without {field} must fail"));
        match err {
            Error::Name { field: f, .. } => assert_eq!(f, format!("artifacts.{field}")),
            other => panic!("expected Error::Name, got {other:?}"),
        }
    }

    // A draft may still be missing its generated artifacts.
    let mut draft = base;
    draft.spec.lifecycle = DataModelLifecycle::Draft;
    draft.spec.artifacts.docs = None;
    draft.spec.artifacts.example = None;
    assert!(draft.validate().is_ok());
    assert!(!draft.spec.can_be_referenced());
}

#[test]
fn artifact_paths_must_stay_inside_the_space_directory() {
    let mut dm = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");
    dm.spec.artifacts.context = Some("../../other-space/context.jsonld".to_string());
    let err = dm.validate().expect_err("escaping artifact path must fail");
    assert!(matches!(
        err,
        Error::Name {
            field: "artifacts.context",
            ..
        }
    ));
}

#[test]
fn imported_source_needs_the_whole_triple_dm08() {
    let base = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");

    let mut partial = base.clone();
    partial.spec.source = Some(DataModelSource {
        repository: Some("https://github.com/smart-data-models/x".to_string()),
        path: None,
        commit: None,
        remote: None,
    });
    assert!(
        partial.validate().is_err(),
        "repository alone is not provenance"
    );

    let mut http = base.clone();
    if let Some(src) = http.spec.source.as_mut() {
        src.repository = Some("http://github.com/smart-data-models/x".to_string());
    }
    let err = http.validate().expect_err("plaintext http must be refused");
    assert!(matches!(
        err,
        Error::Name {
            field: "source.repository",
            ..
        }
    ));

    let mut bad_commit = base.clone();
    if let Some(src) = bad_commit.spec.source.as_mut() {
        src.commit = Some("NOTAHEXSHA".to_string());
    }
    assert!(matches!(
        bad_commit.validate().expect_err("bad commit"),
        Error::Name {
            field: "source.commit",
            ..
        }
    ));

    // A hand-authored model simply omits `source` (DM-08).
    let mut authored = base;
    authored.spec.source = None;
    assert!(authored.validate().is_ok());
}

#[test]
fn mirrored_and_remote_imply_each_other_dm48() {
    let base = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");
    let remote = DataModelSource {
        repository: None,
        path: None,
        commit: None,
        remote: Some(RemoteSource {
            url: "https://portal.bratislava.sk/cs/ovzdusie/schema/v1".to_string(),
            version: SemVer::new("1.0.0").expect("valid semver"),
            sha256: "a".repeat(64),
            fetched_at: at("2026-09-05T10:00:00Z"),
        }),
    };

    // remote source but a non-mirrored lifecycle
    let mut wrong_lifecycle = base.clone();
    wrong_lifecycle.spec.source = Some(remote.clone());
    assert!(matches!(
        wrong_lifecycle.validate().expect_err("published + remote"),
        Error::Name {
            field: "source.remote",
            ..
        }
    ));

    // mirrored lifecycle but an imported source
    let mut wrong_source = base.clone();
    wrong_source.spec.lifecycle = DataModelLifecycle::Mirrored;
    assert!(wrong_source.validate().is_err());

    // both together validate
    let mut mirrored = base.clone();
    mirrored.spec.lifecycle = DataModelLifecycle::Mirrored;
    mirrored.spec.source = Some(remote.clone());
    mirrored.validate().expect("mirrored + remote validates");
    assert!(
        !mirrored.spec.can_be_referenced(),
        "a mirror is not referenceable (DM-26)"
    );

    // a short or upper-case digest is not a sha256
    let mut bad_digest = mirrored.clone();
    if let Some(src) = bad_digest.spec.source.as_mut() {
        if let Some(r) = src.remote.as_mut() {
            r.sha256 = "A".repeat(64);
        }
    }
    assert!(matches!(
        bad_digest.validate().expect_err("upper-case digest"),
        Error::Name {
            field: "source.remote.sha256",
            ..
        }
    ));

    let mut http_remote = mirrored;
    if let Some(src) = http_remote.spec.source.as_mut() {
        if let Some(r) = src.remote.as_mut() {
            r.url = "http://portal.bratislava.sk/schema".to_string();
        }
    }
    assert!(http_remote.validate().is_err());
}

#[test]
fn lifecycle_transitions_dm26() {
    use DataModelLifecycle::{Deprecated, Draft, Mirrored, Published, Retired};

    let allowed = [
        (Draft, Published),
        (Published, Deprecated),
        (Deprecated, Retired),
        (Draft, Draft),
        (Published, Published),
        (Deprecated, Deprecated),
        (Retired, Retired),
        (Mirrored, Mirrored),
    ];
    for (from, to) in allowed {
        assert!(
            from.allows_transition_to(to),
            "{from:?} -> {to:?} must be allowed"
        );
    }

    let forbidden = [
        (Published, Draft),
        (Deprecated, Published),
        (Retired, Deprecated),
        (Retired, Published),
        (Retired, Draft),
        (Draft, Deprecated),
        (Draft, Retired),
        (Published, Retired),
        (Draft, Mirrored),
        (Mirrored, Published),
    ];
    for (from, to) in forbidden {
        assert!(
            !from.allows_transition_to(to),
            "{from:?} -> {to:?} must be refused"
        );
    }
}

#[test]
fn unknown_fields_and_bad_class_names_are_rejected() {
    let with_secret = format!("{GOLDEN}  secretRef:\n    name: model-credentials\n");
    assert!(DataModel::from_yaml(&with_secret).is_err());

    let legacy_field = GOLDEN.replace("  linkml: ", "  linkmlPath: ");
    assert!(
        DataModel::from_yaml(&legacy_field).is_err(),
        "deny_unknown_fields must reject a renamed field"
    );

    let mut dm = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");
    dm.spec.classes = vec!["air quality".to_string()];
    assert!(matches!(
        dm.validate().expect_err("bad NGSI-LD type"),
        Error::Name {
            field: "classes",
            ..
        }
    ));
}

/// An organization model: namespace `org`, no space (DM-74, ADR-N-039).
const ORGANIZATION_MODEL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataModel
metadata:
  name: air-quality
  namespace: org
spec:
  linkml: ./air-quality.linkml.yaml
  version: 1.2.0
  lifecycle: published
  classes: [AirQualityObserved]
  origin:
    project: bb-ovzdusie
    space: ovzdusie
    name: bb-air-quality
    version: 1.2.0
    commit: 9f1c2b7
  artifacts:
    jsonSchema: ./json-schema/air-quality.v1.json
    context: ./context/air-quality.v1.jsonld
    docs: ./docs/air-quality.md
    example: ./examples/air-quality.example.jsonld
"#;

#[test]
fn a_model_lives_in_the_organization_or_in_a_project_dm74() {
    let organization = DataModel::from_yaml(ORGANIZATION_MODEL).expect("valid YAML");
    organization
        .validate()
        .expect("an organization model validates");
    assert_eq!(
        organization.resource_path().expect("path"),
        "datamodels/air-quality/air-quality.yaml"
    );
    assert_eq!(
        organization.spec.origin.as_ref().map(|o| o.version.major()),
        Some(1)
    );

    // A project model no space owns sits in the project's own datamodels folder.
    let mut project = organization.clone();
    project.metadata.namespace = Some("bb-ovzdusie".into());
    project.spec.origin = None;
    project
        .validate()
        .expect("a project model without a space validates");
    assert_eq!(
        project.resource_path().expect("path"),
        "projects/bb-ovzdusie/datamodels/air-quality/air-quality.yaml"
    );

    // A space's model keeps its path (DM-61).
    let space = DataModel::from_yaml(GOLDEN).expect("valid golden YAML");
    assert_eq!(
        space.resource_path().expect("path"),
        "projects/bb-ovzdusie/spaces/ovzdusie/datamodels/bb-air-quality.yaml"
    );

    let info = jc_core::registry::by_kind("DataModel").expect("catalogued");
    assert_eq!(info.repo_path("org", "", "m"), "datamodels/m/m.yaml");
    assert_eq!(
        info.repo_path("p", "", "m"),
        "projects/p/datamodels/m/m.yaml"
    );
    assert_eq!(
        info.repo_path("p", "s", "m"),
        "projects/p/spaces/s/datamodels/m.yaml"
    );
}

#[test]
fn an_organization_model_belongs_to_no_space_and_needs_its_namespace_dm74() {
    let mut in_a_space = DataModel::from_yaml(ORGANIZATION_MODEL).expect("valid YAML");
    in_a_space.spec.context_space_ref = Some("ovzdusie".into());
    let err = in_a_space
        .validate()
        .expect_err("an organization model in a space");
    assert!(
        matches!(
            err,
            Error::Name {
                field: "contextSpaceRef",
                ..
            }
        ),
        "{err}"
    );

    let mut no_namespace = DataModel::from_yaml(ORGANIZATION_MODEL).expect("valid YAML");
    no_namespace.metadata.namespace = None;
    let err = no_namespace
        .validate()
        .expect_err("the organization is namespace `org`");
    assert!(
        matches!(
            err,
            Error::Name {
                field: "metadata.namespace",
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn origin_names_a_project_model_at_a_commit_dm76() {
    let base = DataModel::from_yaml(ORGANIZATION_MODEL).expect("valid YAML");
    type Break = fn(&mut DataModelOrigin);
    let cases: [(&str, Break); 4] = [
        ("origin.project", |o| o.project = "org".into()),
        ("origin.space", |o| o.space = Some("Bad Space".into())),
        ("origin.name", |o| o.name = "Not_A_Label".into()),
        ("origin.commit", |o| o.commit = "HEAD".into()),
    ];
    for (field, break_it) in cases {
        let mut model = base.clone();
        break_it(model.spec.origin.as_mut().expect("origin"));
        let err = model.validate().expect_err(field);
        assert!(
            matches!(err, Error::Name { field: f, .. } if f == field),
            "{field}: {err}"
        );
    }
    let mut unknown = serde_json::to_value(&base).expect("json");
    unknown["spec"]["origin"]["url"] = "https://example.org".into();
    assert!(serde_json::from_value::<DataModel>(unknown).is_err());
}

#[test]
fn a_model_import_names_an_organization_or_own_project_model_at_a_major_dm75() {
    use jc_core::kinds::data_model::ModelImport;
    let parsed = |entry: &str| ModelImport::parse(entry).map(|r| r.map(|i| i.to_string()).ok());
    assert_eq!(
        parsed("org.air-quality.v1"),
        Some(Some("org.air-quality.v1".into()))
    );
    assert_eq!(
        parsed("project.bikes.v0"),
        Some(Some("project.bikes.v0".into()))
    );
    // Not a platform model: LinkML's own, the shipped core, a URL.
    for other in [
        "linkml:types",
        "ngsi-ld-core",
        "https://w3id.org/x",
        "./local",
    ] {
        assert_eq!(parsed(other), None, "{other}");
    }
    // Starts like one and is malformed: refused, never read as a local file.
    for bad in [
        "org.air-quality",
        "org.air-quality.1",
        "org.air-quality.v01",
        "org.air-quality.v",
        "org.Air_Quality.v1",
        "project./x.v1",
        "org.a.v1.extra",
    ] {
        assert_eq!(parsed(bad), Some(None), "{bad}");
    }
    let source = serde_json::json!({
        "imports": ["linkml:types", "ngsi-ld-core", "org.air-quality.v2", "project.bikes.v1"]
    });
    let all = ModelImport::all_in(&source).expect("well formed");
    assert_eq!(
        all.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["org.air-quality.v2", "project.bikes.v1"]
    );
    assert!(ModelImport::all_in(&serde_json::json!({ "imports": ["org.x"] })).is_err());
    assert!(ModelImport::all_in(&serde_json::json!({}))
        .expect("none")
        .is_empty());
}
