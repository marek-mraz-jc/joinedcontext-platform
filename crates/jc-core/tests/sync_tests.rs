//! T-0121: `kind: SyncSource` and the `kind: Bundle` download index (MF-17..MF-19, MF-27..MF-32).

use jc_core::envelope::SecretRef;
use jc_core::error::Error;
use jc_core::kinds::sync::{
    BundleItem, ConflictPolicy, Schedule, SyncMode, SyncOrigin, WebhookAuth,
};
use jc_core::kinds::{Bundle, SyncSource};

/// Verbatim from docs/Architecture/06-configuration-as-code.md section 6.
const GOLDEN_SYNC: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: SyncSource
metadata:
  name: regional-datamodels
  namespace: bb-doprava
spec:
  source:
    git: { url: https://git.region.sk/udp/datamodels.git, ref: main, path: models/transport, secretRef: { name: region-git-ro } }
  schedule: { interval: 30m }
  mode: mirror
  selector: { joinedcontext.com/tier: standard }
  conflictPolicy: replace
  prune: false
  autoMerge: false
"#;

const GOLDEN_BUNDLE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Bundle
metadata:
  name: bb-ovzdusie-export
  namespace: org
spec:
  exportedAt: "2026-09-05T10:00:00Z"
  exportedBy: digitalizacia
  sourceInstance: https://portal.banskabystrica.sk
  sourceRevision: 3f9c2e1
  items:
    - { kind: ContextSpace, namespace: bb-ovzdusie, name: ovzdusie, path: projects/bb-ovzdusie/spaces/ovzdusie/space.yaml }
    - { kind: Endpoint, namespace: bb-ovzdusie, name: air-quality-public, path: projects/bb-ovzdusie/spaces/ovzdusie/endpoints/air-quality-public.yaml }
    - { kind: Organization, name: banskabystrica, path: org.yaml }
  nativeFiles:
    - projects/bb-ovzdusie/spaces/ovzdusie/datamodels/air-quality.linkml.yaml
  omitted: 2
"#;

#[test]
fn golden_sync_source_parses_validates_and_roundtrips() {
    let sync = SyncSource::from_yaml(GOLDEN_SYNC).expect("valid golden YAML");
    sync.validate().expect("golden SyncSource validates");

    assert_eq!(
        sync.resource_path().expect("resource path"),
        "projects/bb-doprava/sync/regional-datamodels.yaml"
    );
    assert_eq!(sync.spec.mode, SyncMode::Mirror);
    assert_eq!(sync.spec.conflict_policy, ConflictPolicy::Replace);
    assert_eq!(sync.spec.schedule.interval_seconds(), Some(1800));
    assert!(!sync.spec.requires_red_lane());

    let serialized = sync.to_yaml().expect("serialize");
    assert_eq!(sync, SyncSource::from_yaml(&serialized).expect("re-import"));
}

#[test]
fn golden_bundle_parses_validates_and_roundtrips() {
    let bundle = Bundle::from_yaml(GOLDEN_BUNDLE).expect("valid golden YAML");
    bundle.validate().expect("golden Bundle validates");

    assert_eq!(
        bundle.resource_path().expect("resource path"),
        "bundle.yaml"
    );
    assert_eq!(bundle.spec.omitted, 2);
    assert!(bundle
        .spec
        .contains("Endpoint", Some("bb-ovzdusie"), "air-quality-public"));
    assert!(bundle.spec.contains("Organization", None, "banskabystrica"));
    assert!(!bundle.spec.contains("Endpoint", None, "air-quality-public"));
    assert!(!bundle
        .spec
        .contains("Endpoint", Some("bb-ovzdusie"), "nope"));

    let serialized = bundle.to_yaml().expect("serialize");
    assert_eq!(bundle, Bundle::from_yaml(&serialized).expect("re-import"));
}

#[test]
fn schedule_interval_table_and_webhook() {
    for (raw, secs) in [("60s", 60u64), ("30m", 1800), ("6h", 21600), ("1d", 86_400)] {
        let s = Schedule::interval(raw);
        assert_eq!(s.interval_seconds(), Some(secs), "{raw}");
        let sync = sync_with_schedule(s);
        assert!(sync.spec.validate().is_ok(), "{raw} must be accepted");
    }

    for raw in ["", "30", "m", "0m", "30x", "-5m", "1.5h", "30 m", "01m"] {
        let sync = sync_with_schedule(Schedule::interval(raw));
        let err = sync
            .spec
            .validate()
            .expect_err(&format!("`{raw}` must be rejected"));
        assert!(matches!(
            err,
            Error::Name {
                field: "schedule.interval",
                ..
            }
        ));
    }

    // 30s is a well-formed interval but below the 1m floor.
    let too_fast = sync_with_schedule(Schedule::interval("30s"));
    assert!(too_fast.spec.validate().is_err());

    // `webhook: true` is a schedule, `webhook: false` is not. A webhook schedule also needs the
    // credential the route authorises it with, which is the case below (MF-44).
    let mut driven = sync_with_schedule(Schedule::webhook());
    driven.spec.webhook = Some(hook("region-hook"));
    driven
        .spec
        .validate()
        .expect("a webhook source with its own secret validates");
    assert_eq!(Schedule::webhook().interval_seconds(), None);
    let err = sync_with_schedule(Schedule {
        interval: None,
        webhook: Some(false),
    })
    .spec
    .validate()
    .expect_err("webhook: false is not a schedule");
    assert!(matches!(
        err,
        Error::Name {
            field: "schedule.webhook",
            ..
        }
    ));
}

#[test]
fn plaintext_http_origins_are_refused_for_every_variant() {
    let cases = [
        (
            r#"    git: { url: http://git.region.sk/udp/datamodels.git, ref: main }"#,
            "source.git.url",
        ),
        (
            r#"    bundle: { url: http://region.sk/bundles/models.tar.gz }"#,
            "source.bundle.url",
        ),
        (
            r#"    platformApi: { baseUrl: http://portal.region.sk, project: doprava }"#,
            "source.platformApi.baseUrl",
        ),
    ];

    for (origin, field) in cases {
        let yaml = GOLDEN_SYNC.replace(
            r#"    git: { url: https://git.region.sk/udp/datamodels.git, ref: main, path: models/transport, secretRef: { name: region-git-ro } }"#,
            origin,
        );
        let sync = SyncSource::from_yaml(&yaml).expect("parses");
        let err = sync.validate().expect_err("plaintext http must be refused");
        match err {
            // T-1194: the reason is the point, not the check. This was proposed for merging
            // with `data_source`'s scheme check into one `validate_url_scheme(value, schemes,
            // field)`; one function returns one reason, and the reason an operator needs here
            // is that plaintext was refused, not that a connection type speaks other schemes.
            Error::Name {
                field: f, reason, ..
            } => {
                assert_eq!(f, field);
                assert!(
                    reason.contains("plaintext http is refused"),
                    "a sync origin says why it was refused: {reason}"
                );
            }
            other => panic!("expected Error::Name, got {other:?}"),
        }
    }

    // The same origins over https are accepted.
    for origin in [
        r#"    bundle: { url: https://region.sk/bundles/models.tar.gz }"#,
        r#"    platformApi: { baseUrl: https://portal.region.sk, project: doprava }"#,
        r#"    git: { url: "git@git.region.sk:udp/datamodels.git", ref: v1.2.0 }"#,
    ] {
        let yaml = GOLDEN_SYNC.replace(
            r#"    git: { url: https://git.region.sk/udp/datamodels.git, ref: main, path: models/transport, secretRef: { name: region-git-ro } }"#,
            origin,
        );
        SyncSource::from_yaml(&yaml)
            .expect("parses")
            .validate()
            .unwrap_or_else(|e| panic!("{origin} must validate: {e}"));
    }
}

#[test]
fn inline_credentials_do_not_deserialize_mf31() {
    let inline = GOLDEN_SYNC.replace(
        "secretRef: { name: region-git-ro }",
        r#"token: "ghp_inline_token_in_a_manifest""#,
    );
    assert!(
        SyncSource::from_yaml(&inline).is_err(),
        "deny_unknown_fields must reject an inline credential (MF-31)"
    );

    let inline_spec = format!("{GOLDEN_SYNC}  credentials:\n    token: inline\n");
    assert!(SyncSource::from_yaml(&inline_spec).is_err());
}

/// MF-31, T-1479: the SyncSource holds its paths to the one rule every kind shares: `./` alone
/// is as empty as `""` (its own copy of the check used to let `./` through).
#[test]
fn git_path_must_stay_inside_the_repository() {
    for path in [
        "/etc/passwd",
        "../../secrets",
        "models/../../../etc",
        "",
        "./",
    ] {
        let yaml = GOLDEN_SYNC.replace("path: models/transport", &format!("path: \"{path}\""));
        let sync = SyncSource::from_yaml(&yaml).expect("parses");
        let err = sync
            .validate()
            .expect_err(&format!("`{path}` must be refused"));
        assert!(matches!(
            err,
            Error::Name {
                field: "source.git.path",
                ..
            }
        ));
    }
}

#[test]
fn prune_and_auto_merge_raise_the_lane_cc70_cc19() {
    let mut sync = SyncSource::from_yaml(GOLDEN_SYNC).expect("valid golden YAML");
    assert!(!sync.spec.requires_red_lane());

    sync.spec.prune = true;
    assert!(sync.spec.requires_red_lane());

    sync.spec.prune = false;
    sync.spec.auto_merge = true;
    assert!(sync.spec.requires_red_lane());
}

#[test]
fn selector_labels_are_validated() {
    let mut sync = SyncSource::from_yaml(GOLDEN_SYNC).expect("valid golden YAML");
    sync.spec
        .selector
        .insert("not a label key".to_string(), "standard".to_string());
    let err = sync.validate().expect_err("bad selector key must fail");
    assert!(matches!(
        err,
        Error::Name {
            field: "selector",
            ..
        }
    ));

    let mut sync = SyncSource::from_yaml(GOLDEN_SYNC).expect("valid golden YAML");
    sync.spec
        .selector
        .insert("joinedcontext.com/tier".to_string(), "a".repeat(64));
    assert!(
        sync.validate().is_err(),
        "selector value over 63 chars must fail"
    );
}

#[test]
fn bundle_index_rejects_unknown_kinds_duplicates_and_escaping_paths() {
    let base = Bundle::from_yaml(GOLDEN_BUNDLE).expect("valid golden YAML");

    let mut unknown = base.clone();
    unknown.spec.items[0].kind = "Widget".to_string();
    let err = unknown.validate().expect_err("unknown kind must fail");
    match err {
        Error::Name { field, value, .. } => {
            assert_eq!(field, "items.kind");
            assert_eq!(value, "Widget");
        }
        other => panic!("expected Error::Name, got {other:?}"),
    }

    let mut duplicate = base.clone();
    let first = duplicate.spec.items[0].clone();
    duplicate.spec.items.push(first);
    assert!(
        duplicate.validate().is_err(),
        "duplicate identity must fail"
    );

    // The same name in a different namespace is a different resource, so it is allowed.
    let mut other_namespace = base.clone();
    let mut item = other_namespace.spec.items[0].clone();
    item.namespace = Some("bb-doprava".to_string());
    other_namespace.spec.items.push(item);
    assert!(other_namespace.validate().is_ok());

    let mut escaping = base.clone();
    escaping.spec.items[0].path = "../../../etc/passwd".to_string();
    assert!(escaping.validate().is_err());

    let mut escaping_native = base.clone();
    escaping_native.spec.native_files = vec!["/etc/shadow".to_string()];
    assert!(escaping_native.validate().is_err());

    let mut empty = base.clone();
    empty.spec.items.clear();
    assert!(empty.validate().is_err(), "an empty bundle is not a bundle");

    let mut bad_rev = base;
    bad_rev.spec.source_revision = "not-a-sha".to_string();
    assert!(bad_rev.validate().is_err());
}

#[test]
fn bundle_omitted_defaults_to_zero_mf18() {
    let without_omitted = GOLDEN_BUNDLE.replace("  omitted: 2\n", "");
    let bundle = Bundle::from_yaml(&without_omitted).expect("parses without omitted");
    bundle.validate().expect("validates");
    assert_eq!(bundle.spec.omitted, 0);
}

#[test]
fn secret_ref_is_the_only_credential_channel() {
    let sync = SyncSource::from_yaml(GOLDEN_SYNC).expect("valid golden YAML");
    let secret = sync
        .spec
        .source
        .secret_ref()
        .expect("git origin carries a secretRef");
    assert_eq!(secret.name, "region-git-ro");

    let bundle_origin = SyncOrigin {
        bundle: Some(jc_core::kinds::BundleOrigin {
            url: "https://region.sk/b.tar.gz".to_string(),
            secret_ref: Some(SecretRef {
                name: "region-bundle-ro".to_string(),
                key: None,
                env_var: None,
            }),
        }),
        ..SyncOrigin::default()
    };
    assert_eq!(
        bundle_origin.secret_ref().map(|s| s.name.as_str()),
        Some("region-bundle-ro")
    );
}

fn sync_with_schedule(schedule: Schedule) -> SyncSource {
    let mut sync = SyncSource::from_yaml(GOLDEN_SYNC).expect("valid golden YAML");
    sync.spec.schedule = schedule;
    sync
}

#[test]
fn bundle_item_namespace_is_optional_for_org_scoped_kinds() {
    let item = BundleItem {
        kind: "Organization".to_string(),
        namespace: None,
        name: "banskabystrica".to_string(),
        path: "org.yaml".to_string(),
    };
    let mut bundle = Bundle::from_yaml(GOLDEN_BUNDLE).expect("valid golden YAML");
    bundle.spec.items = vec![item];
    bundle
        .validate()
        .expect("org-scoped item without namespace validates");
}

#[test]
fn a_complete_export_carries_its_readme_and_schemas_in_the_index() {
    // MF-41: a stream export has no README file to put beside its manifests, so the index
    // carries the same text and the same schemas an archive writes at its root.
    let yaml = GOLDEN_BUNDLE.replace(
        "  omitted: 2\n",
        "  omitted: 2\n  readme: |\n    # bb-ovzdusie\n    Two resources.\n  schemas:\n    \
         kinds:\n      Endpoint: { title: Endpoint }\n    models:\n      air-quality: { linkml: \
         \"id: air\" }\n",
    );
    let bundle = Bundle::from_yaml(&yaml).expect("a bundle with a readme parses");
    bundle.validate().expect("it validates");

    let schemas = bundle.spec.schemas.as_ref().expect("schemas");
    assert_eq!(schemas.kinds["Endpoint"]["title"], "Endpoint");
    assert_eq!(schemas.models["air-quality"]["linkml"], "id: air");
    assert!(bundle.spec.readme.as_deref().unwrap().contains("Two"));

    let serialized = bundle.to_yaml().expect("serialize");
    assert_eq!(bundle, Bundle::from_yaml(&serialized).expect("re-import"));
}

#[test]
fn a_bundle_without_a_readme_serialises_without_the_member() {
    let bundle = Bundle::from_yaml(GOLDEN_BUNDLE).expect("valid golden YAML");
    let serialized = bundle.to_yaml().expect("serialize");
    assert!(!serialized.contains("readme"), "{serialized}");
    assert!(!serialized.contains("schemas"), "{serialized}");
}

/// A reference to the secret `name`, as a webhook block carries it.
fn hook(name: &str) -> WebhookAuth {
    WebhookAuth {
        secret_ref: SecretRef {
            name: name.to_owned(),
            key: None,
            env_var: None,
        },
        previous_secret_ref: None,
    }
}

/// MF-44: the webhook route authorises a run against the source's own secret, so the schedule and
/// the credential come together — a webhook schedule with no secret is a door with no lock, and a
/// secret with no webhook schedule is a lock on no door. Both are refused where they are written,
/// rather than found later as a source that never runs or a credential nobody uses.
#[test]
fn a_webhook_schedule_and_its_own_secret_come_together() {
    let mut driven = sync_with_schedule(Schedule::webhook());
    let err = driven
        .spec
        .validate()
        .expect_err("a webhook schedule with no secret is refused");
    assert!(
        matches!(&err, Error::Invalid { field, .. } if field == "spec.webhook"),
        "{err}"
    );
    assert!(
        err.to_string().contains("secretRef"),
        "the refusal does not say what to write: {err}"
    );

    driven.spec.webhook = Some(hook("region-hook"));
    driven.spec.validate().expect("the pair validates");

    // The retiring secret is the rotation window and is validated the same way.
    let mut rotating = driven.clone();
    rotating.spec.webhook = Some(WebhookAuth {
        previous_secret_ref: Some(SecretRef {
            name: "region-hook-old".to_owned(),
            key: None,
            env_var: None,
        }),
        ..hook("region-hook")
    });
    rotating
        .spec
        .validate()
        .expect("a rotation window validates");

    // A polled source carries no webhook block: it is not reachable through that route at all.
    let mut polled = sync_with_schedule(Schedule::interval("30m"));
    polled.spec.webhook = Some(hook("region-hook"));
    let err = polled
        .spec
        .validate()
        .expect_err("a secret on a polled source is refused");
    assert!(
        matches!(&err, Error::Invalid { field, .. } if field == "spec.webhook"),
        "{err}"
    );
    assert!(sync_with_schedule(Schedule::interval("30m"))
        .spec
        .validate()
        .is_ok());
}

/// MF-24, T-2238: the webhook secret is named, never written. A token pasted into the name box is
/// refused by the same check every other `secretRef` gets, and the refusal never repeats it.
#[test]
fn a_credential_pasted_into_the_webhook_block_is_refused_without_being_repeated() {
    for (block, field) in [
        (
            hook("glpat-AAAAAAAAAAAAAAAAAAAA"),
            "spec.webhook.secretRef.name",
        ),
        (
            WebhookAuth {
                previous_secret_ref: Some(SecretRef {
                    name: "xoxb-1111111111-2222222222".to_owned(),
                    key: None,
                    env_var: None,
                }),
                ..hook("region-hook")
            },
            "spec.webhook.previousSecretRef.name",
        ),
    ] {
        let pasted = block
            .previous_secret_ref
            .as_ref()
            .map_or_else(|| block.secret_ref.name.clone(), |r| r.name.clone());
        let mut driven = sync_with_schedule(Schedule::webhook());
        driven.spec.webhook = Some(block);
        let err = driven
            .spec
            .validate()
            .expect_err("a credential in the name box is refused");
        assert!(
            matches!(&err, Error::Invalid { field: f, .. } if f == field),
            "{err}"
        );
        assert!(
            !err.to_string().contains(&pasted),
            "the refusal repeats what was pasted: {err}"
        );
    }
}

/// The whole manifest a webhook-driven source is written as, from YAML and back (MF-44).
#[test]
fn a_webhook_driven_source_roundtrips_through_yaml() {
    let yaml = GOLDEN_SYNC
        .replace("schedule: { interval: 30m }", "schedule: { webhook: true }")
        .replace(
            "  mode: mirror\n",
            "  webhook:\n    secretRef: { name: region-hook }\n    previousSecretRef: { name: region-hook-old }\n  mode: mirror\n",
        );
    let sync = SyncSource::from_yaml(&yaml).expect("valid YAML");
    sync.validate().expect("validates");
    let block = sync.spec.webhook.as_ref().expect("a webhook block");
    assert_eq!(block.secret_ref.name, "region-hook");
    assert_eq!(
        block
            .previous_secret_ref
            .as_ref()
            .expect("the retiring one")
            .name,
        "region-hook-old"
    );

    let serialized = sync.to_yaml().expect("serialize");
    assert_eq!(sync, SyncSource::from_yaml(&serialized).expect("re-import"));
    // A polled source serializes without the block at all, so nothing new appears in every file.
    let polled = SyncSource::from_yaml(GOLDEN_SYNC).expect("valid YAML");
    assert!(
        !polled.to_yaml().expect("serialize").contains("webhook"),
        "a polled source grew a webhook member"
    );
}
