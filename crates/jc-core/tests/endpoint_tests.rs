use jc_core::kinds::endpoint::{Audience, EndpointSlug, RateLimits, Representation};
use jc_core::kinds::{Endpoint, SharedSpaceReference};
use jc_core::Urn;

const GOLDEN: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: Endpoint
metadata:
  name: air-quality-public
  namespace: bb-ovzdusie
spec:
  contextSpaceRef: ovzdusie
  slug: zt4qm7ge2xdv6ksb3ncf5arw2y
  audience: public
  allowedProjects: []
  policyRef: urn:ngsi-ld:Policy:banskabystrica.sk:ovzdusie:public-air-quality
  enabledRepresentations:
    - ngsi-ld
    - mcp
    - geojson
    - csv
    - ogc-features
    - sta
  rateLimits:
    requestsPerMinute: 600
    burst: 50
  caching:
    maxAgeSeconds: 60
"#;

const GOLDEN_SHARED: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: SharedSpaceReference
metadata:
  name: external-air-quality
  namespace: bb-doprava
spec:
  endpointSlug: zt4qm7ge2xdv6ksb3ncf5arw2y
  alias: regional-air-data
"#;

#[test]
fn golden_endpoint_parses_validates_and_roundtrips() {
    let ep = Endpoint::from_yaml(GOLDEN).expect("valid golden YAML");
    ep.validate().expect("golden endpoint validates");

    assert_eq!(
        ep.resource_path().expect("resource path"),
        "projects/bb-ovzdusie/spaces/ovzdusie/endpoints/air-quality-public.yaml"
    );

    let serialized = ep.to_yaml().expect("serialize to yaml");
    let reimported = Endpoint::from_yaml(&serialized).expect("re-import yaml");
    assert_eq!(ep, reimported);
}

#[test]
fn representation_variants_serialize_to_expected_wire_names() {
    let representations = [
        (Representation::NgsiLd, "ngsi-ld"),
        (Representation::Mcp, "mcp"),
        (Representation::GeoJson, "geojson"),
        (Representation::Csv, "csv"),
        (Representation::Xlsx, "xlsx"),
        (Representation::Json, "json"),
        (Representation::Zip, "zip"),
        (Representation::OgcFeatures, "ogc-features"),
        (Representation::Sta, "sta"),
    ];

    let expected_names: Vec<&str> = representations.iter().map(|(_, name)| *name).collect();
    assert_eq!(
        expected_names,
        vec![
            "ngsi-ld",
            "mcp",
            "geojson",
            "csv",
            "xlsx",
            "json",
            "zip",
            "ogc-features",
            "sta"
        ]
    );

    for (variant, expected) in representations {
        assert_eq!(variant.as_str(), expected);
        let json = serde_json::to_string(&variant).expect("serialize representation");
        assert_eq!(json, format!("\"{expected}\""));
        let deserialized: Representation =
            serde_json::from_str(&json).expect("deserialize representation");
        assert_eq!(deserialized, variant);
    }
}

#[test]
fn endpoint_slug_validation_ep02() {
    // 25 characters (carry only 125 bits entropy) -> rejected
    let too_short = "zt4qm7ge2xdv6ksb3ncf5arw2";
    assert_eq!(too_short.len(), 25);
    assert!(serde_json::from_str::<EndpointSlug>(&format!("\"{too_short}\"")).is_err());
    assert!(EndpointSlug::new(too_short).is_err());

    // Characters outside RFC 4648 base32 (1, 8, 9, uppercase) -> rejected
    assert!(EndpointSlug::new("zt4qm7ge2xdv6ksb3ncf5arw21").is_err());
    assert!(EndpointSlug::new("zt4qm7ge2xdv6ksb3ncf5arw28").is_err());
    assert!(EndpointSlug::new("zt4qm7ge2xdv6ksb3ncf5arw29").is_err());
    assert!(EndpointSlug::new("zt4qm7ge2xdv6ksb3ncf5arw2A").is_err());
    assert!(EndpointSlug::new("").is_err());

    // 26 characters (carry 130 bits entropy >= 128) -> accepted
    let valid_26 = "zt4qm7ge2xdv6ksb3ncf5arw2y";
    assert_eq!(valid_26.len(), 26);
    let slug26 = EndpointSlug::new(valid_26).expect("26 chars accepted");
    assert_eq!(slug26.as_str(), valid_26);

    // 52 characters -> accepted
    let valid_52 = "zt4qm7ge2xdv6ksb3ncf5arw2yzt4qm7ge2xdv6ksb3ncf5arw2y";
    assert_eq!(valid_52.len(), 52);
    assert!(EndpointSlug::new(valid_52).is_ok());
}

#[test]
fn endpoint_slug_opacity_validation_ep03() {
    let ep = Endpoint::from_yaml(GOLDEN).expect("valid golden YAML");
    assert!(ep
        .spec
        .validate_opacity("bb-ovzdusie", "banskabystrica.sk")
        .is_ok());

    // Slug containing space name "ovzdusie" fails opacity check
    let leaky_slug = EndpointSlug::new("ovzdusie234567abcdefghijkl").expect("valid base32");
    let mut leaky_spec = ep.spec.clone();
    leaky_spec.slug = leaky_slug;
    assert!(leaky_spec
        .validate_opacity("bb-ovzdusie", "banskabystrica.sk")
        .is_err());
}

#[test]
fn audience_scoping_rules_ep14_ep15() {
    let ep = Endpoint::from_yaml(GOLDEN).expect("valid golden YAML");

    // audience: project-list with empty allowedProjects fails
    let mut bad_pl = ep.spec.clone();
    bad_pl.audience = Audience::ProjectList;
    bad_pl.allowed_projects = vec![];
    assert!(bad_pl.validate().is_err());

    // audience: project-list with valid projects passes
    bad_pl.allowed_projects = vec!["bb-doprava".to_string()];
    assert!(bad_pl.validate().is_ok());

    // audience: public with non-empty allowedProjects fails
    let mut bad_pub = ep.spec.clone();
    bad_pub.audience = Audience::Public;
    bad_pub.allowed_projects = vec!["bb-doprava".to_string()];
    assert!(bad_pub.validate().is_err());

    // audience: organization with empty list passes
    let mut org_ep = ep.spec.clone();
    org_ep.audience = Audience::Organization;
    org_ep.allowed_projects = vec![];
    assert!(org_ep.validate().is_ok());
}

#[test]
fn endpoint_spec_invariants_and_rate_limits() {
    let ep = Endpoint::from_yaml(GOLDEN).expect("valid golden YAML");

    // Empty representations fails
    let mut empty_rep = ep.spec.clone();
    empty_rep.enabled_representations = vec![];
    assert!(empty_rep.validate().is_err());

    // Duplicate representations fails
    let mut dup_rep = ep.spec.clone();
    dup_rep.enabled_representations = vec![Representation::NgsiLd, Representation::NgsiLd];
    assert!(dup_rep.validate().is_err());

    // requestsPerMinute: 0 fails
    let mut bad_limits = ep.spec.clone();
    bad_limits.rate_limits = Some(RateLimits {
        requests_per_minute: 0,
        burst: None,
    });
    assert!(bad_limits.validate().is_err());

    // policyRef whose type is not Policy fails
    let mut bad_policy_type = ep.spec.clone();
    bad_policy_type.policy_ref = Some(
        "urn:ngsi-ld:AirQualityObserved:banskabystrica.sk:ovzdusie:station-01"
            .parse::<Urn>()
            .expect("valid URN"),
    );
    assert!(bad_policy_type.validate().is_err());
}

#[test]
fn golden_shared_space_reference_parses_validates_and_paths() {
    let shared = SharedSpaceReference::from_yaml(GOLDEN_SHARED).expect("valid golden YAML");
    shared.validate().expect("shared space validates");

    assert_eq!(
        shared.resource_path().expect("resource path"),
        "projects/bb-doprava/shared/external-air-quality.yaml"
    );
}

/// EP-44: a declared ceiling has to leave something to download, so a zero is refused
/// where the manifest is written rather than at the request that returns nothing.
#[test]
fn a_zero_file_limit_is_refused() {
    let with_limits = |body: &str| {
        GOLDEN.replace(
            "  caching:\n    maxAgeSeconds: 60\n",
            &format!("  fileLimits:\n{body}  caching:\n    maxAgeSeconds: 60\n"),
        )
    };

    let good = Endpoint::from_yaml(&with_limits("    maxFileRows: 50000\n")).expect("valid YAML");
    good.validate().expect("a positive ceiling validates");
    assert_eq!(
        good.spec
            .file_limits
            .as_ref()
            .and_then(|limits| limits.max_file_rows),
        Some(50_000)
    );

    for body in ["    maxFileRows: 0\n", "    maxFileBytes: 0\n"] {
        let endpoint = Endpoint::from_yaml(&with_limits(body)).expect("valid YAML");
        assert!(
            endpoint.validate().is_err(),
            "a zero ceiling returns nothing at all: {body}"
        );
    }
}

#[test]
fn a_hidden_attribute_list_must_name_real_attributes_once() {
    let with_projection = |body: &str| {
        GOLDEN.replace(
            "  caching:\n    maxAgeSeconds: 60\n",
            &format!("  caching:\n    maxAgeSeconds: 60\n  projection:\n{body}"),
        )
    };

    let good = Endpoint::from_yaml(&with_projection(
        "    hiddenAttributes:\n      - calibrationOffset\n      - deviceSerial\n",
    ))
    .expect("valid YAML");
    good.validate().expect("two distinct names validate");
    assert_eq!(
        good.spec
            .projection
            .as_ref()
            .map(|projection| projection.hidden_attributes.len()),
        Some(2)
    );

    // An empty or repeated entry is refused: a steward who writes one means to hide
    // something, and a list that quietly drops an entry hides nothing (EP-61).
    for body in [
        "    hiddenAttributes:\n      - \"\"\n",
        "    hiddenAttributes:\n      - pm10\n      - pm10\n",
    ] {
        let endpoint = Endpoint::from_yaml(&with_projection(body)).expect("valid YAML");
        assert!(
            endpoint.validate().is_err(),
            "an unusable hidden attribute must be refused: {body}"
        );
    }
}
