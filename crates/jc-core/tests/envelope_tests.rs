//! T-0109: ResourceEnvelope, ObjectMeta, Ref and SecretRef (MF-01…MF-08).

use jc_core::{Kind, ObjectMeta, Phase, Ref, ResourceEnvelope, Scope, SecretRef};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `ContextSpace` spec of docs/Architecture/06-configuration-as-code.md, as far as this
/// shot models it. The real kind lands with T-0112; this stands in so the envelope can be
/// tested against the real golden manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DemoSpec {
    is_sandbox: bool,
    default_locale: String,
}

impl Kind for DemoSpec {
    const KIND: &'static str = "ContextSpace";
    const PLURAL: &'static str = "spaces";
    const SCOPE: Scope = Scope::Project;
    const PATH_TEMPLATE: &'static str = "projects/{project}/spaces/{name}/space.yaml";
}

type Demo = ResourceEnvelope<DemoSpec>;

/// Verbatim from docs/Architecture/06-configuration-as-code.md section 2.
const GOLDEN: &str = r#"
apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: bb-doprava
  labels:
    joinedcontext.com/domain: environment
  annotations:
    joinedcontext.com/managed-attributes: "title,description,tags"
    joinedcontext.com/imported-from: "https://udp.example.sk/bb-doprava@3f9c2e1"
  title: { sk: "Ovzdušie", en: "Air quality", de: "Luftqualität", cs: "Ovzduší" }
  description: { sk: "Merania kvality ovzdušia", en: "Air-quality observations" }
spec:
  isSandbox: false
  defaultLocale: sk
"#;

fn golden() -> Demo {
    Demo::from_yaml(GOLDEN).expect("the documented ContextSpace manifest must deserialize")
}

#[test]
fn the_documented_manifest_round_trips_with_every_field_preserved() {
    let parsed = golden();
    assert_eq!(parsed.api_version, "joinedcontext.com/v1alpha1");
    assert_eq!(parsed.kind, "ContextSpace");
    assert_eq!(parsed.metadata.name, "ovzdusie");
    assert_eq!(parsed.metadata.namespace.as_deref(), Some("bb-doprava"));
    assert_eq!(
        parsed.metadata.labels.get("joinedcontext.com/domain"),
        Some(&"environment".to_string())
    );
    assert_eq!(
        parsed
            .metadata
            .annotations
            .get(jc_core::annotations::MANAGED_ATTRIBUTES),
        Some(&"title,description,tags".to_string())
    );
    let title = parsed.metadata.title.as_ref().expect("title");
    assert_eq!(title.get("sk"), Some("Ovzdušie"));
    assert_eq!(title.get("de"), Some("Luftqualität"));
    assert_eq!(title.get("cs"), Some("Ovzduší"));
    assert!(!parsed.spec.is_sandbox);
    assert_eq!(parsed.spec.default_locale, "sk");

    let yaml = parsed.to_yaml().expect("serialize");
    let again = Demo::from_yaml(&yaml).expect("re-parse the exported manifest");
    assert_eq!(
        parsed, again,
        "export/import must preserve 100% of the fields"
    );

    let json = parsed.to_json().expect("serialize json");
    assert_eq!(Demo::from_json(&json).expect("re-parse json"), parsed);
}

#[test]
fn the_commented_out_status_block_of_the_docs_deserializes_and_is_never_exported() {
    // docs/Architecture/06 shows this block commented out; uncommenting it must still parse,
    // and MF-04 says it must never reach Git.
    let with_status = format!(
        "{GOLDEN}\nstatus:\n  phase: Live\n  observedRevision: 3f9c2e1\n  conditions:\n    - {{ type: Reconciled, status: \"True\", reason: Applied, lastTransitionTime: 2026-09-05T10:00:00Z }}\n"
    );
    let parsed = Demo::from_yaml(&with_status).expect("status must deserialize");
    let status = parsed.status.as_ref().expect("status present");
    assert_eq!(status.phase, Some(Phase::Live));
    assert_eq!(status.observed_revision.as_deref(), Some("3f9c2e1"));
    assert_eq!(status.conditions.len(), 1);

    assert!(!parsed.to_yaml().expect("yaml").contains("status"));
    assert!(!parsed.to_json().expect("json").contains("status"));
    assert!(parsed.clone().without_status().status.is_none());
    let mut stripped = parsed;
    stripped.strip_status();
    assert!(stripped.status.is_none());
}

#[test]
fn a_wrong_api_version_is_refused() {
    let bad = GOLDEN.replace("joinedcontext.com/v1alpha1", "joinedcontext.com/v1");
    let err = Demo::from_yaml(&bad).expect_err("apiVersion must be pinned");
    assert!(err.to_string().contains("apiVersion"), "{err}");
}

#[test]
fn a_wrong_kind_is_refused_for_the_typed_envelope() {
    let bad = GOLDEN.replace("kind: ContextSpace", "kind: Endpoint");
    let err = Demo::from_yaml(&bad).expect_err("kind must match the spec type");
    assert!(err.to_string().contains("kind"), "{err}");
}

#[test]
fn unknown_fields_are_refused_everywhere() {
    for bad in [
        GOLDEN.replace("  name: ovzdusie", "  name: ovzdusie\n  uid: 1234"),
        GOLDEN.replace("  isSandbox: false", "  isSandbox: false\n  ttlDays: 14"),
        format!("{GOLDEN}\nextra: true\n"),
    ] {
        assert!(
            Demo::from_yaml(&bad).is_err(),
            "deny_unknown_fields must reject:\n{bad}"
        );
    }
}

#[test]
fn dns_1123_names_block_path_traversal_and_illegal_labels() {
    for bad in [
        "Mobility_Traffic",
        "-x",
        "x-",
        "../etc/passwd",
        "a/b",
        "a.b",
        "",
        &"a".repeat(64),
    ] {
        let env = Demo::new(
            ObjectMeta::new(bad, "bb-doprava"),
            DemoSpec {
                is_sandbox: false,
                default_locale: "sk".into(),
            },
        );
        assert!(env.validate().is_err(), "`{bad}` must be an invalid name");
    }
    let ok = Demo::new(
        ObjectMeta::new("a".repeat(63), "bb-doprava"),
        DemoSpec {
            is_sandbox: false,
            default_locale: "sk".into(),
        },
    );
    assert!(ok.validate().is_ok(), "63 characters is the DNS-1123 limit");
}

#[test]
fn resource_path_is_derived_from_the_identity_tuple() {
    assert_eq!(
        golden().resource_path().expect("valid manifest"),
        "projects/bb-doprava/spaces/ovzdusie/space.yaml"
    );
}

#[test]
fn scope_and_namespace_must_agree() {
    // A project-scoped kind may not sit in the organization namespace (MF-02).
    let mut env = golden();
    env.metadata.namespace = Some("org".into());
    assert!(env.validate().is_err());

    // The namespace is required even though serde makes it optional.
    env.metadata.namespace = None;
    assert!(env.validate().is_err());

    env.metadata.namespace = Some("bb-doprava".into());
    assert!(env.validate().is_ok());
}

#[test]
fn secret_ref_rejects_an_inline_secret_value() {
    let ok: SecretRef = serde_json::from_str(r#"{"name":"mqtt-credentials","key":"password"}"#)
        .expect("a plain secretRef");
    assert_eq!(ok.name, "mqtt-credentials");
    for bad in [
        r#"{"name":"mqtt-credentials","value":"hunter2"}"#,
        r#"{"name":"mqtt-credentials","password":"hunter2"}"#,
        r#"{"name":"mqtt-credentials","key":"password","secret":"hunter2"}"#,
    ] {
        assert!(
            serde_json::from_str::<SecretRef>(bad).is_err(),
            "an inline secret must be refused at parse time: {bad}"
        );
    }
}

#[test]
fn refs_accept_both_the_bare_and_the_typed_form() {
    let bare: Ref = serde_json::from_str(r#""ovzdusie""#).expect("bare name form");
    let typed: Ref = serde_json::from_str(r#"{"kind":"ContextSpace","name":"ovzdusie"}"#)
        .expect("typed reference form");
    assert_eq!(bare.name(), "ovzdusie");
    assert_eq!(typed.name(), "ovzdusie");
    assert_eq!(bare.kind(), None);
    assert_eq!(typed.kind(), Some("ContextSpace"));
    assert_eq!(
        serde_json::from_str::<Ref>(r#"{"kind":"ContextSpace","name":"x","namespace":"p"}"#)
            .expect("namespaced")
            .namespace(),
        Some("p")
    );
    // MF-07: a reference is a name or a typed object, never a file path object.
    assert!(serde_json::from_str::<Ref>(r#"{"path":"spaces/ovzdusie.yaml"}"#).is_err());
}

/// T-0453, UI-25: the six lifecycle states the chip renders are the six the contract carries,
/// and each is the one word a manifest writes.
#[test]
fn every_lifecycle_phase_the_ui_shows_round_trips_through_the_envelope() {
    for (phase, written) in [
        (Phase::Draft, "\"Draft\""),
        (Phase::Pending, "\"Pending\""),
        (Phase::Deploying, "\"Deploying\""),
        (Phase::Live, "\"Live\""),
        (Phase::Error, "\"Error\""),
        (Phase::Drifted, "\"Drifted\""),
    ] {
        let json = serde_json::to_string(&phase).expect("a phase serialises");
        assert_eq!(json, written);
        assert_eq!(
            serde_json::from_str::<Phase>(&json).expect("and reads back"),
            phase
        );
    }
}
