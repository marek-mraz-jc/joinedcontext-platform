use chrono::{DateTime, Utc};
use jc_core::envelope::{Ref, SecretRef};
use jc_core::error::Error;
use jc_core::kinds::dataspace::{AgreementRole, AgreementState, ConnectorEngine, Did};
use jc_core::kinds::policy::Validity;
use jc_core::kinds::{DataAgreement, DataOffer, DataSpaceParticipant};
use serde_json::json;

const GOLDEN_PARTICIPANT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataSpaceParticipant
metadata:
  name: participant
  namespace: org
  title:
    sk: "Dátový priestor mesta Banská Bystrica"
    en: "Banská Bystrica Data Space Participant"
spec:
  did: "did:web:banskabystrica.sk"
  credentialIssuer: "https://auth.banskabystrica.sk/realms/organization"
  connectorUrl: "https://connector.banskabystrica.sk"
  engine: rainbow
  trustAnchors:
    - "did:web:smartcity-trust.sk"
    - "did:web:gaia-x.europa.eu"
"#;

const GOLDEN_OFFER: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataOffer
metadata:
  name: air-quality-public-offer
  namespace: bb-ovzdusie
  title:
    sk: "Verejná ponuka dát o ovzduší"
    en: "Public air quality data offer"
spec:
  contextSpaceRef: ovzdusie
  endpointRefs:
    - kind: Endpoint
      name: air-quality-public
  policy:
    "@context": "http://www.w3.org/ns/odrl.jsonld"
    "@type": "Offer"
    permission:
      - target: "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:*"
        action: "read"
  containsPersonalData: false
"#;

const GOLDEN_AGREEMENT: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataAgreement
metadata:
  name: bratislava-air-sharing
  namespace: bb-ovzdusie
  title:
    sk: "Dohoda o zdieľaní dát s Bratislavou"
    en: "Data sharing agreement with Bratislava"
spec:
  role: provider
  offerRef:
    kind: DataOffer
    name: air-quality-public-offer
  remoteParticipant: "did:web:bratislava.sk"
  agreementId: "dsp-agr-2026-09-001"
  state: finalized
  validity:
    from: "2026-09-01T00:00:00Z"
    to: "2027-09-01T00:00:00Z"
  constraints:
    q: "pm10>=0"
    scopeQ: "/geo/SK/BB"
"#;

#[test]
fn golden_dataspace_participant_manifest_parses_validates_and_roundtrips() {
    let p = DataSpaceParticipant::from_yaml(GOLDEN_PARTICIPANT).expect("valid participant YAML");
    p.validate().expect("participant validates");

    assert_eq!(p.api_version, "joinedcontext.com/v1alpha1");
    assert_eq!(p.kind, "DataSpaceParticipant");
    assert_eq!(p.metadata.name, "participant");
    assert_eq!(p.metadata.namespace.as_deref(), Some("org"));
    assert_eq!(p.spec.did.as_str(), "did:web:banskabystrica.sk");
    assert_eq!(p.spec.did.org_domain(), "banskabystrica.sk");
    assert_eq!(
        p.spec.credential_issuer,
        "https://auth.banskabystrica.sk/realms/organization"
    );
    assert_eq!(p.spec.connector_url, "https://connector.banskabystrica.sk");
    assert_eq!(p.spec.engine, ConnectorEngine::Rainbow);
    assert_eq!(p.spec.trust_anchors.len(), 2);
    assert_eq!(
        p.spec.trust_anchors[0].as_str(),
        "did:web:smartcity-trust.sk"
    );
    assert_eq!(p.spec.trust_anchors[1].as_str(), "did:web:gaia-x.europa.eu");

    assert_eq!(
        p.resource_path().expect("resource path"),
        "dataspace/participant.yaml"
    );

    let serialized = p.to_yaml().expect("serialize to yaml");
    let reimported = DataSpaceParticipant::from_yaml(&serialized).expect("re-import yaml");
    assert_eq!(p, reimported);

    let json = p.to_json().expect("serialize to json");
    let reimported_json = DataSpaceParticipant::from_json(&json).expect("re-import json");
    assert_eq!(p, reimported_json);
}

#[test]
fn golden_data_offer_manifest_parses_validates_and_roundtrips() {
    let offer = DataOffer::from_yaml(GOLDEN_OFFER).expect("valid offer YAML");
    offer.validate().expect("offer validates");

    assert_eq!(offer.api_version, "joinedcontext.com/v1alpha1");
    assert_eq!(offer.kind, "DataOffer");
    assert_eq!(offer.metadata.name, "air-quality-public-offer");
    assert_eq!(offer.metadata.namespace.as_deref(), Some("bb-ovzdusie"));
    assert_eq!(offer.spec.context_space_ref, "ovzdusie");
    assert_eq!(offer.spec.endpoint_refs.len(), 1);
    assert_eq!(offer.spec.endpoint_refs[0].name(), "air-quality-public");
    assert!(!offer.spec.contains_personal_data);
    assert!(offer.spec.purpose.is_none());

    assert_eq!(
        offer.resource_path().expect("resource path"),
        "projects/bb-ovzdusie/spaces/ovzdusie/dataspace/offers/air-quality-public-offer.yaml"
    );

    let serialized = offer.to_yaml().expect("serialize to yaml");
    let reimported = DataOffer::from_yaml(&serialized).expect("re-import yaml");
    assert_eq!(offer, reimported);

    let json = offer.to_json().expect("serialize to json");
    let reimported_json = DataOffer::from_json(&json).expect("re-import json");
    assert_eq!(offer, reimported_json);
}

#[test]
fn golden_data_agreement_manifest_parses_validates_and_roundtrips() {
    let agr = DataAgreement::from_yaml(GOLDEN_AGREEMENT).expect("valid agreement YAML");
    agr.validate().expect("agreement validates");

    assert_eq!(agr.api_version, "joinedcontext.com/v1alpha1");
    assert_eq!(agr.kind, "DataAgreement");
    assert_eq!(agr.metadata.name, "bratislava-air-sharing");
    assert_eq!(agr.metadata.namespace.as_deref(), Some("bb-ovzdusie"));
    assert_eq!(agr.spec.role, AgreementRole::Provider);
    assert_eq!(
        agr.spec.offer_ref.as_ref().map(|r| r.name()),
        Some("air-quality-public-offer")
    );
    assert_eq!(
        agr.spec.remote_participant.as_str(),
        "did:web:bratislava.sk"
    );
    assert_eq!(agr.spec.remote_participant.org_domain(), "bratislava.sk");
    assert_eq!(agr.spec.agreement_id, "dsp-agr-2026-09-001");
    assert_eq!(agr.spec.state, AgreementState::Finalized);
    assert_eq!(agr.spec.constraints.q.as_deref(), Some("pm10>=0"));
    assert_eq!(agr.spec.constraints.scope_q.as_deref(), Some("/geo/SK/BB"));

    assert_eq!(
        agr.resource_path().expect("resource path"),
        "projects/bb-ovzdusie/dataspace/agreements/bratislava-air-sharing.yaml"
    );

    let serialized = agr.to_yaml().expect("serialize to yaml");
    let reimported = DataAgreement::from_yaml(&serialized).expect("re-import yaml");
    assert_eq!(agr, reimported);

    let json = agr.to_json().expect("serialize to json");
    let reimported_json = DataAgreement::from_json(&json).expect("re-import json");
    assert_eq!(agr, reimported_json);
}

#[test]
fn did_web_syntax_table_valid_and_invalid() {
    let valid_cases = [
        ("did:web:banskabystrica.sk", "banskabystrica.sk"),
        ("did:web:bratislava.sk", "bratislava.sk"),
        ("did:web:data.smartcity.sk", "data.smartcity.sk"),
        ("did:web:sub-domain.example.com", "sub-domain.example.com"),
    ];

    for (s, expected_domain) in valid_cases {
        let did = Did::new(s).expect("valid did:web");
        assert_eq!(did.as_str(), s);
        assert_eq!(did.org_domain(), expected_domain);
        assert_eq!(did.to_string(), s);
        assert_eq!(s.parse::<Did>().expect("FromStr parse succeeds"), did);
    }

    let invalid_cases = [
        "did:key:z6MkExample",
        "did:web:",
        "did:web:banskabystrica",
        "did:web:BANSKABYSTRICA.SK",
        "did:web:banskabystrica.sk/path",
        "did:web:banskabystrica.sk:8080",
        "did:web:.banskabystrica.sk",
        "did:web:banskabystrica..sk",
        "",
        "urn:did:web:banskabystrica.sk",
    ];

    for bad in invalid_cases {
        let res = Did::new(bad);
        assert!(
            res.is_err(),
            "expected `{bad}` to be rejected as an invalid did:web"
        );
    }
}

#[test]
fn data_offer_personal_data_purpose_validation_ds14() {
    let mut offer = DataOffer::from_yaml(GOLDEN_OFFER).expect("valid golden YAML");

    // containsPersonalData: true without purpose -> fails DS-14
    offer.spec.contains_personal_data = true;
    offer.spec.purpose = None;
    let err_missing = offer
        .validate()
        .expect_err("containsPersonalData without purpose must fail");
    assert!(matches!(
        err_missing,
        Error::Name {
            field: "spec.purpose",
            ..
        }
    ));

    // containsPersonalData: true with non-DPV purpose IRI -> fails DS-14
    offer.spec.purpose = Some("https://example.com/purposes/analytics".to_string());
    let err_non_dpv = offer.validate().expect_err("non-DPV purpose IRI must fail");
    assert!(matches!(
        err_non_dpv,
        Error::Name {
            field: "spec.purpose",
            ..
        }
    ));

    // containsPersonalData: true with valid DPV purpose IRI -> succeeds DS-14
    offer.spec.purpose = Some("https://w3id.org/dpv#ResearchAndDevelopment".to_string());
    assert!(offer.validate().is_ok());

    // containsPersonalData: false with valid DPV purpose IRI -> succeeds
    offer.spec.contains_personal_data = false;
    assert!(offer.validate().is_ok());

    // containsPersonalData: false with non-DPV purpose IRI -> fails
    offer.spec.purpose = Some("https://example.com/purposes/public".to_string());
    assert!(offer.validate().is_err());
}

#[test]
fn data_agreement_consumer_token_secret_ref_and_secret_field_rejection_ds15() {
    let mut agr = DataAgreement::from_yaml(GOLDEN_AGREEMENT).expect("valid golden YAML");

    agr.spec.role = AgreementRole::Consumer;
    agr.spec.offer_ref = None;
    agr.spec.state = AgreementState::Finalized;
    agr.spec.token_secret_ref = None;

    // Consumer in finalized state without tokenSecretRef -> fails DS-15
    let err_no_token = agr
        .validate()
        .expect_err("finalized consumer agreement requires tokenSecretRef");
    assert!(matches!(
        err_no_token,
        Error::Name {
            field: "spec.tokenSecretRef",
            ..
        }
    ));

    // Consumer in requested state without tokenSecretRef -> allowed
    agr.spec.state = AgreementState::Requested;
    assert!(agr.validate().is_ok());

    // Consumer in finalized state with valid tokenSecretRef -> allowed
    agr.spec.state = AgreementState::Finalized;
    agr.spec.token_secret_ref = Some(SecretRef {
        name: "dsp-bratislava-token".to_string(),
        key: None,
        env_var: None,
    });
    assert!(agr.validate().is_ok());

    // Manifest carrying inline plain token instead of tokenSecretRef -> rejected at parse time
    let inline_secret_yaml = r#"apiVersion: joinedcontext.com/v1alpha1
kind: DataAgreement
metadata:
  name: inline-token-attempt
  namespace: bb-ovzdusie
spec:
  role: consumer
  remoteParticipant: "did:web:bratislava.sk"
  agreementId: "dsp-agr-inline"
  state: finalized
  validity:
    from: "2026-09-01T00:00:00Z"
    to: "2027-09-01T00:00:00Z"
  token: "secret-bearer-token-in-manifest"
"#;
    let res = DataAgreement::from_yaml(inline_secret_yaml);
    assert!(
        res.is_err(),
        "deny_unknown_fields must reject inline `token` field (DS-15)"
    );
}

#[test]
fn data_agreement_state_machine_transitions() {
    // Forward progression pipeline: Requested -> Offered -> Accepted -> Finalized
    assert!(AgreementState::Requested.allows_transition_to(AgreementState::Offered));
    assert!(AgreementState::Offered.allows_transition_to(AgreementState::Accepted));
    assert!(AgreementState::Accepted.allows_transition_to(AgreementState::Finalized));

    // Self transitions
    assert!(AgreementState::Requested.allows_transition_to(AgreementState::Requested));
    assert!(AgreementState::Offered.allows_transition_to(AgreementState::Offered));
    assert!(AgreementState::Accepted.allows_transition_to(AgreementState::Accepted));
    assert!(AgreementState::Finalized.allows_transition_to(AgreementState::Finalized));
    assert!(AgreementState::Terminated.allows_transition_to(AgreementState::Terminated));

    // Any state may transition to Terminated
    assert!(AgreementState::Requested.allows_transition_to(AgreementState::Terminated));
    assert!(AgreementState::Offered.allows_transition_to(AgreementState::Terminated));
    assert!(AgreementState::Accepted.allows_transition_to(AgreementState::Terminated));
    assert!(AgreementState::Finalized.allows_transition_to(AgreementState::Terminated));

    // Terminated is terminal
    assert!(!AgreementState::Terminated.allows_transition_to(AgreementState::Requested));
    assert!(!AgreementState::Terminated.allows_transition_to(AgreementState::Offered));
    assert!(!AgreementState::Terminated.allows_transition_to(AgreementState::Accepted));
    assert!(!AgreementState::Terminated.allows_transition_to(AgreementState::Finalized));

    // Backward or skip transitions
    assert!(!AgreementState::Finalized.allows_transition_to(AgreementState::Accepted));
    assert!(!AgreementState::Finalized.allows_transition_to(AgreementState::Offered));
    assert!(!AgreementState::Finalized.allows_transition_to(AgreementState::Requested));
    assert!(!AgreementState::Accepted.allows_transition_to(AgreementState::Offered));
    assert!(!AgreementState::Accepted.allows_transition_to(AgreementState::Requested));
    assert!(!AgreementState::Offered.allows_transition_to(AgreementState::Requested));
    assert!(!AgreementState::Offered.allows_transition_to(AgreementState::Finalized));
    assert!(!AgreementState::Requested.allows_transition_to(AgreementState::Finalized));
}

#[test]
fn data_agreement_is_active_boundaries() {
    let agr = DataAgreement::from_yaml(GOLDEN_AGREEMENT).expect("valid golden YAML");

    let t_before: DateTime<Utc> = "2026-08-31T23:59:59Z".parse().unwrap();
    let t_start: DateTime<Utc> = "2026-09-01T00:00:00Z".parse().unwrap();
    let t_mid: DateTime<Utc> = "2027-01-15T12:00:00Z".parse().unwrap();
    let t_end: DateTime<Utc> = "2027-09-01T00:00:00Z".parse().unwrap();
    let t_after: DateTime<Utc> = "2027-09-01T00:00:01Z".parse().unwrap();

    assert!(!agr.spec.is_active(t_before));
    assert!(agr.spec.is_active(t_start));
    assert!(agr.spec.is_active(t_mid));
    assert!(agr.spec.is_active(t_end));
    assert!(!agr.spec.is_active(t_after));

    // Non-finalized state is never active
    let mut not_finalized = agr.clone();
    not_finalized.spec.state = AgreementState::Accepted;
    assert!(!not_finalized.spec.is_active(t_mid));

    not_finalized.spec.state = AgreementState::Terminated;
    assert!(!not_finalized.spec.is_active(t_mid));

    // Unbounded validity windows
    let mut unbounded = agr;
    unbounded.spec.validity = Validity {
        from: None,
        to: None,
    };
    assert!(unbounded.spec.is_active(t_before));
    assert!(unbounded.spec.is_active(t_after));
}

#[test]
fn data_offer_validation_failures() {
    let mut offer = DataOffer::from_yaml(GOLDEN_OFFER).expect("valid golden YAML");

    // Empty endpointRefs
    offer.spec.endpoint_refs.clear();
    assert!(offer.validate().is_err());

    // Duplicate endpointRefs
    offer.spec.endpoint_refs = vec![Ref::Name("ep1".to_string()), Ref::Name("ep1".to_string())];
    assert!(offer.validate().is_err());

    // Non-Endpoint kind in endpointRef
    offer.spec.endpoint_refs = vec![Ref::Typed(jc_core::envelope::TypedRef {
        kind: "ContextSpace".to_string(),
        name: "ep1".to_string(),
        namespace: None,
    })];
    assert!(offer.validate().is_err());

    // Policy without permission array
    offer.spec.endpoint_refs = vec![Ref::Name("ep1".to_string())];
    offer.spec.policy = json!({ "@type": "Offer" });
    assert!(offer.validate().is_err());

    // Policy with empty permission array
    offer.spec.policy = json!({ "@type": "Offer", "permission": [] });
    assert!(offer.validate().is_err());

    // Policy as primitive non-object
    offer.spec.policy = json!("invalid-policy");
    assert!(offer.validate().is_err());
}

#[test]
fn data_agreement_provider_requires_offer_ref() {
    let mut agr = DataAgreement::from_yaml(GOLDEN_AGREEMENT).expect("valid golden YAML");

    agr.spec.role = AgreementRole::Provider;
    agr.spec.offer_ref = None;
    let err = agr
        .validate()
        .expect_err("provider agreement without offerRef must fail");
    assert!(matches!(
        err,
        Error::Name {
            field: "spec.offerRef",
            ..
        }
    ));

    // Empty agreementId fails
    agr.spec.offer_ref = Some(Ref::Name("air-quality-public-offer".to_string()));
    agr.spec.agreement_id = "   ".to_string();
    assert!(agr.validate().is_err());
}

#[test]
fn participant_url_and_anchor_validation() {
    let mut p = DataSpaceParticipant::from_yaml(GOLDEN_PARTICIPANT).expect("valid golden YAML");

    // Non-HTTPS connectorUrl
    p.spec.connector_url = "http://connector.banskabystrica.sk".to_string();
    assert!(p.validate().is_err());

    // Non-HTTPS credentialIssuer
    p.spec.connector_url = "https://connector.banskabystrica.sk".to_string();
    p.spec.credential_issuer = "http://auth.banskabystrica.sk".to_string();
    assert!(p.validate().is_err());

    // Duplicate trust anchors
    p.spec.credential_issuer = "https://auth.banskabystrica.sk".to_string();
    p.spec.trust_anchors = vec![
        Did::new("did:web:smartcity-trust.sk").unwrap(),
        Did::new("did:web:smartcity-trust.sk").unwrap(),
    ];
    assert!(p.validate().is_err());
}
