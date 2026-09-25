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

/// An App with two roles and a role-gated write, the shape of docs Architecture/16 §12 and of
/// the T-2598 sample (ADR-N-027), kept where a reader of the examples finds it.
const ROLES: &str = include_str!("../../../examples/apps/alerts/app.yaml");

/// The field and value of a refusal, which is what a person reads.
fn refused(app: &App) -> (&'static str, String) {
    match app.validate().expect_err("the app is refused") {
        Error::Name { field, value, .. } => (field, value),
        other => panic!("expected a named field, got {other:?}"),
    }
}

fn roles_app() -> App {
    App::from_yaml(ROLES).expect("the roles example parses")
}

/// AP-90, AP-91, AP-96: roles, their members and a role-gated data need parse, validate and
/// survive a round trip unchanged.
#[test]
fn an_app_with_roles_members_and_a_role_gated_write_parses_validates_and_roundtrips() {
    let app = roles_app();
    app.validate().expect("the roles example validates");
    assert_eq!(app.spec.visibility, AppVisibility::Roles);
    assert_eq!(app.spec.roles.len(), 2);
    assert_eq!(app.spec.roles[0].title["sk"], "Čitateľ");
    assert_eq!(
        app.spec.access[1].subjects[0].user.as_deref(),
        Some("jana.kovacova@hel.fi")
    );
    assert_eq!(app.spec.data_needs[1].roles, ["editor"]);
    assert!(
        app.spec.data_needs[0].roles.is_empty(),
        "absent means every caller"
    );

    let serialized = app.to_yaml().expect("serialize");
    assert!(
        !serialized.contains("roles: []"),
        "an empty list is not written: {serialized}"
    );
    assert_eq!(app, App::from_yaml(&serialized).expect("re-import"));
    // The golden app has no roles and still validates: the fields are optional.
    App::from_yaml(GOLDEN)
        .expect("golden")
        .validate()
        .expect("no roles is fine");
}

/// AP-90: a role name is a short lower-case slug, declared once, and an app has at most 16.
#[test]
fn a_role_name_that_is_not_a_slug_or_is_declared_twice_or_one_too_many_is_refused() {
    for bad in [
        "Editor",
        "1editor",
        "editor_2",
        "",
        "a-very-long-role-name-over-32-chars",
    ] {
        let mut app = roles_app();
        app.spec.roles[0].name = bad.to_owned();
        app.spec.access.retain(|a| a.role != "viewer");
        assert_eq!(refused(&app), ("roles[].name", bad.to_owned()), "{bad}");
    }

    let mut twice = roles_app();
    twice.spec.roles[1].name = "viewer".to_owned();
    twice.spec.access.retain(|a| a.role != "editor");
    twice.spec.data_needs[1].roles.clear();
    assert_eq!(refused(&twice), ("roles[].name", "viewer".to_owned()));

    let mut many = roles_app();
    let template = many.spec.roles[0].clone();
    many.spec.roles = (0..17)
        .map(|i| jc_core::kinds::AppRole {
            name: format!("role-{i}"),
            ..template.clone()
        })
        .collect();
    many.spec.access.clear();
    many.spec.data_needs[1].roles = vec!["role-0".to_owned()];
    assert_eq!(refused(&many), ("roles", "17".to_owned()));
}

/// AP-91, AP-96: `access` and `dataNeeds[].roles` name only declared roles, and a role's
/// members are listed in one entry.
#[test]
fn access_or_a_data_need_naming_an_undeclared_role_is_refused() {
    let mut access = roles_app();
    access.spec.access[0].role = "admin".to_owned();
    assert_eq!(refused(&access), ("access[].role", "admin".to_owned()));

    let mut split = roles_app();
    split.spec.access[1].role = "viewer".to_owned();
    assert_eq!(refused(&split), ("access[].role", "viewer".to_owned()));

    let mut need = roles_app();
    need.spec.data_needs[1].roles = vec!["admin".to_owned()];
    assert_eq!(refused(&need), ("dataNeeds[].roles", "admin".to_owned()));
}

/// AP-91: a member is exactly one of a lower-case e-mail and a group name, never a wildcard,
/// never twice, and a role lists at least one.
#[test]
fn a_member_that_is_not_one_lower_case_address_or_one_group_is_refused() {
    use jc_core::kinds::Subject;
    let user = |u: &str| Subject {
        user: Some(u.to_owned()),
        group: None,
    };
    let group = |g: &str| Subject {
        user: None,
        group: Some(g.to_owned()),
    };
    for (subjects, value) in [
        (vec![user("Jana.Kovacova@hel.fi")], "Jana.Kovacova@hel.fi"),
        (vec![user("jana.kovacova")], "jana.kovacova"),
        (vec![user("*@hel.fi")], "*@hel.fi"),
        (vec![user(" jana@hel.fi")], " jana@hel.fi"),
        (vec![group("Alert Editors")], "Alert Editors"),
        (vec![group("*")], "*"),
        (
            vec![Subject {
                user: Some("jana@hel.fi".into()),
                group: Some("x".into()),
            }],
            "",
        ),
        (vec![Subject::default()], ""),
        (vec![], ""),
        (
            vec![user("jana@hel.fi"), user("jana@hel.fi")],
            "jana@hel.fi",
        ),
    ] {
        let mut app = roles_app();
        app.spec.access[1].subjects = subjects.clone();
        assert_eq!(
            refused(&app),
            ("access[].subjects", value.to_owned()),
            "{subjects:?}"
        );
    }
}

/// AP-94: `visibility: roles` needs a role to admit anybody, and only the static host enforces
/// it, so a service or fullstack app may not say it.
#[test]
fn visibility_roles_without_roles_or_on_a_pod_served_app_is_refused() {
    let mut none = roles_app();
    none.spec.roles.clear();
    none.spec.access.clear();
    none.spec.data_needs[1].roles.clear();
    assert_eq!(refused(&none), ("visibility", "roles".to_owned()));

    for class in [AppClass::Service, AppClass::Fullstack] {
        let mut app = roles_app();
        app.spec.class = class;
        app.spec.build = jc_core::kinds::AppBuild(
            [
                ("rust".to_owned(), "1.90".to_owned()),
                ("node".to_owned(), "22".to_owned()),
            ]
            .into(),
        );
        assert_eq!(refused(&app), ("visibility", "roles".to_owned()), "{class}");
        // The roles themselves are fine on such an app: its data grants still follow them.
        app.spec.visibility = AppVisibility::Organization;
        app.validate().expect("roles without visibility: roles");
    }
}

/// AP-90, AP-16: a role carries nothing but its name, title and description.
#[test]
fn an_unknown_field_in_a_role_or_an_access_entry_is_refused_at_parse_time() {
    for (from, to) in [
        (
            "description: \"Reads the alerts\" }",
            "description: \"Reads the alerts\", secretRef: x }",
        ),
        (
            "{ role: viewer, subjects:",
            "{ role: viewer, everyone: true, subjects:",
        ),
    ] {
        let text = ROLES.replacen(from, to, 1);
        assert_ne!(text, ROLES, "{from}");
        assert!(App::from_yaml(&text).is_err(), "{to}");
    }
}

/// AP-120, T-2690: an App that does not say who may open it asks for a login: it is `project`,
/// never `public`, and it serializes back with the value it was read as.
#[test]
fn a_missing_visibility_is_project_and_never_public() {
    let yaml = GOLDEN.replace("  visibility: public\n", "");
    assert!(
        !yaml.contains("visibility"),
        "the fixture still names a visibility"
    );
    let app = App::from_yaml(&yaml).expect("an App without visibility parses");
    app.validate().expect("and validates");
    assert_eq!(app.spec.visibility, AppVisibility::Project);
    assert_eq!(AppVisibility::default(), AppVisibility::Project);
    let written = app.to_yaml().expect("serializes");
    assert!(written.contains("visibility: project"), "{written}");
}

/// The golden App with `spec.egress` set to the YAML list `entries`.
fn with_egress(entries: &str) -> Result<App, String> {
    let app =
        App::from_yaml(&format!("{GOLDEN}  egress: {entries}\n")).map_err(|err| err.to_string())?;
    app.validate().map_err(|err| err.to_string())?;
    Ok(app)
}

fn refusal(entries: &str) -> String {
    with_egress(entries).expect_err(entries)
}

#[test]
fn egress_takes_networks_with_their_ports_ap134() {
    let app = with_egress("[{ cidr: 203.0.113.0/24, ports: [443] }, { cidr: \"2001:db8::/32\", ports: [443, 8443] }, { cidr: 198.51.100.7/32, ports: [5432] }, { cidr: \"2001:db8::1/128\", ports: [22] }]")
        .expect("networks with ports are a valid egress");
    assert_eq!(app.spec.egress.len(), 4);
    assert_eq!(app.spec.egress[1].ports, [443, 8443]);
    let serialized = app.to_yaml().expect("serialize");
    assert_eq!(app, App::from_yaml(&serialized).expect("re-import"));
    assert!(
        !App::from_yaml(GOLDEN)
            .expect("golden")
            .to_yaml()
            .expect("serialize")
            .contains("egress"),
        "an App that declares none writes none"
    );
}

#[test]
fn egress_refuses_everything_names_and_sloppy_networks_ap134() {
    for (entries, says) in [
        ("[{ cidr: 0.0.0.0/0, ports: [443] }]", "every address"),
        ("[{ cidr: \"::/0\", ports: [443] }]", "every address"),
        (
            "[{ cidr: api.example.com/32, ports: [443] }]",
            "never a host name",
        ),
        (
            "[{ cidr: api.example.com, ports: [443] }]",
            "an address and a prefix length",
        ),
        (
            "[{ cidr: 203.0.113.0, ports: [443] }]",
            "an address and a prefix length",
        ),
        ("[{ cidr: 203.0.113.0/33, ports: [443] }]", "from 1 to 32"),
        (
            "[{ cidr: \"2001:db8::/129\", ports: [443] }]",
            "from 1 to 128",
        ),
        ("[{ cidr: 203.0.113.9/24, ports: [443] }]", "host bits"),
        ("[{ cidr: \"2001:db8::1/64\", ports: [443] }]", "host bits"),
        ("[{ cidr: 203.0.113.0/24, ports: [] }]", "needs its ports"),
        ("[{ cidr: 203.0.113.0/24, ports: [0] }]", "needs its ports"),
    ] {
        let message = refusal(entries);
        assert!(message.contains(says), "{entries}: {message}");
        assert!(message.contains("AP-134"), "{entries}: {message}");
    }
    // The refusal names the entry, so a person finds it in a list of several.
    let message =
        refusal("[{ cidr: 203.0.113.0/24, ports: [443] }, { cidr: 0.0.0.0/0, ports: [443] }]");
    assert!(message.contains("`0.0.0.0/0`"), "{message}");
}

#[test]
fn egress_refuses_an_unknown_key_and_a_port_out_of_range_ap134() {
    for entries in [
        "[{ cidr: 203.0.113.0/24, ports: [443], host: api.example.com }]",
        "[{ cidr: 203.0.113.0/24, ports: [65536] }]",
        "[{ cidr: 203.0.113.0/24 }]",
    ] {
        assert!(
            App::from_yaml(&format!("{GOLDEN}  egress: {entries}\n")).is_err(),
            "{entries} parses"
        );
    }
}
