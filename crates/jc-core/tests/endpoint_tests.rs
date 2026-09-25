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

/// EP-61, T-2334: the gateway serves the identity members and the broker-set timestamps whatever
/// the projection says, so a list naming one of them would be a narrowing the steward believes in
/// and the gateway never applies. It is refused when the manifest is validated, with the member in
/// the message, rather than accepted and ignored.
#[test]
fn a_hidden_attribute_the_gateway_always_serves_is_refused() {
    let with_projection = |name: &str| {
        GOLDEN.replace(
            "  caching:\n    maxAgeSeconds: 60\n",
            &format!(
                "  caching:\n    maxAgeSeconds: 60\n  projection:\n    hiddenAttributes:\n      - \"{name}\"\n"
            ),
        )
    };

    for name in [
        "id",
        "type",
        "@context",
        "@id",
        "@type",
        "scope",
        "createdAt",
        "modifiedAt",
        "deletedAt",
        "expiresAt",
    ] {
        let endpoint = Endpoint::from_yaml(&with_projection(name)).expect("valid YAML");
        let error = endpoint
            .validate()
            .expect_err(&format!("`{name}` is served whatever the projection says"));
        let message = error.to_string();
        assert!(
            message.contains(name),
            "the steward has to be told which entry is the problem: {message}"
        );
    }

    // An attribute that only resembles one of them is an ordinary attribute and may be hidden.
    for name in [
        "identifier",
        "typeOfMeasurement",
        "scopeNote",
        "created_at",
        "CreatedAt",
    ] {
        let endpoint = Endpoint::from_yaml(&with_projection(name)).expect("valid YAML");
        endpoint
            .validate()
            .unwrap_or_else(|error| panic!("`{name}` is an ordinary attribute: {error}"));
    }
}

/// AP-96, AP-97: an Endpoint's caller role and named roles parse, validate, round-trip, and
/// name the roles the reconciler's Policies use; absent, they are not written at all.
#[test]
fn an_endpoint_with_a_caller_role_and_named_roles_parses_validates_and_names_them() {
    let text = GOLDEN.replace(
        "  audience: public\n",
        "  audience: public\n  callerRole: true\n  roles:\n    - name: editor\n      subjects: [{ user: jana@hel.fi }, { group: alert-editors }]\n",
    );
    let endpoint = Endpoint::from_yaml(&text).expect("the roles parse");
    endpoint.validate().expect("the roles validate");
    assert!(endpoint.spec.caller_role);
    assert_eq!(
        endpoint.spec.roles[0].subjects[1].group.as_deref(),
        Some("alert-editors")
    );
    assert_eq!(
        endpoint,
        Endpoint::from_yaml(&endpoint.to_yaml().expect("serialize")).expect("re-import")
    );

    let plain = Endpoint::from_yaml(GOLDEN).expect("golden");
    assert!(!plain.spec.caller_role && plain.spec.roles.is_empty());
    let written = plain.to_yaml().expect("serialize");
    assert!(
        !written.contains("callerRole") && !written.contains("roles:"),
        "{written}"
    );

    assert_eq!(
        jc_core::kinds::endpoint_role("helsinki", "app-alerts", None),
        "endpoint:helsinki/app-alerts"
    );
    assert_eq!(
        jc_core::kinds::endpoint_role("helsinki", "app-alerts", Some("editor")),
        "endpoint:helsinki/app-alerts/editor"
    );
    assert!(jc_core::kinds::endpoint_role("a", "b", None)
        .starts_with(jc_core::kinds::ENDPOINT_ROLE_PREFIX));
}

/// AP-96: an Endpoint role follows the App role rules: a slug, declared once, and members that
/// are one lower-case address or one group each.
#[test]
fn an_endpoint_role_with_a_bad_name_a_twin_or_a_bad_member_is_refused() {
    for roles in [
        "    - name: Editor\n      subjects: [{ user: jana@hel.fi }]\n",
        "    - name: editor\n      subjects: [{ user: jana@hel.fi }]\n    - name: editor\n      subjects: [{ group: x }]\n",
        "    - name: editor\n      subjects: []\n",
        "    - name: editor\n      subjects: [{ user: JANA@hel.fi }]\n",
        "    - name: editor\n      subjects: [{ user: jana@hel.fi, group: x }]\n",
    ] {
        let text = GOLDEN.replace("  audience: public\n", &format!("  audience: public\n  roles:\n{roles}"));
        let endpoint = Endpoint::from_yaml(&text).expect("the shape parses");
        assert!(endpoint.validate().is_err(), "{roles}");
    }
}

/// The golden Endpoint with a `spec.catalog` block appended (EP-78).
fn with_catalog(catalog: &str) -> String {
    format!("{GOLDEN}  catalog:\n{catalog}")
}

#[test]
fn a_catalog_block_parses_validates_and_roundtrips() {
    let yaml = with_catalog(
        "    publisher: { name: { sk: Mesto Banská Bystrica, en: City of Banská Bystrica } }\n\
         \x20   contactPoint: { name: Otvorené dáta, email: opendata@example.org }\n\
         \x20   license: CC_BY_4_0\n\
         \x20   themes: [ENVI]\n\
         \x20   spatial: [SK032]\n\
         \x20   frequency: HOURLY\n",
    );
    let ep = Endpoint::from_yaml(&yaml).expect("valid catalog");
    ep.validate().expect("catalog validates");
    let catalog = ep.spec.catalog.as_ref().expect("catalog kept");
    assert_eq!(catalog.license.map(|l| l.code()), Some("CC_BY_4_0"));
    let reimported = Endpoint::from_yaml(&ep.to_yaml().expect("yaml")).expect("re-import");
    assert_eq!(ep, reimported);
}

#[test]
fn a_catalog_with_a_bad_member_fails_the_endpoint_with_the_field_named() {
    let yaml = with_catalog("    contactPoint: { name: Desk, email: not-an-address }\n");
    let ep = Endpoint::from_yaml(&yaml).expect("parses");
    let message = ep.validate().expect_err("refused").to_string();
    assert!(
        message.contains("spec.catalog.contactPoint.email"),
        "{message}"
    );
}

#[test]
fn a_catalog_licence_outside_the_table_does_not_parse() {
    assert!(Endpoint::from_yaml(&with_catalog("    license: proprietary\n")).is_err());
}
