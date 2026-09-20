//! T-0303: the `ContextSourceRegistration` manifest (MF-36, PF-48, SP-09).
//!
//! Federation is the one place where a manifest changes where an answer can come from, so the
//! things worth a test here are the refusals: a registration that says nothing about where the
//! data is, one that says two things, one that claims no coverage and would federate silently,
//! and an identity that has nothing to forward as.

use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{ContextSourceRegistrationSpec, FederationIdentity, RegistrationMode};
use jc_core::registry;

type Registration = ResourceEnvelope<ContextSourceRegistrationSpec>;

const LOCAL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSourceRegistration
metadata:
  name: transport
  namespace: helsinki
  title: { fi: "Liikenne", en: "Transport" }
spec:
  contextSpaceRef: hub
  endpointRef: { kind: Endpoint, name: transport-internal }
  information:
    - entities:
        - type: Vehicle
      propertyNames: [location, speed, occupancy]
  federation:
    identity: serviceAccount
    serviceAccountRef: { kind: ServiceAccount, name: hub-reader }
"#;

const EXTERNAL: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ContextSourceRegistration
metadata:
  name: regional-air
  namespace: helsinki
spec:
  contextSpaceRef: hub
  endpoint: https://regional.example/ngsi-ld/v1
  mode: exclusive
  expiresAt: '2027-01-01T00:00:00Z'
  information:
    - entities:
        - type: AirQualityObserved
  federation:
    identity: caller
"#;

#[test]
fn a_registration_on_this_platform_round_trips() {
    let parsed = Registration::from_yaml(LOCAL).expect("parses");
    parsed.validate().expect("valid");
    assert_eq!(parsed.spec.context_space_ref.name(), "hub");
    assert_eq!(
        parsed.spec.endpoint_ref.as_ref().map(|r| r.name()),
        Some("transport-internal")
    );
    assert_eq!(
        parsed.spec.federation.identity,
        FederationIdentity::ServiceAccount
    );
    // The default is the merging one: a source that silently became the only answer for what
    // it covers would hide the broker's own data without the manifest saying so.
    assert_eq!(parsed.spec.mode, RegistrationMode::Inclusive);

    let again = Registration::from_yaml(&serde_norway::to_string(&parsed).expect("serialize"))
        .expect("re-parses");
    assert_eq!(parsed, again);
}

#[test]
fn a_registration_elsewhere_round_trips() {
    let parsed = Registration::from_yaml(EXTERNAL).expect("parses");
    parsed.validate().expect("valid");
    assert_eq!(
        parsed.spec.endpoint.as_deref(),
        Some("https://regional.example/ngsi-ld/v1")
    );
    assert_eq!(parsed.spec.mode, RegistrationMode::Exclusive);
    assert!(parsed.spec.expires_at.is_some());
}

#[test]
fn the_source_is_named_once_and_only_once() {
    let neither = LOCAL.replace(
        "  endpointRef: { kind: Endpoint, name: transport-internal }\n",
        "",
    );
    assert!(
        Registration::from_yaml(&neither)
            .unwrap()
            .validate()
            .is_err(),
        "a registration that does not say where the data is federates nothing"
    );

    let both = LOCAL.replace(
        "  endpointRef: { kind: Endpoint, name: transport-internal }",
        "  endpointRef: { kind: Endpoint, name: transport-internal }\n  endpoint: https://elsewhere.example/ngsi-ld/v1",
    );
    assert!(
        Registration::from_yaml(&both).unwrap().validate().is_err(),
        "two targets means the reconciler would have to choose one"
    );
}

#[test]
fn a_claim_of_nothing_is_refused() {
    let empty = LOCAL.replace(
        "  information:\n    - entities:\n        - type: Vehicle\n      propertyNames: [location, speed, occupancy]\n",
        "  information: []\n",
    );
    assert!(Registration::from_yaml(&empty).unwrap().validate().is_err());

    let no_entities = LOCAL.replace(
        "    - entities:\n        - type: Vehicle\n      propertyNames: [location, speed, occupancy]",
        "    - entities: []",
    );
    assert!(
        Registration::from_yaml(&no_entities)
            .unwrap()
            .validate()
            .is_err(),
        "an information entry with no entity selector matches nothing"
    );
}

/// PF-48: neither identity is a grant, and neither may be arrived at by accident.
#[test]
fn an_identity_has_to_be_one_the_forward_can_actually_use() {
    let no_account = LOCAL.replace(
        "\n    serviceAccountRef: { kind: ServiceAccount, name: hub-reader }",
        "",
    );
    assert!(
        Registration::from_yaml(&no_account)
            .unwrap()
            .validate()
            .is_err(),
        "serviceAccount identity with no account would fall back to something nobody wrote"
    );

    let caller_with_account = LOCAL.replace("identity: serviceAccount", "identity: caller");
    assert!(
        Registration::from_yaml(&caller_with_account)
            .unwrap()
            .validate()
            .is_err(),
        "an account that would never be used reads as a grant that is not there"
    );

    let wrong_kind = LOCAL.replace(
        "kind: ServiceAccount, name: hub-reader",
        "kind: Policy, name: hub-reader",
    );
    assert!(Registration::from_yaml(&wrong_kind)
        .unwrap()
        .validate()
        .is_err());
}

/// PF-48: a hub reads its members' tenants directly, so registering one is what exposes it to
/// the hub's audience. That act stays inside the steward's own project.
#[test]
fn a_member_of_another_project_is_refused() {
    for (field, yaml) in [
        (
            "spec.endpointRef.namespace",
            LOCAL.replace(
                "endpointRef: { kind: Endpoint, name: transport-internal }",
                "endpointRef: { kind: Endpoint, name: transport-internal, namespace: espoo }",
            ),
        ),
        (
            "spec.contextSpaceRef.namespace",
            LOCAL.replace(
                "contextSpaceRef: hub",
                "contextSpaceRef: { kind: ContextSpace, name: hub, namespace: espoo }",
            ),
        ),
    ] {
        let parsed = Registration::from_yaml(&yaml).expect("parses");
        let refused = parsed.validate().expect_err("another project is refused");
        let said = refused.to_string();
        assert!(said.contains(field), "{field}: {said}");
        assert!(said.contains("espoo"), "{field}: {said}");
    }

    // The project's own namespace, spelled out, is the same project and stays valid.
    let same = LOCAL.replace(
        "endpointRef: { kind: Endpoint, name: transport-internal }",
        "endpointRef: { kind: Endpoint, name: transport-internal, namespace: helsinki }",
    );
    Registration::from_yaml(&same)
        .expect("parses")
        .validate()
        .expect("the hub's own project is not a crossing");
}

#[test]
fn an_external_source_is_a_url() {
    let not_a_url = EXTERNAL.replace("https://regional.example/ngsi-ld/v1", "regional.example");
    assert!(Registration::from_yaml(&not_a_url)
        .unwrap()
        .validate()
        .is_err());
}

#[test]
fn a_manifest_field_nobody_declared_is_refused() {
    let extra = LOCAL.replace(
        "  contextSpaceRef: hub",
        "  contextSpaceRef: hub\n  tenant: helsinki",
    );
    assert!(
        Registration::from_yaml(&extra).is_err(),
        "the tenant is the platform's to set, never a manifest's (SP-08, SP-09)"
    );
}

#[test]
fn the_kind_is_in_the_catalogue_with_its_repository_path() {
    let info = registry::by_kind("ContextSourceRegistration").expect("registered");
    assert_eq!(info.plural, "csrs");
    assert_eq!(
        info.repo_path("helsinki", "hub", "transport"),
        "projects/helsinki/spaces/hub/registrations/transport.yaml"
    );
    assert_eq!(
        registry::by_plural("csrs").map(|k| k.kind),
        Some("ContextSourceRegistration")
    );
    assert!(registry::validate_yaml("ContextSourceRegistration", LOCAL)
        .expect("known kind")
        .is_ok());
    let schema = registry::schema_of("ContextSourceRegistration").expect("schema");
    assert_eq!(
        schema.get("$schema").and_then(|v| v.as_str()),
        Some("http://json-schema.org/draft-07/schema#")
    );
}
