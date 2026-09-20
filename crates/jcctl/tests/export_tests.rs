//! `jcctl export`: a live space copied out as the repository that would produce it
//! (T-0133, CC-22, MF-16, MF-17, PF-20).

mod common;

use common::*;
use jcctl::commands::{export, validate};
use jcctl::loader::RawManifest;
use jcctl::platform::{InMemory, Platform};

/// The same endpoint the platform would hand back: the declaration, plus the metadata the
/// reconciler keeps beside it.
const LIVE_ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
  uid: 7c9f0f5a-2b41-4b6e-9f10-2f7a1c3e5d80
  resourceVersion: "418"
  generation: 3
  creationTimestamp: "2026-09-01T08:00:00Z"
  annotations:
    joinedcontext.com/revision: 3f9c2e1
    joinedcontext.com/last-applied: "2026-09-05T22:10:00Z"
    banskabystrica.sk/owner: odbor-zivotneho-prostredia
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld", "geojson"]
"#;

/// A space somebody else's project owns, which must not come out with ours.
const OTHER_SPACE_ENDPOINT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: doprava-public
  namespace: ovzdusie
spec:
  contextSpaceRef: doprava
  slug: hj3wq8mzt6vhx3nbwrs5cjd8fk
  audience: public
  enabledRepresentations: ["ngsi-ld"]
"#;

const PIPELINE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Pipeline
metadata:
  name: aq
  namespace: ovzdusie
spec:
  class: resident
  source: { dataSourceRef: { kind: DataSource, name: mqtt-mesto } }
  compute: { kind: bloblang }
  targetEndpoint: "urn:ngsi-ld:Endpoint:banskabystrica.sk:ovzdusie:public-air"
"#;

/// The pipeline's own logic, in Bento's format: ours only to carry (MF-17).
const BENTO: &str = "input:\n  mqtt:\n    urls: [ mqtts://mqtt.mesto.sk:8883 ]\n";

fn live(manifests: &[&str]) -> InMemory {
    let mut platform = InMemory::new();
    for yaml in manifests {
        platform.put(&manifest(yaml)).expect("the platform accepts");
    }
    platform
}

fn exported<'a>(report: &'a export::Report, path: &str) -> &'a RawManifest {
    &report
        .resources
        .iter()
        .find(|resource| resource.path.to_str() == Some(path))
        .unwrap_or_else(|| panic!("{path} was exported"))
        .manifest
}

/// MF-16, MF-17 and T-0824: what `jcctl export` writes is the project of the checkout — every
/// manifest at the path it had, the native files beside them, and an index the platform's own
/// registry accepts.
#[test]
fn a_project_of_the_repository_is_written_out_as_a_bundle_that_validates() {
    let repo = demo_repo("export-bundle-repo");
    common::write(
        &repo,
        "projects/ovzdusie/pipelines/aq/pipeline.yaml",
        PIPELINE,
    );
    common::write(&repo, "projects/ovzdusie/pipelines/aq/bento.yaml", BENTO);

    let bundle = export::collect_project(&repo, "ovzdusie").expect("the checkout is read");
    let paths: Vec<String> = bundle
        .resources
        .iter()
        .map(|resource| resource.path.to_string_lossy().into_owned())
        .collect();
    assert!(
        paths.contains(&"projects/ovzdusie/pipelines/aq/pipeline.yaml".to_owned()),
        "{paths:?}"
    );
    let natives: Vec<String> = bundle
        .natives
        .iter()
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        natives,
        vec!["projects/ovzdusie/pipelines/aq/bento.yaml".to_owned()],
        "the pipeline's own logic travels with it (MF-17)"
    );

    let out = temp_dir("export-bundle-out");
    let written = export::write_bundle(&out, "ovzdusie", "3f9c2e1", "digitalizacia", &bundle)
        .expect("written");
    assert_eq!(written, bundle.resources.len() + bundle.natives.len() + 1);
    assert_eq!(
        std::fs::read_to_string(out.join("projects/ovzdusie/pipelines/aq/bento.yaml"))
            .expect("the native file is in the bundle"),
        BENTO,
        "byte for byte"
    );

    // The index is the platform's own kind, so what the CLI wrote is what it accepts back.
    let index = std::fs::read_to_string(out.join("bundle.yaml")).expect("the index");
    assert_eq!(
        jc_core::registry::validate_yaml("Bundle", &index),
        Some(Ok(())),
        "{index}"
    );
    assert!(index.contains("sourceRevision: 3f9c2e1"), "{index}");

    // MF-42: one checksum per file, over the bytes the bundle carries, so a transfer between
    // instances is verified before anyone deletes the source.
    let parsed: serde_json::Value = serde_norway::from_str(&index).expect("the index parses");
    let files = parsed["spec"]["files"].as_array().expect("files");
    assert_eq!(
        files.len(),
        bundle.resources.len() + bundle.natives.len(),
        "{index}"
    );
    let bento = files
        .iter()
        .find(|file| file["path"] == "projects/ovzdusie/pipelines/aq/bento.yaml")
        .expect("the native file is listed");
    assert_eq!(
        bento["sha256"].as_str().unwrap_or_default(),
        export::sha256_of(BENTO.as_bytes()),
        "the checksum is of the bytes as written"
    );

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&out);
}

/// MF-17: a literal credential in the checkout is dropped on the way out and named, the same
/// rule the live read follows.
#[test]
fn a_bundle_of_the_repository_carries_no_literal_credential() {
    let repo = demo_repo("export-bundle-secret");
    common::write(
        &repo,
        "projects/ovzdusie/datasources/mqtt.yaml",
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSource
metadata:
  name: mqtt-mesto
  namespace: ovzdusie
spec:
  kind: mqtt
  mqtt:
    urls: ["mqtts://mqtt.mesto.sk:8883"]
    topics: ["air/#"]
  password: "hunter2"
"#,
    );

    let bundle = export::collect_project(&repo, "ovzdusie").expect("the checkout is read");
    let rendered = format!("{:?}", bundle.resources);
    assert!(!rendered.contains("hunter2"), "{rendered}");
    assert_eq!(bundle.redactions, vec!["DataSource/mqtt-mesto: password"]);

    let _ = std::fs::remove_dir_all(&repo);
}

/// A project the checkout does not hold is nothing to export, not an empty bundle with an
/// index nobody can import.
#[test]
fn a_project_the_checkout_does_not_hold_writes_no_index() {
    let repo = demo_repo("export-bundle-missing");
    let bundle = export::collect_project(&repo, "doprava").expect("the checkout is read");
    assert!(bundle.resources.is_empty() && bundle.natives.is_empty());

    let out = temp_dir("export-bundle-missing-out");
    assert_eq!(
        export::write_bundle(&out, "doprava", "3f9c2e1", "digitalizacia", &bundle)
            .expect("written"),
        0
    );
    assert!(!out.join("bundle.yaml").exists());

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&out);
}

/// MF-16 and MF-04: an export is a declaration, not a snapshot. The platform's own
/// bookkeeping does not come out with it, and the operator's annotation does.
#[test]
fn system_metadata_is_stripped_and_hand_written_metadata_survives() {
    let platform = live(&[SPACE, LIVE_ENDPOINT]);
    let report = export::collect(&platform, "ovzdusie", "ovzdusie").expect("the platform answers");
    let endpoint = exported(
        &report,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/public-air.yaml",
    );

    for owned in ["uid", "resourceVersion", "generation", "creationTimestamp"] {
        assert!(
            !endpoint.metadata.rest.contains_key(owned),
            "{owned} is the platform's, not the declaration's"
        );
    }
    let annotations = endpoint.metadata.rest["annotations"]
        .as_object()
        .expect("annotations");
    assert!(!annotations.contains_key("joinedcontext.com/revision"));
    assert!(!annotations.contains_key("joinedcontext.com/last-applied"));
    assert_eq!(
        annotations["banskabystrica.sk/owner"],
        serde_json::json!("odbor-zivotneho-prostredia"),
        "an annotation somebody wrote by hand is theirs"
    );

    assert_eq!(endpoint.metadata.name, "public-air");
    assert_eq!(
        endpoint.spec["slug"],
        serde_json::json!("zt4qm7ge2xdv6ksb3ncf5arw2y")
    );
}

/// UI-50: a live manifest that still carries the legacy language map comes out with the one
/// plain string, so the repository converts itself on the next export.
#[test]
fn a_legacy_title_map_is_written_as_one_string() {
    let titled = LIVE_ENDPOINT.replacen(
        "  uid:",
        "  title: { sk: Ovzdušie, en: Air quality }\n  description: { sk: Merania }\n  uid:",
        1,
    );
    let platform = live(&[SPACE, &titled]);
    let report = export::collect(&platform, "ovzdusie", "ovzdusie").expect("the platform answers");
    let endpoint = exported(
        &report,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/public-air.yaml",
    );
    assert_eq!(
        endpoint.metadata.rest["title"],
        serde_json::json!("Air quality")
    );
    assert_eq!(
        endpoint.metadata.rest["description"],
        serde_json::json!("Merania"),
        "without en, the first non-empty value"
    );
}

/// MF-17 and MF-24: a manifest carries a `secretRef`, never a secret. A platform that
/// hands one back has it dropped and the operator is told, because a silent lossy export
/// is worse than a loud one.
#[test]
fn a_literal_credential_is_dropped_and_reported_while_its_reference_survives() {
    let mut spec = serde_json::json!({
        "contextSpaceRef": "ovzdusie",
        "slug": "zt4qm7ge2xdv6ksb3ncf5arw2y",
        "audience": "public",
        "password": "hunter2",
        "cachedTokenSecretRef": { "name": "gateway-token", "key": "token" },
        "tokenEndpoint": "https://keycloak.example.sk/realms/bb/protocol/openid-connect/token",
        "upstream": { "apiKey": "ak_live_9c1e", "url": "https://broker.internal" },
        "credentials": [{ "kind": "apiKey", "secretRef": { "name": "writer" } }],
    });
    let mut endpoint = manifest(LIVE_ENDPOINT);
    std::mem::swap(&mut endpoint.spec, &mut spec);

    let mut platform = live(&[SPACE]);
    platform.put(&endpoint).expect("the platform accepts");
    let report = export::collect(&platform, "ovzdusie", "ovzdusie").expect("the platform answers");
    let exported_endpoint = exported(
        &report,
        "projects/ovzdusie/spaces/ovzdusie/endpoints/public-air.yaml",
    );

    assert!(!exported_endpoint.spec.to_string().contains("hunter2"));
    assert!(!exported_endpoint.spec.to_string().contains("ak_live_9c1e"));
    assert_eq!(
        report.redactions,
        vec![
            "Endpoint/public-air: password",
            "Endpoint/public-air: apiKey"
        ],
        "every drop is named, so nobody discovers the loss in a diff"
    );

    // A reference is not a secret, and neither is a URL that happens to say `token`.
    assert_eq!(
        exported_endpoint.spec["cachedTokenSecretRef"]["name"],
        serde_json::json!("gateway-token")
    );
    assert!(exported_endpoint.spec["tokenEndpoint"].is_string());
    assert_eq!(
        exported_endpoint.spec["credentials"][0]["secretRef"]["name"],
        serde_json::json!("writer"),
        "a list of references is configuration, not a credential"
    );
}

/// CC-22: `export` copies one space out. Another space's endpoint and a resource that
/// belongs to no space at all stay where they are.
#[test]
fn only_the_named_space_comes_out() {
    let platform = live(&[ORG, PROJECT, SPACE, LIVE_ENDPOINT, OTHER_SPACE_ENDPOINT]);
    let report = export::collect(&platform, "ovzdusie", "ovzdusie").expect("the platform answers");

    let rendered = format!("{:?}", report.resources);
    assert!(
        !rendered.contains("doprava"),
        "another space came out: {rendered}"
    );
    assert!(
        !rendered.contains("Organization") && !rendered.contains("kind: \"Project\""),
        "an installation-level resource is not part of a space"
    );
    assert_eq!(report.resources.len(), 2, "the space and its one endpoint");
}

/// A fresh installation exports an empty repository rather than failing: `export` on
/// nothing is nothing, which is what `jcctl export` prints today.
#[test]
fn an_empty_platform_exports_an_empty_repository() {
    let report = export::collect(&InMemory::new(), "ovzdusie", "ovzdusie").expect("answers");
    assert!(report.resources.is_empty());
    assert!(report.redactions.is_empty());

    let dir = temp_dir("export-empty");
    assert_eq!(export::write(&dir, &report).expect("written"), 0);
    assert!(validate::run(&dir).is_valid());
    let _ = std::fs::remove_dir_all(&dir);
}

/// CC-08: no `jcctl` command reads a file outside the directory it was given.
///
/// `walk` used `std::fs::metadata`, which follows a link, so a link planted in a checkout put the
/// target's bytes in the archive as a native file — the loader has refused this all along, and now
/// the export walks with the loader's own rule (T-1478).
#[test]
fn export_refuses_a_link_out_of_the_checkout() {
    let repo = demo_repo("export-link-escape");
    let outside = temp_dir("export-link-escape-outside");
    std::fs::write(
        outside.join("stolen.txt"),
        "a secret from outside the checkout",
    )
    .expect("write the file the link points at");

    let inside = repo.join("projects/ovzdusie/pipelines/aq");
    std::fs::create_dir_all(&inside).expect("create the pipeline directory");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("stolen.txt"), inside.join("bento.yaml"))
        .expect("plant the link");

    let refused = export::collect_project(&repo, "ovzdusie")
        .expect_err("a link out of the checkout is refused, not read");
    let said = refused.to_string();
    assert!(
        said.contains("escapes repository root"),
        "the refusal says why: {said}"
    );
    assert!(
        !said.contains("a secret from outside"),
        "the refusal never carries what it refused: {said}"
    );
}

/// The link that stays inside is the one a ConfigMap or Secret volume is made of, so it still works.
#[test]
fn export_reads_a_link_that_stays_inside_the_checkout() {
    let repo = demo_repo("export-link-inside");
    common::write(
        &repo,
        "projects/ovzdusie/pipelines/aq/pipeline.yaml",
        PIPELINE,
    );
    common::write(&repo, "projects/ovzdusie/.real/bento.yaml", BENTO);
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        repo.join("projects/ovzdusie/.real/bento.yaml"),
        repo.join("projects/ovzdusie/pipelines/aq/bento.yaml"),
    )
    .expect("plant the link");

    let bundle = export::collect_project(&repo, "ovzdusie").expect("the checkout is read");
    let natives: Vec<String> = bundle
        .natives
        .iter()
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();
    assert!(
        natives.contains(&"projects/ovzdusie/pipelines/aq/bento.yaml".to_owned()),
        "{natives:?}"
    );
}
