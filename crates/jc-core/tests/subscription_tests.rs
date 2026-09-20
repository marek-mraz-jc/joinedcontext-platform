//! T-0913: the `Subscription` manifest (CC-72, DS-16, MF-31).
//!
//! A subscription is the one manifest that makes the platform call an address of the author's
//! choosing, so the tests here are about the two ways that goes wrong: a credential written into
//! the address instead of named as a `secretRef`, and a subscription that cannot fire at all —
//! one that watches nothing, or one whose expiry is already past.

use chrono::{TimeZone, Utc};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::SubscriptionSpec;
use jc_core::registry;

type Subscription = ResourceEnvelope<SubscriptionSpec>;

const GOLDEN: &str = include_str!("golden/027-Subscription-06-configuration-as-code.yaml");

/// A moment before the golden manifest's `expiresAt`, so the expiry check has something to
/// compare against without a clock (jc-core takes chrono without `clock` on purpose).
fn before_expiry() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 17, 12, 0, 0).unwrap()
}

#[test]
fn the_golden_manifest_parses_validates_and_round_trips() {
    let subscription = Subscription::from_yaml(GOLDEN).expect("the documented manifest parses");
    subscription
        .spec
        .validate_at(before_expiry())
        .expect("the documented manifest validates");

    assert_eq!(subscription.metadata.name, "air-quality-alerts");
    assert_eq!(subscription.spec.context_space_ref.name(), "air-quality");
    assert_eq!(subscription.spec.throttling, Some(60));
    assert!(subscription.spec.is_active);

    let again = Subscription::from_yaml(&subscription.to_yaml().expect("serialises"))
        .expect("the serialised manifest parses");
    assert_eq!(again, subscription);
}

#[test]
fn the_manifest_is_filed_under_the_space_it_watches() {
    let subscription = Subscription::from_yaml(GOLDEN).expect("parses");
    assert_eq!(
        subscription.resource_path().expect("resource path"),
        "projects/helsinki/spaces/air-quality/subscriptions/air-quality-alerts.yaml",
        "the space comes from contextSpaceRef, not from the project slug"
    );
}

#[test]
fn the_registry_routes_the_kind() {
    assert!(
        matches!(
            registry::validate_yaml("Subscription", GOLDEN),
            Some(Ok(()))
        ),
        "the registry validates a Subscription manifest"
    );
    assert_eq!(
        registry::by_plural("subscriptions")
            .expect("the plural resolves")
            .repo_path("helsinki", "air-quality", "air-quality-alerts"),
        "projects/helsinki/spaces/air-quality/subscriptions/air-quality-alerts.yaml",
        "the catalogue's path, so a manifest lands where Architecture/06 says"
    );
}

#[test]
fn a_credential_in_the_notification_endpoint_is_refused() {
    for uri in [
        "https://alerts.example.fi/hooks/air-quality?access_token=s3cr3t-value",
        "https://hook:s3cr3t-value@alerts.example.fi/hooks/air-quality",
    ] {
        let yaml = GOLDEN.replace(
            "uri: https://alerts.example.fi/hooks/air-quality",
            &format!("uri: \"{uri}\""),
        );
        let subscription = Subscription::from_yaml(&yaml).expect("parses");
        let error = subscription.spec.validate().expect_err(uri).to_string();
        assert!(error.contains("secretRef"), "{error}");
        assert!(
            !error.contains("s3cr3t-value"),
            "the refusal repeated the secret: {error}"
        );
    }
}

#[test]
fn a_credential_header_is_refused_even_when_the_url_is_clean() {
    let yaml = GOLDEN.replace(
        "        - { key: X-Space, value: air-quality }",
        "        - { key: Authorization, value: \"Bearer s3cr3t-value\" }",
    );
    let subscription = Subscription::from_yaml(&yaml).expect("parses");
    let error = subscription
        .spec
        .validate()
        .expect_err("a header credential")
        .to_string();
    assert!(error.contains("secretRef"), "{error}");
    assert!(
        !error.contains("s3cr3t-value"),
        "the refusal repeated the secret: {error}"
    );
}

#[test]
fn an_expiry_already_past_is_refused() {
    let subscription = Subscription::from_yaml(GOLDEN).expect("parses");
    let after = Utc.with_ymd_and_hms(2027, 6, 1, 0, 0, 0).unwrap();
    let error = subscription
        .spec
        .validate_at(after)
        .expect_err("an expired subscription")
        .to_string();
    assert!(error.contains("expiresAt"), "{error}");
}

#[test]
fn a_subscription_that_watches_nothing_is_refused() {
    let yaml = GOLDEN
        .replace("  entities:\n    - type: AirQualityObserved      # a type, an id or an idPattern; at least one of them\n", "")
        .replace("  watchedAttributes: [airQualityIndex]\n", "");
    let subscription = Subscription::from_yaml(&yaml).expect("parses");
    assert!(
        subscription.spec.validate().is_err(),
        "a subscription with no selector matches everything the broker stores"
    );
}

#[test]
fn an_entity_selector_names_something() {
    let yaml = GOLDEN.replace(
        "    - type: AirQualityObserved      # a type, an id or an idPattern; at least one of them",
        "    - {}",
    );
    let subscription = Subscription::from_yaml(&yaml).expect("parses");
    assert!(subscription.spec.validate().is_err(), "an empty selector");
}
