//! T-0119: `kind: App` (AP-01, AP-02, AP-04, AP-05, AP-09, AP-11, AP-12, AP-16, AP-17, AP-18).

use jc_core::envelope::Ref;
use jc_core::error::Error;
use jc_core::kinds::app::{
    AppClass, AppLifecycle, AppLimits, AppVisibility, ContentSecurityPolicy, GeoConstraint,
    GeoWithin, TemporalConstraint,
};
use jc_core::kinds::endpoint::Representation;
use jc_core::kinds::policy::{Operation, OperationGroup, OperationRef};
use jc_core::kinds::App;

/// Verbatim from docs/Architecture/16-apps-on-demand.md section 2.
const GOLDEN: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata:
  name: air-quality-today
  namespace: bb-ovzdusie
  title: { sk: "Kvalita ovzdušia dnes", en: "Air quality today" }
  annotations:
    joinedcontext.com/generated-by: "agent:app-builder@bb"
    joinedcontext.com/prompt-digest: "sha256:…"
spec:
  kind: fullstack
  source: { path: ./src }
  build: { rust: "1.90", node: "22" }
  visibility: public
  dataNeeds:
    - contextSpaceRef: { kind: ContextSpace, name: ovzdusie }
      types: [AirQualityObserved, District]
      attrs: [pm10, pm25, airQualityIndex, location, name, refDistrict]
      operations: [queryEntity, retrieveEntity, queryTemporal]
      temporalQ: { window: P1D }
      geoQ: { within: { scopeRef: /geo/SK/BB } }
      representations: [ngsi-ld, geojson]
  limits: { requestsPerMinute: 600, maxFileRows: 20000 }
  csp: { connectSrc: [self], frameAncestors: [none] }
"#;

#[test]
fn golden_app_parses_validates_and_roundtrips() {
    let app = App::from_yaml(GOLDEN).expect("valid golden YAML");
    app.validate().expect("golden App validates");

    assert_eq!(
        app.resource_path().expect("resource path"),
        "projects/bb-ovzdusie/apps/air-quality-today/app.yaml"
    );
    assert_eq!(app.spec.class, AppClass::Fullstack);
    assert_eq!(app.spec.visibility, AppVisibility::Public);
    assert_eq!(
        app.spec.lifecycle,
        AppLifecycle::Draft,
        "a manifest without one is a draft"
    );
    assert_eq!(app.spec.build.0["rust"], "1.90");
    assert_eq!(
        app.spec.representations(),
        [Representation::NgsiLd, Representation::GeoJson]
            .into_iter()
            .collect()
    );

    let serialized = app.to_yaml().expect("serialize");
    assert_eq!(app, App::from_yaml(&serialized).expect("re-import"));
}

#[test]
fn secret_ref_is_rejected_at_parse_time_ap16() {
    let under_spec = format!("{GOLDEN}  secretRef:\n    name: database-credentials\n");
    assert!(
        App::from_yaml(&under_spec).is_err(),
        "deny_unknown_fields must reject secretRef under spec (AP-16)"
    );

    let under_source = GOLDEN.replace(
        "  source: { path: ./src }",
        "  source: { path: ./src, secretRef: { name: git-credentials } }",
    );
    assert!(
        App::from_yaml(&under_source).is_err(),
        "deny_unknown_fields must reject secretRef under spec.source (AP-16)"
    );

    let under_need = GOLDEN.replace(
        "      representations: [ngsi-ld, geojson]",
        "      representations: [ngsi-ld, geojson]\n      apiKey: \"inline\"",
    );
    assert!(App::from_yaml(&under_need).is_err());
}

#[test]
fn source_is_a_path_or_a_forge_repository_ap02() {
    let both = GOLDEN.replace(
        "  source: { path: ./src }",
        "  source: { path: ./src, git: { url: https://forge.banskabystrica.sk/mesto/app.git, ref: main } }",
    );
    let app = App::from_yaml(&both).expect("parses");
    assert!(matches!(
        app.validate().expect_err("path and git together"),
        Error::Name {
            field: "source",
            ..
        }
    ));

    let neither = GOLDEN.replace("  source: { path: ./src }", "  source: {}");
    let app = App::from_yaml(&neither).expect("parses");
    assert!(app.validate().is_err(), "an app needs a source");

    let git = GOLDEN.replace(
        "  source: { path: ./src }",
        "  source: { git: { url: https://forge.banskabystrica.sk/mesto/app.git, ref: main, path: apps/air } }",
    );
    App::from_yaml(&git)
        .expect("parses")
        .validate()
        .expect("git source validates");

    let plaintext = GOLDEN.replace(
        "  source: { path: ./src }",
        "  source: { git: { url: http://forge.banskabystrica.sk/mesto/app.git, ref: main } }",
    );
    let app = App::from_yaml(&plaintext).expect("parses");
    assert!(matches!(
        app.validate().expect_err("plaintext http forge"),
        Error::Name {
            field: "source.git.url",
            ..
        }
    ));

    let escaping = GOLDEN.replace("  source: { path: ./src }", "  source: { path: ../../etc }");
    let app = App::from_yaml(&escaping).expect("parses");
    assert!(app.validate().is_err());
}

#[test]
fn build_must_pin_toolchain_versions_ap11() {
    let empty = GOLDEN.replace(r#"  build: { rust: "1.90", node: "22" }"#, "  build: {}");
    let app = App::from_yaml(&empty).expect("parses");
    assert!(matches!(
        app.validate().expect_err("no toolchain pinned"),
        Error::Name { field: "build", .. }
    ));

    let unpinned = GOLDEN.replace(
        r#"  build: { rust: "1.90", node: "22" }"#,
        r#"  build: { rust: "" }"#,
    );
    let app = App::from_yaml(&unpinned).expect("parses");
    assert!(app.validate().is_err(), "an empty version is not a pin");
}

#[test]
fn data_needs_are_required_and_validated_ap04_ap05() {
    let none = GOLDEN.replace("  dataNeeds:", "  dataNeeds: []\n  unusedDataNeeds:");
    // the replacement above would introduce an unknown field, so build the empty case directly
    let _ = none;
    let mut app = App::from_yaml(GOLDEN).expect("valid golden YAML");
    app.spec.data_needs.clear();
    assert!(matches!(
        app.validate().expect_err("an app with no declared needs"),
        Error::Name {
            field: "dataNeeds",
            ..
        }
    ));

    let mut no_types = App::from_yaml(GOLDEN).expect("valid golden YAML");
    no_types.spec.data_needs[0].types.clear();
    assert!(matches!(
        no_types.validate().expect_err("no types"),
        Error::Name {
            field: "dataNeeds.types",
            ..
        }
    ));

    let mut duplicate_type = App::from_yaml(GOLDEN).expect("valid golden YAML");
    duplicate_type.spec.data_needs[0].types = vec![
        "AirQualityObserved".to_string(),
        "AirQualityObserved".to_string(),
    ];
    assert!(duplicate_type.validate().is_err());

    let mut bad_type = App::from_yaml(GOLDEN).expect("valid golden YAML");
    bad_type.spec.data_needs[0].types = vec!["air quality".to_string()];
    assert!(bad_type.validate().is_err());

    let mut no_ops = App::from_yaml(GOLDEN).expect("valid golden YAML");
    no_ops.spec.data_needs[0].operations.clear();
    assert!(matches!(
        no_ops.validate().expect_err("no operations"),
        Error::Name {
            field: "dataNeeds.operations",
            ..
        }
    ));

    let mut wrong_ref_kind = App::from_yaml(GOLDEN).expect("valid golden YAML");
    wrong_ref_kind.spec.data_needs[0].context_space_ref = Ref::Typed(jc_core::envelope::TypedRef {
        kind: "Endpoint".to_string(),
        name: "ovzdusie".to_string(),
        namespace: None,
    });
    assert!(matches!(
        wrong_ref_kind
            .validate()
            .expect_err("an app reads a space, not an endpoint"),
        Error::Kind {
            expected: "ContextSpace",
            ..
        }
    ));
}

#[test]
fn unknown_cim009_operation_names_are_rejected_r8() {
    let bad = GOLDEN.replace(
        "      operations: [queryEntity, retrieveEntity, queryTemporal]",
        "      operations: [queryEntity, readEverything]",
    );
    assert!(
        App::from_yaml(&bad).is_err(),
        "an operation outside CIM 009 clause 4.20 must not deserialize"
    );

    let group = GOLDEN.replace(
        "      operations: [queryEntity, retrieveEntity, queryTemporal]",
        "      operations: [retrieveOps]",
    );
    App::from_yaml(&group)
        .expect("groups are legal")
        .validate()
        .expect("a group validates");
}

#[test]
fn write_operations_raise_the_lane_ap09() {
    let app = App::from_yaml(GOLDEN).expect("valid golden YAML");
    assert!(!app.spec.write_operations(), "the golden app only reads");
    assert!(
        app.spec.requires_red_lane(),
        "but it is public, which is red (AP-10)"
    );

    let mut writer = App::from_yaml(GOLDEN).expect("valid golden YAML");
    writer.spec.visibility = AppVisibility::Project;
    assert!(!writer.spec.requires_red_lane());
    writer.spec.data_needs[0]
        .operations
        .push(OperationRef::Single(Operation::CreateEntity));
    assert!(writer.spec.write_operations());
    assert!(writer.spec.requires_red_lane());

    // A group that stands for updates counts as a write; a read-only group does not.
    let mut group_writer = App::from_yaml(GOLDEN).expect("valid golden YAML");
    group_writer.spec.visibility = AppVisibility::Project;
    group_writer.spec.data_needs[0].operations =
        vec![OperationRef::Group(OperationGroup::UpdateOps)];
    assert!(group_writer.spec.write_operations());

    let mut group_reader = App::from_yaml(GOLDEN).expect("valid golden YAML");
    group_reader.spec.data_needs[0].operations =
        vec![OperationRef::Group(OperationGroup::RetrieveOps)];
    assert!(!group_reader.spec.write_operations());
}

#[test]
fn constraints_are_validated_ap05() {
    let mut bad_window = App::from_yaml(GOLDEN).expect("valid golden YAML");
    for window in ["1D", "P", "", "P1X", "yesterday"] {
        bad_window.spec.data_needs[0].temporal_q = Some(TemporalConstraint {
            window: window.to_string(),
        });
        assert!(
            bad_window.validate().is_err(),
            "window `{window}` must be refused"
        );
    }
    bad_window.spec.data_needs[0].temporal_q = Some(TemporalConstraint {
        window: "PT12H".to_string(),
    });
    assert!(bad_window.validate().is_ok());

    let mut bad_scope = App::from_yaml(GOLDEN).expect("valid golden YAML");
    for scope in ["geo/SK/BB", "//geo/SK", ""] {
        bad_scope.spec.data_needs[0].geo_q = Some(GeoConstraint {
            within: GeoWithin {
                scope_ref: scope.to_string(),
            },
        });
        assert!(
            bad_scope.validate().is_err(),
            "scopeRef `{scope}` must be refused"
        );
    }
}

#[test]
fn limits_and_csp_ap12_ap17() {
    let mut zero = App::from_yaml(GOLDEN).expect("valid golden YAML");
    zero.spec.limits = Some(AppLimits {
        requests_per_minute: Some(0),
        max_file_rows: None,
    });
    assert!(matches!(
        zero.validate()
            .expect_err("a zero rate limit blocks the app"),
        Error::Name {
            field: "limits.requestsPerMinute",
            ..
        }
    ));

    let mut wildcard = App::from_yaml(GOLDEN).expect("valid golden YAML");
    wildcard.spec.csp = Some(ContentSecurityPolicy {
        connect_src: vec!["*".to_string()],
        frame_ancestors: vec!["none".to_string()],
    });
    assert!(matches!(
        wildcard.validate().expect_err("a wildcard connect-src"),
        Error::Name {
            field: "csp.connectSrc",
            ..
        }
    ));

    let mut plaintext = App::from_yaml(GOLDEN).expect("valid golden YAML");
    plaintext.spec.csp = Some(ContentSecurityPolicy {
        connect_src: vec!["http://tracker.example.com".to_string()],
        frame_ancestors: vec![],
    });
    assert!(plaintext.validate().is_err());

    let mut issuer = App::from_yaml(GOLDEN).expect("valid golden YAML");
    issuer.spec.csp = Some(ContentSecurityPolicy {
        connect_src: vec![
            "self".to_string(),
            "https://id.banskabystrica.sk".to_string(),
        ],
        frame_ancestors: vec!["none".to_string()],
    });
    assert!(
        issuer.validate().is_ok(),
        "the OIDC issuer is allowed (AP-11)"
    );
}

#[test]
fn lifecycle_transitions_ap18() {
    use AppLifecycle::{Draft, Preview, Published, Retired};

    for (from, to) in [
        (Draft, Preview),
        (Preview, Published),
        (Published, Retired),
        (Draft, Retired),
        (Preview, Retired),
        (Draft, Draft),
        (Retired, Retired),
    ] {
        assert!(
            from.allows_transition_to(to),
            "{from} -> {to} must be allowed"
        );
    }

    for (from, to) in [
        (Published, Preview),
        (Published, Draft),
        (Preview, Draft),
        (Retired, Published),
        (Retired, Draft),
        (Draft, Published),
    ] {
        assert!(
            !from.allows_transition_to(to),
            "{from} -> {to} must be refused"
        );
    }

    let mut private_published = App::from_yaml(GOLDEN).expect("valid golden YAML");
    private_published.spec.lifecycle = Published;
    private_published.spec.visibility = AppVisibility::Private;
    assert!(matches!(
        private_published
            .validate()
            .expect_err("published but unreachable"),
        Error::Name {
            field: "visibility",
            ..
        }
    ));
}

/// AP-13a: the artifact an App runs is named in `status.build`, written back by the build lane.
#[test]
fn the_build_status_parses_and_a_malformed_one_is_refused() {
    let digest = format!("sha256:{}", "a1b2c3d4".repeat(8));
    let with_build = format!(
        "{GOLDEN}status:\n  phase: Live\n  build:\n    digest: \"{digest}\"\n    \
         commit: 8c56954a1f0e\n    sdkVersion: 0.4.1\n    builtAt: \"2026-09-17T06:00:00Z\"\n"
    );
    let app = App::from_yaml(&with_build).expect("parse");
    app.validate().expect("valid");
    let build = app
        .status
        .as_ref()
        .and_then(|s| s.build.as_ref())
        .expect("build");
    assert_eq!(build.digest, digest);
    assert_eq!(build.sdk_version, "0.4.1");

    for (field, line) in [
        ("digest", "    digest: \"sha256:deadbeef\"\n"),
        ("commit", "    commit: nothexadecimal\n"),
        ("sdkVersion", "    sdkVersion: \"\"\n"),
    ] {
        let broken = with_build
            .lines()
            .map(|text| {
                if text.trim_start().starts_with(&format!("{field}:")) {
                    line.trim_end().to_owned()
                } else {
                    text.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let app = App::from_yaml(&broken).expect("parses");
        let refused = app.validate().expect_err(&format!("{field} is refused"));
        assert!(format!("{refused}").contains(field), "{field}: {refused}");
    }
}

/// AP-13a: a digest somebody typed into an annotation would deploy an artifact this platform
/// never built, so the manifest is refused rather than quietly stripped.
#[test]
fn an_image_or_module_annotation_on_an_app_is_refused() {
    for key in ["joinedcontext.com/image", "joinedcontext.com/module"] {
        let written = GOLDEN.replace(
            "    joinedcontext.com/generated-by:",
            &format!("    {key}: \"sha256:deadbeef\"\n    joinedcontext.com/generated-by:"),
        );
        let app = App::from_yaml(&written).expect("parses");
        let refused = app.validate().expect_err("the annotation is refused");
        assert!(format!("{refused}").contains(key), "{refused}");
        assert!(format!("{refused}").contains("status.build"), "{refused}");
    }
}

/// AP-87: a published static App names its repository. One naming a folder of the configuration
/// repository is refused with the field and the way out; the same App with `source.git`, the
/// bundle the Portal image ships, and every other lifecycle or class are accepted.
#[test]
fn a_published_static_app_without_a_repository_is_refused_unless_the_portal_ships_it() {
    use jc_core::kinds::app::{GitSource, SHIPPED_WITH_ANNOTATION};

    let mut app = App::from_yaml(GOLDEN).expect("valid golden YAML");
    app.spec.class = AppClass::Static;
    // A static bundle is never built by rust (AP-83).
    app.spec.build.0.remove("rust");
    app.spec.visibility = AppVisibility::Project;
    app.spec.lifecycle = AppLifecycle::Published;
    match app.validate().expect_err("a folder the lane cannot build") {
        Error::Name { field, reason, .. } => {
            assert_eq!(field, "spec.source");
            assert!(
                reason.contains("retire") && reason.contains("AP-87"),
                "{reason}"
            );
        }
        other => panic!("unexpected error {other:?}"),
    }

    let mut shipped = app.clone();
    shipped
        .metadata
        .annotations
        .insert(SHIPPED_WITH_ANNOTATION.to_owned(), "portal".to_owned());
    assert!(shipped.validate().is_ok(), "the bundle the image ships");
    shipped
        .metadata
        .annotations
        .insert(SHIPPED_WITH_ANNOTATION.to_owned(), "gitea".to_owned());
    assert!(shipped.validate().is_err(), "only `portal` ships a bundle");

    let mut in_git = app.clone();
    in_git.spec.source.path = None;
    in_git.spec.source.git = Some(GitSource {
        url: "https://forge.example/joinedcontext/helsinki_bikes.git".to_owned(),
        git_ref: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: None,
    });
    assert!(in_git.validate().is_ok(), "its own repository");

    for lifecycle in [
        AppLifecycle::Draft,
        AppLifecycle::Preview,
        AppLifecycle::Retired,
    ] {
        let mut other = app.clone();
        other.spec.lifecycle = lifecycle;
        assert!(other.validate().is_ok(), "{lifecycle} is not served");
    }
    let mut fullstack = app.clone();
    fullstack.spec.class = AppClass::Fullstack;
    assert!(
        fullstack.validate().is_ok(),
        "the lane of AP-87 is the static one"
    );
    // `jcctl validate` and the Portal's write doors reach the rule by the kind's name.
    let yaml = app.to_yaml().expect("the manifest serializes");
    assert!(matches!(
        jc_core::registry::validate_yaml("App", &yaml),
        Some(Err(_))
    ));
}

/// A plain-HTML application: `static` with `build: {}`, published from its own repository.
const PLAIN_HTML: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: App
metadata:
  name: helsinki-events
  namespace: helsinki
  title: { en: "Events", fi: "Tapahtumat" }
spec:
  kind: static
  source:
    git:
      url: https://forge.example/joinedcontext/helsinki_events.git
      ref: 0123456789abcdef0123456789abcdef01234567
  build: {}
  visibility: organization
  lifecycle: published
  dataNeeds:
    - contextSpaceRef: { kind: ContextSpace, name: helsinki }
      types: [Event]
      operations: [queryEntity, retrieveEntity]
"#;

/// AP-83: `build: {}` on a static App is no build step, and the manifest round-trips and passes
/// the registry `jcctl validate` calls.
#[test]
fn a_static_app_with_an_empty_build_is_a_plain_html_app_ap83() {
    let app = App::from_yaml(PLAIN_HTML).expect("parses");
    app.validate()
        .expect("no build step is valid on a static app");
    assert!(app.spec.build.0.is_empty());

    let yaml = app.to_yaml().expect("serializes");
    assert_eq!(App::from_yaml(&yaml).expect("round-trips"), app);
    assert!(
        matches!(
            jc_core::registry::validate_yaml("App", PLAIN_HTML),
            Some(Ok(_))
        ),
        "what jcctl validate says of the example"
    );
}

/// AP-83, AP-11: only a static App may skip the build, and a static App is never built by rust.
#[test]
fn an_empty_build_or_a_rust_toolchain_is_refused_where_it_cannot_run_ap83() {
    for class in ["service", "fullstack"] {
        let yaml = PLAIN_HTML.replace("  kind: static", &format!("  kind: {class}"));
        let app = App::from_yaml(&yaml).expect("parses");
        match app.validate().expect_err("a backend needs a toolchain") {
            Error::Name { field, .. } => assert_eq!(field, "build", "{class}"),
            other => panic!("{class}: unexpected error {other:?}"),
        }
    }

    let rust = PLAIN_HTML.replace("  build: {}", r#"  build: { rust: "1.90", node: "22" }"#);
    let app = App::from_yaml(&rust).expect("parses");
    match app
        .validate()
        .expect_err("a static bundle is not compiled from Rust")
    {
        Error::Name {
            field,
            value,
            reason,
        } => {
            assert_eq!((field, value.as_str()), ("build", "rust"));
            assert!(reason.contains("fullstack"), "{reason}");
        }
        other => panic!("unexpected error {other:?}"),
    }

    let node = PLAIN_HTML.replace("  build: {}", r#"  build: { node: "22" }"#);
    App::from_yaml(&node)
        .expect("parses")
        .validate()
        .expect("a Vite build stays valid");
}
