//! T-0448: which URL a reference's mirror fetches, and when it is due (DM-48, DM-49).
//!
//! `mirror` is handed a base URL because only the reconciler can turn a reference into one.
//! This is that step, and it is where the three cases stop looking alike: a
//! `SharedSpaceReference` always names an Endpoint this instance serves, a registration may
//! name one of ours or a broker that is not ours at all and may publish no schema surface.
//! DM-48 is explicit that the last case is not an error — the reference stands without a
//! model — so it comes back as a sentence a reviewer reads, not as a failure.

use jcctl::foreign_models::{self, Surface};
use jcctl::loader::{RawManifest, Repository};
use std::path::{Path, PathBuf};

const HERE: &str = "https://portal.banskabystrica.sk";
const SLUG: &str = "zt4qm7ge2xdv6ksb3ncf5arw2y";

fn manifest(yaml: &str) -> RawManifest {
    serde_norway::from_str(yaml).expect("the manifest parses")
}

fn shared_reference() -> RawManifest {
    manifest(&format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: SharedSpaceReference
metadata:
  name: partner
  namespace: ovzdusie
spec:
  endpointSlug: {SLUG}
  alias: partner-air
"#
    ))
}

fn registration(where_clause: &str) -> RawManifest {
    manifest(&format!(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSourceRegistration
metadata:
  name: regional-hub
  namespace: doprava
spec:
  contextSpaceRef: hub
  {where_clause}
  federation: {{ identity: caller }}
  information:
    - entities: [{{ type: AirQualityObserved }}]
"#
    ))
}

/// A repository directory holding whatever the test writes into it.
fn repo_dir(test: &str) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("jcctl-surface-{test}-{now}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the repository");
    dir
}

fn write_endpoint(dir: &Path, project: &str, name: &str, slug: &str) {
    let path = dir.join(format!("projects/{project}/endpoints"));
    std::fs::create_dir_all(&path).expect("create the endpoint directory");
    std::fs::write(
        path.join(format!("{name}.yaml")),
        format!(
            r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: {name}
  namespace: {project}
spec:
  slug: {slug}
  contextSpaceRef: hub
  audience: organization
  representations: [ngsi-ld]
"#
        ),
    )
    .expect("write the endpoint");
}

fn write_registration(dir: &Path, name: &str, where_clause: &str) {
    let path = dir.join("projects/doprava/spaces/hub/registrations");
    std::fs::create_dir_all(&path).expect("create the registration directory");
    let mut yaml = registration(where_clause);
    yaml.metadata.name = name.to_owned();
    std::fs::write(
        path.join(format!("{name}.yaml")),
        serde_norway::to_string(&yaml).expect("the registration serialises"),
    )
    .expect("write the registration");
}

fn load(dir: &Path) -> Repository {
    Repository::load(dir).expect("the repository loads")
}

fn surface(reference: &RawManifest, repo: &Repository) -> Surface {
    foreign_models::surface_of(reference, HERE, repo).expect("the reference resolves")
}

#[test]
fn a_shared_space_reference_resolves_against_this_instance() {
    let dir = repo_dir("shared");
    assert_eq!(
        surface(&shared_reference(), &load(&dir)),
        Surface::At(format!("{HERE}/api/endpoint/{SLUG}"))
    );
}

#[test]
fn a_trailing_slash_on_our_own_url_does_not_double_up() {
    let dir = repo_dir("slash");
    let resolved =
        foreign_models::surface_of(&shared_reference(), &format!("{HERE}/"), &load(&dir))
            .expect("resolves");
    assert_eq!(resolved, Surface::At(format!("{HERE}/api/endpoint/{SLUG}")));
}

#[test]
fn a_plaintext_instance_url_is_refused_before_anything_is_fetched() {
    // Our own URL is checked exactly like a peer's: a plaintext instance would make every
    // local mirror an unauthenticated fetch over the wire.
    let dir = repo_dir("plain-self");
    let refused =
        foreign_models::surface_of(&shared_reference(), "http://portal.local", &load(&dir))
            .expect_err("http is not a schema surface");
    assert!(refused.to_string().contains("https"), "{refused}");
}

#[test]
fn a_registration_pointing_at_one_of_our_endpoints_resolves_through_the_repository() {
    let dir = repo_dir("endpointref");
    write_endpoint(&dir, "doprava", "transport-internal", SLUG);
    let reference = registration("endpointRef: { kind: Endpoint, name: transport-internal }");
    assert_eq!(
        surface(&reference, &load(&dir)),
        Surface::At(format!("{HERE}/api/endpoint/{SLUG}"))
    );
}

#[test]
fn an_endpoint_ref_the_repository_does_not_hold_is_flagged_not_guessed() {
    let dir = repo_dir("missing-endpoint");
    let reference = registration("endpointRef: { kind: Endpoint, name: transport-internal }");
    match surface(&reference, &load(&dir)) {
        Surface::None(reason) => {
            assert!(reason.contains("transport-internal"), "{reason}");
            assert!(reason.contains("DM-48"), "{reason}");
        }
        other => panic!("expected a flag, got {other:?}"),
    }
}

#[test]
fn an_endpoint_of_another_project_is_not_borrowed() {
    // Resolution is scoped to the registration's own project. Finding a same-named Endpoint
    // in someone else's project and mirroring it would cross a project boundary silently.
    let dir = repo_dir("other-project");
    write_endpoint(&dir, "ovzdusie", "transport-internal", SLUG);
    let reference = registration("endpointRef: { kind: Endpoint, name: transport-internal }");
    assert!(matches!(surface(&reference, &load(&dir)), Surface::None(_)));
}

#[test]
fn a_broker_elsewhere_loses_its_ngsi_ld_suffix() {
    let dir = repo_dir("remote");
    let reference = registration("endpoint: https://other.example/ngsi-ld/v1");
    assert_eq!(
        surface(&reference, &load(&dir)),
        Surface::At("https://other.example".to_owned())
    );
}

#[test]
fn a_plaintext_broker_is_flagged_rather_than_fetched() {
    // jc-core allows http on `endpoint`, because a member broker on the mesh is reached that
    // way. A mirror crosses an organisation boundary, so it declines instead.
    let dir = repo_dir("plain-peer");
    let reference = registration("endpoint: http://broker.svc.cluster.local/ngsi-ld/v1");
    match surface(&reference, &load(&dir)) {
        Surface::None(reason) => assert!(reason.contains("not https"), "{reason}"),
        other => panic!("expected a flag, got {other:?}"),
    }
}

#[test]
fn a_broker_whose_url_is_not_an_ngsi_ld_base_is_flagged() {
    let dir = repo_dir("odd-peer");
    let reference = registration("endpoint: https://other.example/broker/v2");
    match surface(&reference, &load(&dir)) {
        Surface::None(reason) => {
            assert!(reason.contains("/ngsi-ld/v1"), "{reason}");
            assert!(reason.contains("no model is mirrored"), "{reason}");
        }
        other => panic!("expected a flag, got {other:?}"),
    }
}

#[test]
fn a_manifest_that_is_not_a_reference_is_an_error_not_a_flag() {
    let dir = repo_dir("not-a-reference");
    let other = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: hub
  namespace: doprava
spec: {}
"#,
    );
    assert!(foreign_models::surface_of(&other, HERE, &load(&dir)).is_err());
}

#[test]
fn the_plan_flags_the_peers_it_cannot_read_and_stays_quiet_about_the_rest() {
    let dir = repo_dir("plan-flags");
    write_endpoint(&dir, "doprava", "transport-internal", SLUG);
    write_registration(
        &dir,
        "ours",
        "endpointRef: { kind: Endpoint, name: transport-internal }",
    );
    write_registration(
        &dir,
        "good-peer",
        "endpoint: https://other.example/ngsi-ld/v1",
    );
    write_registration(
        &dir,
        "plain-peer",
        "endpoint: http://other.example/ngsi-ld/v1",
    );
    write_registration(
        &dir,
        "odd-peer",
        "endpoint: https://other.example/broker/v2",
    );

    let flags = foreign_models::plan_flags(&load(&dir));
    assert_eq!(flags.len(), 2, "{flags:#?}");
    assert!(flags
        .iter()
        .any(|f| f.contains("plain-peer") && f.contains("not https")));
    assert!(flags
        .iter()
        .any(|f| f.contains("odd-peer") && f.contains("/ngsi-ld/v1")));
    // The two that resolve say nothing: a plan that warns about every working reference is a
    // plan nobody reads.
    assert!(!flags.iter().any(|f| f.contains("good-peer")));
    assert!(!flags.iter().any(|f| f.contains("ours")));
}

#[test]
fn a_mirror_that_never_ran_is_due() {
    assert!(foreign_models::due(None, None, 0));
}

#[test]
fn the_default_interval_is_twenty_four_hours() {
    let day = 24 * 60 * 60;
    assert!(!foreign_models::due(None, Some(0), day - 1));
    assert!(foreign_models::due(None, Some(0), day));
}

#[test]
fn a_declared_interval_wins_over_the_default() {
    let six_hours = jc_core::kinds::Schedule::interval("6h");
    assert!(!foreign_models::due(
        Some(&six_hours),
        Some(0),
        6 * 3600 - 1
    ));
    assert!(foreign_models::due(Some(&six_hours), Some(0), 6 * 3600));
}

#[test]
fn a_clock_that_went_backwards_does_not_make_a_mirror_due() {
    // `saturating_sub`, so a corrected clock delays the next fetch instead of firing one on
    // every tick until the timestamps agree again.
    assert!(!foreign_models::due(None, Some(1_000_000), 0));
}
