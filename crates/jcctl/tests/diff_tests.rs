use jcctl::diff::{diff, FieldDiff};
use jcctl::loader::RawManifest;
use serde_json::json;

fn manifest(yaml: &str) -> RawManifest {
    serde_norway::from_str(yaml).expect("manifest parses")
}

const DECLARED: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: public-air
  namespace: ovzdusie
  labels:
    joinedcontext.com/domain: environment
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  enabledRepresentations: ["ngsi-ld", "geojson"]
  rateLimits:
    requestsPerMinute: 600
    burst: 50
"#;

#[test]
fn a_live_resource_that_matches_the_manifest_has_no_diff() {
    let declared = manifest(DECLARED);
    assert!(diff(&declared, &declared.clone()).is_empty());
}

/// The whole point of a structural diff: the same manifest written in another order, or
/// with a set-valued list reordered, is not a change (CC-17).
#[test]
fn reordered_members_and_reordered_lists_are_not_a_change() {
    let live = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  namespace: ovzdusie
  name: public-air
  labels:
    joinedcontext.com/domain: environment
spec:
  audience: public
  rateLimits:
    burst: 50
    requestsPerMinute: 600
  enabledRepresentations: ["geojson", "ngsi-ld"]
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  contextSpaceRef: ovzdusie
"#,
    );

    assert_eq!(diff(&manifest(DECLARED), &live), vec![]);
}

#[test]
fn a_modified_member_is_reported_with_its_path_and_both_values() {
    let live = manifest(&DECLARED.replace("burst: 50", "burst: 5"));

    assert_eq!(
        diff(&manifest(DECLARED), &live),
        vec![FieldDiff {
            path: "spec.rateLimits.burst".to_owned(),
            declared: Some(json!(50)),
            live: Some(json!(5)),
        }]
    );
}

#[test]
fn a_member_the_live_resource_does_not_have_yet_is_reported_as_missing() {
    let live = manifest(&DECLARED.replace("    burst: 50\n", ""));

    assert_eq!(
        diff(&manifest(DECLARED), &live),
        vec![FieldDiff {
            path: "spec.rateLimits.burst".to_owned(),
            declared: Some(json!(50)),
            live: None,
        }]
    );
}

/// Server-computed members are stripped from both sides, so a live resource carrying a
/// status block and timestamps still matches the manifest (CC-17).
#[test]
fn server_managed_members_are_never_a_change() {
    let live = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
  createdAt: "2026-09-05T10:00:00Z"
  modifiedAt: "2026-09-06T11:00:00Z"
spec:
  isSandbox: false
  status:
    phase: Live
"#,
    );
    let declared = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
  createdAt: "1970-01-01T00:00:00Z"
spec:
  isSandbox: false
  status:
    phase: Draft
"#,
    );

    assert!(diff(&declared, &live).is_empty());
}

/// CC-69: the manifest owns exactly what it declares, so an attribute a pipeline wrote on
/// the live resource is not drift.
#[test]
fn an_attribute_only_the_live_resource_carries_is_not_drift() {
    let declared = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
"#,
    );
    let live = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
  pm10: 34.2
"#,
    );

    assert!(diff(&declared, &live).is_empty());
}

/// Claiming an attribute in the annotation reverses that: dropping it from the manifest
/// now means the live value has to go (CC-69).
#[test]
fn a_claimed_attribute_missing_from_the_manifest_is_drift() {
    let declared = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
  annotations:
    joinedcontext.com/managed-attributes: "isSandbox,defaultLocale"
spec:
  isSandbox: false
"#,
    );
    let live = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  isSandbox: false
  defaultLocale: sk
  pm10: 34.2
"#,
    );

    assert_eq!(
        diff(&declared, &live),
        vec![FieldDiff {
            path: "spec.defaultLocale".to_owned(),
            declared: None,
            live: Some(json!("sk")),
        }]
    );
}

/// A multi-valued NGSI-LD attribute is keyed by `datasetId`, never by position (CC-17).
#[test]
fn multi_valued_attributes_are_keyed_by_dataset_id() {
    let declared = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  temperature:
    - datasetId: "urn:ngsi-ld:Dataset:indoor"
      value: 21
    - datasetId: "urn:ngsi-ld:Dataset:outdoor"
      value: 8
"#,
    );
    let live = manifest(
        r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSpace
metadata:
  name: ovzdusie
  namespace: ovzdusie
spec:
  temperature:
    - datasetId: "urn:ngsi-ld:Dataset:outdoor"
      value: 9
    - datasetId: "urn:ngsi-ld:Dataset:indoor"
      value: 21
"#,
    );

    assert_eq!(
        diff(&declared, &live),
        vec![FieldDiff {
            path: "spec.temperature[urn:ngsi-ld:Dataset:outdoor].value".to_owned(),
            declared: Some(json!(8)),
            live: Some(json!(9)),
        }]
    );
}
