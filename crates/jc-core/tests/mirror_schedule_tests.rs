//! T-0448: both reference kinds carry the schedule their foreign-model mirror runs on (DM-49).
//!
//! The interval vocabulary is `kind: SyncSource`'s, deliberately, so a repository has one way
//! of writing "every six hours" rather than two. What differs is which schedules make sense:
//! a peer in another organisation sends no webhook, so a webhook schedule here would be a
//! mirror that never runs and never says so.

use jc_core::envelope::ResourceEnvelope;
use jc_core::error::Error;
use jc_core::kinds::{ContextSourceRegistrationSpec, Schedule, SharedSpaceReference};

/// `kind: ContextSourceRegistration` as a whole manifest; the crate exports the spec only.
type ContextSourceRegistration = ResourceEnvelope<ContextSourceRegistrationSpec>;

const REFERENCE: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: SharedSpaceReference
metadata:
  name: partner
  namespace: ovzdusie
spec:
  endpointSlug: zt4qm7ge2xdv6ksb3ncf5arw2y
  alias: partner-air
"#;

const REGISTRATION: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSourceRegistration
metadata:
  name: regional-hub
  namespace: doprava
spec:
  contextSpaceRef: hub
  endpoint: https://other.example/ngsi-ld/v1
  federation: { identity: caller }
  information:
    - entities: [{ type: AirQualityObserved }]
"#;

fn reference_with(schedule: &str) -> Result<SharedSpaceReference, String> {
    let yaml = format!("{REFERENCE}  schedule: {schedule}\n");
    let manifest: SharedSpaceReference =
        serde_norway::from_str(&yaml).map_err(|err| err.to_string())?;
    manifest.validate().map_err(|err| err.to_string())?;
    Ok(manifest)
}

fn registration_with(schedule: &str) -> Result<ContextSourceRegistration, String> {
    let yaml = format!("{REGISTRATION}  schedule: {schedule}\n");
    let manifest: ContextSourceRegistration =
        serde_norway::from_str(&yaml).map_err(|err| err.to_string())?;
    manifest.validate().map_err(|err| err.to_string())?;
    Ok(manifest)
}

#[test]
fn a_reference_without_a_schedule_is_valid_and_means_the_default() {
    let manifest: SharedSpaceReference = serde_norway::from_str(REFERENCE).expect("parses");
    manifest.validate().expect("no schedule is the 24h default");
    assert!(manifest.spec.schedule.is_none());
    assert_eq!(jc_core::kinds::MIRROR_INTERVAL_DEFAULT_SECONDS, 86_400);
}

#[test]
fn both_reference_kinds_take_an_interval() {
    let reference = reference_with("{ interval: 6h }").expect("valid");
    assert_eq!(
        reference
            .spec
            .schedule
            .as_ref()
            .and_then(Schedule::interval_seconds),
        Some(6 * 60 * 60)
    );
    let registration = registration_with("{ interval: 30m }").expect("valid");
    assert_eq!(
        registration
            .spec
            .schedule
            .as_ref()
            .and_then(Schedule::interval_seconds),
        Some(30 * 60)
    );
}

#[test]
fn a_webhook_schedule_is_refused_on_a_reference() {
    // The peer is another organisation; it has no reason to call us and no way to. Accepting
    // this would be a mirror that silently never runs.
    let refused = reference_with("{ webhook: true }").expect_err("a peer sends no webhook");
    assert!(refused.contains("webhook"), "{refused}");
    assert!(refused.contains("interval"), "{refused}");
    let refused = registration_with("{ webhook: true }").expect_err("a peer sends no webhook");
    assert!(refused.contains("webhook"), "{refused}");
}

#[test]
fn an_interval_this_crate_cannot_parse_is_refused_rather_than_defaulted() {
    // Silently falling back to 24h would make `interval: 6 hours` look deliberate.
    let refused = reference_with("{ interval: 6 hours }").expect_err("not an interval");
    assert!(refused.contains("suffix"), "{refused}");
    let refused = reference_with("{ interval: fortnightly }").expect_err("not an interval");
    assert!(refused.contains("suffix"), "{refused}");
}

#[test]
fn an_empty_schedule_is_refused_because_it_never_fires() {
    let refused = reference_with("{}").expect_err("neither interval nor webhook");
    assert!(refused.contains("never fires"), "{refused}");
}

#[test]
fn the_field_is_rejected_when_misspelled() {
    // `deny_unknown_fields` is what keeps `schedul:` from being read as "no schedule".
    let yaml = format!("{REFERENCE}  schedul: {{ interval: 6h }}\n");
    let parsed = serde_norway::from_str::<SharedSpaceReference>(&yaml);
    assert!(parsed.is_err(), "a misspelled field must not be ignored");
}

#[test]
fn the_error_names_the_field_a_person_has_to_edit() {
    let manifest: SharedSpaceReference =
        serde_norway::from_str(&format!("{REFERENCE}  schedule: {{ webhook: true }}\n"))
            .expect("parses");
    match manifest.validate() {
        Err(Error::Name { field, .. }) => assert_eq!(field, "spec.schedule.webhook"),
        other => panic!("expected a named field, got {other:?}"),
    }
}
